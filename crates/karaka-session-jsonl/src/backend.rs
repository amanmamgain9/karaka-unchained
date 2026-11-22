//! [`JsonlPersistence`], the backend: finding, reading and writing session
//! files, the in-process bookkeeping of writers and created sessions, and
//! the handle. (dsh: `JsonlSessionPersistence` in
//! `session-persistence-jsonl/src/index.ts`, and `JsonlSessionHandle` and
//! `JsonlBackendTracker` in `storage.ts`.)
//!
//! # One writer
//!
//! In this process, the tracker lets one write handle claim an id. Across
//! processes, the write handle holds the session's lock (see `lease.rs`),
//! taken when it opens an existing session, or, for a created one, just
//! before its first write. A created session nobody wrote has no lock and
//! no files.
//!
//! # Generations
//!
//! A session directory may hold several generations of its log; the
//! highest wins. Only the current one, v4, is read. Older generations need
//! a migration that isn't ported yet, and newer ones a newer harness; both
//! are refused with dsh's wording and the raw log's path, and neither is
//! ever written.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use karaka_session::{Entry, SESSION_FORMAT_VERSION, SessionHeader, SessionId};
use karaka_session_persistence::{
    CreateOptions, FormatStatus, PersistenceError, PersistenceFuture, SessionAccess, SessionHandle,
    SessionLocation, SessionPersistence, SessionSnapshot, assert_contiguous, assert_create_header,
    assert_stored_id, format_version_refusal, validate_stored_events,
};
use serde_json::Value;

use crate::file;
use crate::format::{self, ScanError};
use crate::layout::{self, Encoding};
use crate::lease::Lease;

/// The backend's name in its messages. (dsh: `session-persistence-jsonl`.)
const NAME: &str = "session-persistence-jsonl";

/// The backend's one setting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JsonlConfig {
    /// Where every session file goes. Required, with no default: a default
    /// of the working directory would scatter sessions as it changes. A
    /// relative root is resolved once, when the backend starts. An existing
    /// root must be a readable directory; a missing one is made at the
    /// first write.
    pub root: PathBuf,
}

/// Sessions stored as JSONL files under a root directory, one directory per
/// session. (dsh: the `session-persistence-jsonl` plugin with
/// `compression: 'none'`.)
#[derive(Clone)]
pub struct JsonlPersistence {
    backend: Arc<Backend>,
}

impl JsonlPersistence {
    /// A backend over `config.root`.
    ///
    /// # Errors
    ///
    /// When the root exists but can't be read as a directory.
    pub fn new(config: JsonlConfig) -> Result<Self, PersistenceError> {
        let root = std::path::absolute(&config.root)
            .map_err(|error| PersistenceError::io("resolve the session root", error))?;
        match fs::read_dir(&root) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(PersistenceError::io(
                    format!("session root {}", root.display()),
                    error,
                ));
            }
        }
        Ok(Self {
            backend: Arc::new(Backend {
                root,
                tracker: Mutex::new(Tracker::default()),
                root_checked: AtomicBool::new(false),
                #[cfg(test)]
                fail_next_sync: AtomicBool::new(false),
            }),
        })
    }

    /// The resolved root.
    pub fn root(&self) -> &Path {
        &self.backend.root
    }
}

impl std::fmt::Debug for JsonlPersistence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JsonlPersistence")
            .field("root", &self.backend.root)
            .finish_non_exhaustive()
    }
}

/// Runs blocking file work off the async executor.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, PersistenceError> + Send + 'static,
) -> Result<T, PersistenceError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| PersistenceError::Other(format!("{NAME}: storage task failed: {error}")))?
}

fn not_found(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound
}

fn io_error(context: impl std::fmt::Display, error: io::Error) -> PersistenceError {
    PersistenceError::io(context.to_string(), error)
}

fn layout_error(message: String) -> PersistenceError {
    PersistenceError::Other(message)
}

fn location(path: &Path) -> SessionLocation {
    SessionLocation {
        kind: "jsonl",
        path: path.to_owned(),
    }
}

/// The selected generation of one session directory.
#[derive(Clone, Debug)]
struct Generation {
    path: PathBuf,
    version: u64,
}

/// A stored log, read and checked.
struct Stored {
    header: SessionHeader,
    inherited_event_count: u64,
    entries: Vec<Entry>,
    /// Where to cut a torn tail before the next append.
    torn_truncate_to: Option<u64>,
}

/// A session created here and not yet written.
#[derive(Clone)]
struct Pending {
    header: SessionHeader,
    revision: String,
    inherited_event_count: u64,
}

/// Who writes what, in this process. (dsh: `JsonlBackendTracker`.)
#[derive(Default)]
struct Tracker {
    /// The write claim of each id; `None` while its handle is being made.
    writers: HashMap<SessionId, Option<Weak<Core>>>,
    pending: HashMap<SessionId, Pending>,
    counter: u64,
}

struct Backend {
    root: PathBuf,
    tracker: Mutex<Tracker>,
    /// Whether the root was checked for the other encoding.
    root_checked: AtomicBool,
    /// Makes the next append's sync fail after its bytes are written.
    #[cfg(test)]
    fail_next_sync: AtomicBool,
}

impl Backend {
    fn tracker(&self) -> MutexGuard<'_, Tracker> {
        self.tracker.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn encoding_mismatch(path: &Path) -> PersistenceError {
        let quoted = Value::from(path.display().to_string()).to_string();
        layout_error(format!(
            "session artifact {quoted} uses .jsonl.zstd, but this backend is configured for \
             compression \"none\"; use a separate root or select the matching compression mode"
        ))
    }

    fn legacy_layout(path: &Path) -> PersistenceError {
        let quoted = Value::from(path.display().to_string()).to_string();
        layout_error(format!(
            "session artifact {quoted} uses the unsupported flat-file layout; use a separate \
             root or move it into a project/session directory before loading"
        ))
    }

    // --- discovery (blocking) ---

    /// The project directories under the root; none when it doesn't exist.
    fn project_dirs(&self) -> Result<Vec<PathBuf>, PersistenceError> {
        let entries = match fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(error) if not_found(&error) => return Ok(Vec::new()),
            Err(error) => return Err(io_error(format!("read {}", self.root.display()), error)),
        };
        let mut dirs = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|error| io_error("read the session root", error))?;
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                dirs.push(entry.path());
            }
        }
        dirs.sort();
        Ok(dirs)
    }

    /// The session directories of a project, refusing the old flat layout.
    fn session_dirs(project: &Path) -> Result<Vec<PathBuf>, PersistenceError> {
        let entries = fs::read_dir(project)
            .map_err(|error| io_error(format!("read {}", project.display()), error))?;
        let mut dirs = Vec::new();
        for entry in entries {
            let entry =
                entry.map_err(|error| io_error(format!("read {}", project.display()), error))?;
            let kind = entry
                .file_type()
                .map_err(|error| io_error(format!("read {}", project.display()), error))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if kind.is_file() && (name.ends_with(".jsonl") || name.ends_with(".jsonl.zstd")) {
                return Err(Self::legacy_layout(&entry.path()));
            }
            if kind.is_dir() {
                dirs.push(entry.path());
            }
        }
        dirs.sort();
        Ok(dirs)
    }

    /// The highest canonical raw generation in `dir`, refusing a
    /// compressed one. (dsh: `resolveGenerationInDirectory`.)
    fn generation_in(dir: &Path) -> Result<Option<Generation>, PersistenceError> {
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) if not_found(&error) => return Ok(None),
            Err(error) => return Err(io_error(format!("read {}", dir.display()), error)),
        };
        let mut latest: Option<Generation> = None;
        let mut compressed: Vec<PathBuf> = Vec::new();
        for entry in entries {
            let entry =
                entry.map_err(|error| io_error(format!("read {}", dir.display()), error))?;
            let Some((version, encoding)) = entry
                .file_name()
                .to_str()
                .and_then(layout::parse_generation_filename)
            else {
                continue;
            };
            match encoding {
                Encoding::Zstd => compressed.push(entry.path()),
                Encoding::Raw => {
                    if latest
                        .as_ref()
                        .is_none_or(|latest| version > latest.version)
                    {
                        latest = Some(Generation {
                            path: entry.path(),
                            version,
                        });
                    }
                }
            }
        }
        compressed.sort();
        if let Some(path) = compressed.first() {
            return Err(Self::encoding_mismatch(path));
        }
        Ok(latest)
    }

    /// Refuses a root that already holds compressed logs, once. (dsh:
    /// `ensureRootEncoding`.)
    fn ensure_root_encoding(&self) -> Result<(), PersistenceError> {
        if self.root_checked.load(Ordering::Acquire) {
            return Ok(());
        }
        for project in self.project_dirs()? {
            for dir in Self::session_dirs(&project)? {
                Self::generation_in(&dir)?;
            }
        }
        self.root_checked.store(true, Ordering::Release);
        Ok(())
    }

    /// The selected generation of `id`, across every project directory.
    /// (dsh: `findLog`.)
    fn find_log(&self, id: &SessionId) -> Result<Option<Generation>, PersistenceError> {
        let encoded = layout::encode_segment(id.as_str()).map_err(layout_error)?;
        let mut matches = Vec::new();
        for project in self.project_dirs()? {
            for suffix in [".jsonl.zstd", ".jsonl"] {
                let flat = project.join(format!("{encoded}{suffix}"));
                if flat.exists() {
                    return Err(Self::legacy_layout(&flat));
                }
            }
            if let Some(generation) = Self::generation_in(&project.join(&encoded))? {
                matches.push(generation);
            }
        }
        if matches.len() > 1 {
            return Err(layout_error(format!(
                "duplicate JSONL session id \"{id}\" appears in multiple project directories"
            )));
        }
        Ok(matches.pop())
    }

    /// Refuses metadata that doesn't name the file it was read from. (dsh:
    /// `assertStoredIdentity`.)
    fn assert_stored_identity(
        &self,
        path: &Path,
        version: u64,
        header: &SessionHeader,
        expected: Option<&SessionId>,
    ) -> Result<(), PersistenceError> {
        let shown = path.display();
        if let Some(expected) = expected
            && expected != &header.id
        {
            return Err(layout_error(format!(
                "corrupt session log \"{shown}\": requested id \"{expected}\" does not match \
                 header id \"{}\"",
                header.id
            )));
        }
        let expected_path = layout::generation_path(
            &self.root,
            header.cwd.as_deref(),
            header.id.as_str(),
            version,
        )
        .map_err(|_| {
            layout_error(format!(
                "corrupt session log \"{shown}\": header id cannot name a storage path"
            ))
        })?;
        if path != expected_path && !same_file(path, &expected_path)? {
            return Err(layout_error(format!(
                "corrupt session log \"{shown}\": header id \"{}\" and cwd identify \"{}\"",
                header.id,
                expected_path.display()
            )));
        }
        Ok(())
    }

    /// Reads only the header of a selected generation. `None` when the file
    /// went away or has no readable header. (dsh: `readGenerationHeader`.)
    fn generation_header(
        &self,
        generation: &Generation,
        expected: Option<&SessionId>,
    ) -> Result<Option<SessionHeader>, PersistenceError> {
        let line = match file::read_first_line(&generation.path) {
            Ok(Some(line)) => line,
            Ok(None) => return Ok(None),
            Err(error) if not_found(&error) => return Ok(None),
            Err(error) => {
                return Err(io_error(
                    format!("read {}", generation.path.display()),
                    error,
                ));
            }
        };
        let Ok(value) = serde_json::from_slice::<Value>(&line) else {
            return Ok(None);
        };
        format::assert_no_retired_header_fields(&value).map_err(layout_error)?;
        if let Some(stored) = value.get("version").and_then(Value::as_u64)
            && stored != generation.version
        {
            return Err(layout_error(format!(
                "session generation filename identifies v{}, but its header identifies v{stored}",
                generation.version
            )));
        }
        if let Err(ScanError::Unsupported(reason)) = format::refuse_foreign_version(&value) {
            return Err(PersistenceError::unsupported(
                reason,
                Some(location(&generation.path)),
            ));
        }
        let Some(header) = format::header_of(&value) else {
            return Ok(None);
        };
        self.assert_stored_identity(&generation.path, generation.version, &header, expected)?;
        Ok(Some(header))
    }

    /// Reads and checks the current log at `path`. (dsh: `readStoredLog` and
    /// `decodeStoredLog`.)
    fn read_stored(&self, path: &Path, expected: &SessionId) -> Result<Stored, PersistenceError> {
        let (bytes, _) = file::read_stable(path)
            .map_err(|error| io_error(format!("read {}", path.display()), error))?;
        let scan = format::scan_log(&bytes).map_err(|error| match error {
            ScanError::Unsupported(reason) => {
                PersistenceError::unsupported(reason, Some(location(path)))
            }
            ScanError::Corrupt(reason) => PersistenceError::Corruption(format!(
                "session \"{expected}\": stored log is corrupt: {reason} (raw log: {})",
                path.display()
            )),
        })?;
        self.assert_stored_identity(path, SESSION_FORMAT_VERSION, &scan.header, Some(expected))?;
        assert_stored_id(expected, &scan.header)?;
        let entries = validate_stored_events(&scan.header, scan.rows, Some(&location(path)))?;
        let torn_truncate_to =
            (scan.committed_bytes < bytes.len() as u64).then_some(scan.committed_bytes);
        Ok(Stored {
            header: scan.header,
            inherited_event_count: scan.inherited_event_count,
            entries,
            torn_truncate_to,
        })
    }

    /// The refusal for a selected generation this build doesn't read.
    fn refuse_generation(&self, id: &SessionId, generation: &Generation) -> PersistenceError {
        if generation.version > SESSION_FORMAT_VERSION {
            // A readable newer header is refused inside, naming its own id.
            match self.generation_header(generation, Some(id)) {
                Err(error) => return error,
                Ok(None) => {
                    return PersistenceError::Corruption(format!(
                        "session \"{id}\": stored log has a malformed header (raw log: {})",
                        generation.path.display()
                    ));
                }
                Ok(Some(_)) => {}
            }
        }
        PersistenceError::unsupported(
            format_version_refusal(id.as_str(), generation.version),
            Some(location(&generation.path)),
        )
    }

    /// The stored log of `id`, refusing loudly when it's absent or in a
    /// generation this build doesn't read. (dsh: `requireStoredLog`.)
    fn require_stored(&self, id: &SessionId) -> Result<(Stored, Generation), PersistenceError> {
        let generation = self
            .find_log(id)?
            .ok_or_else(|| PersistenceError::NotFound(id.clone()))?;
        if generation.version != SESSION_FORMAT_VERSION {
            return Err(self.refuse_generation(id, &generation));
        }
        Ok((self.read_stored(&generation.path, id)?, generation))
    }

    /// The current log's path for a handle's read; `None` when there is
    /// none yet. (dsh: `resolveCurrentLog`.)
    fn current_log(&self, id: &SessionId) -> Result<Option<PathBuf>, PersistenceError> {
        let Some(generation) = self.find_log(id)? else {
            return Ok(None);
        };
        if generation.version != SESSION_FORMAT_VERSION {
            return Err(self.refuse_generation(id, &generation));
        }
        Ok(Some(generation.path))
    }

    /// Refuses a compressed log in a created session's directory, the last
    /// moment it can still be absent. (dsh: `rejectOppositeArtifact`.)
    fn acquire_lease(&self, header: &SessionHeader) -> Result<Lease, PersistenceError> {
        let dir = layout::session_dir(&self.root, header.cwd.as_deref(), header.id.as_str())
            .map_err(layout_error)?;
        Self::generation_in(&dir)?;
        Lease::acquire(&dir, &header.id)
    }

    /// The current log's path for `header`.
    fn log_path(&self, header: &SessionHeader) -> Result<PathBuf, PersistenceError> {
        layout::generation_path(
            &self.root,
            header.cwd.as_deref(),
            header.id.as_str(),
            SESSION_FORMAT_VERSION,
        )
        .map_err(layout_error)
    }

    /// Publishes a session's first bytes. (dsh: `materialize`.)
    fn materialize(&self, header: &SessionHeader, content: &[u8]) -> Result<(), PersistenceError> {
        let project =
            layout::project_dir(&self.root, header.cwd.as_deref()).map_err(layout_error)?;
        let dir = layout::session_dir(&self.root, header.cwd.as_deref(), header.id.as_str())
            .map_err(layout_error)?;
        Self::generation_in(&dir)?;
        file::materialize(
            &self.root,
            &project,
            &dir,
            &self.log_path(header)?,
            header.id.as_str(),
            content,
        )
    }

    /// Writes and syncs one batch.
    fn write_batch(&self, file: &mut fs::File, content: &[u8]) -> io::Result<()> {
        #[cfg(test)]
        if self.fail_next_sync.swap(false, Ordering::SeqCst) {
            use std::io::Write;
            file.write_all(content)?;
            return Err(io::Error::other("simulated fsync ENOSPC"));
        }
        file::write_and_sync(file, content)
    }

    // --- bookkeeping ---

    /// Gives up a handle's claim: a creator that never wrote leaves nothing
    /// behind. (dsh: `JsonlBackendTracker.release`.)
    fn release(&self, id: &SessionId, access: SessionAccess) {
        if access != SessionAccess::Write {
            return;
        }
        let mut tracker = self.tracker();
        tracker.writers.remove(id);
        tracker.pending.remove(id);
    }
}

/// Whether two spellings name one file, as a case-insensitive filesystem
/// allows. (dsh: `sameFile`.)
fn same_file(path: &Path, expected: &Path) -> Result<bool, PersistenceError> {
    match (fs::canonicalize(path), fs::canonicalize(expected)) {
        (Ok(actual), Ok(expected)) => Ok(actual == expected),
        (Err(error), _) | (_, Err(error)) if not_found(&error) => Ok(false),
        (Err(error), _) | (_, Err(error)) => Err(io_error("resolve a session log path", error)),
    }
}

/// What a handle knows of its log. (dsh: `StorageHandleState`.)
struct State {
    /// The next seq.
    cursor: u64,
    /// Whether the session has a file yet.
    materialized: bool,
    torn_truncate_to: Option<u64>,
    /// The log a write open read, served until the first append.
    primed: Option<Vec<Entry>>,
    /// The longest log a read has seen.
    observed: usize,
    lease: Option<Lease>,
}

/// The handle's shared part, which the tracker can reach to flush it.
struct Core {
    backend: Arc<Backend>,
    id: SessionId,
    header: SessionHeader,
    access: SessionAccess,
    inherited_event_count: u64,
    /// One operation at a time, in order. (dsh: the handle's mutation
    /// chain.)
    state: tokio::sync::Mutex<State>,
    closed: AtomicBool,
    released: AtomicBool,
}

impl Core {
    fn check_open(&self, operation: &'static str) -> Result<(), PersistenceError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(PersistenceError::HandleClosed {
                id: self.id.clone(),
                operation,
            });
        }
        Ok(())
    }

    fn check_write(&self, operation: &'static str) -> Result<(), PersistenceError> {
        if self.access != SessionAccess::Write {
            return Err(PersistenceError::ReadOnly {
                id: self.id.clone(),
                operation,
            });
        }
        Ok(())
    }

    /// Holds the session's lock before its first write. A write open holds
    /// it from the start; a created session takes it here, just before its
    /// first bytes, and keeps it even if the write then fails.
    async fn ensure_lease(&self, state: &mut State) -> Result<(), PersistenceError> {
        if state.lease.is_some() {
            return Ok(());
        }
        let backend = Arc::clone(&self.backend);
        let header = self.header.clone();
        state.lease = Some(blocking(move || backend.acquire_lease(&header)).await?);
        Ok(())
    }

    async fn read(
        &self,
        offset: usize,
        length: Option<usize>,
    ) -> Result<Vec<Entry>, PersistenceError> {
        self.check_open("read")?;
        let mut state = self.state.lock().await;
        let events = if let Some(primed) = &state.primed {
            primed.clone()
        } else if self.access == SessionAccess::Write && !state.materialized {
            Vec::new()
        } else {
            let backend = Arc::clone(&self.backend);
            let id = self.id.clone();
            let stored = blocking(move || match backend.current_log(&id)? {
                Some(path) => backend
                    .read_stored(&path, &id)
                    .map(|stored| Some(stored.entries)),
                None => Ok(None),
            })
            .await?;
            match stored {
                Some(entries) => entries,
                None if self.backend.tracker().pending.contains_key(&self.id) => Vec::new(),
                None => return Err(PersistenceError::NotFound(self.id.clone())),
            }
        };
        if events.len() < state.observed {
            return Err(PersistenceError::Other(format!(
                "session \"{}\": stored log shrank below a previously observed prefix ({} < {})",
                self.id,
                events.len(),
                state.observed
            )));
        }
        state.observed = events.len();
        let start = offset.min(events.len());
        let end = length.map_or(events.len(), |length| {
            start.saturating_add(length).min(events.len())
        });
        Ok(events[start..end].to_vec())
    }

    async fn append(&self, events: Vec<Entry>) -> Result<(), PersistenceError> {
        self.check_open("append")?;
        let mut state = self.state.lock().await;
        self.check_open("append")?;
        self.check_write("append")?;
        if events.is_empty() {
            return Ok(());
        }
        // The batch is checked and encoded before the lock, so a refused
        // one leaves no trace on disk.
        assert_contiguous(&self.id, &events, state.cursor)?;
        let mut content = Vec::new();
        for entry in &events {
            content.extend_from_slice(format::event_line(entry)?.as_bytes());
            content.push(b'\n');
        }
        self.ensure_lease(&mut state).await?;
        let path = self.backend.log_path(&self.header)?;
        // A torn tail is cut first, and the cut forgotten only once it
        // lands, so a failed one is retried at the next append.
        if let Some(offset) = state.torn_truncate_to {
            let torn = path.clone();
            blocking(move || {
                file::truncate(&torn, offset)
                    .map_err(|error| io_error(format!("truncate {}", torn.display()), error))
            })
            .await?;
            state.torn_truncate_to = None;
        }
        let backend = Arc::clone(&self.backend);
        if state.materialized {
            blocking(move || {
                file::append(
                    &path,
                    &content,
                    |file, content| backend.write_batch(file, content),
                    file::truncate,
                )
            })
            .await?;
        } else {
            let header = self.header.clone();
            let inherited = self.inherited_event_count;
            blocking(move || {
                let mut first = format::header_line(&header, inherited)?.into_bytes();
                first.push(b'\n');
                first.extend_from_slice(&content);
                backend.materialize(&header, &first)
            })
            .await?;
            self.backend.tracker().pending.remove(&self.id);
        }
        state.cursor += events.len() as u64;
        state.materialized = true;
        state.primed = None;
        state.observed = usize::try_from(state.cursor).unwrap_or(usize::MAX);
        Ok(())
    }

    async fn flush(&self) -> Result<(), PersistenceError> {
        self.check_open("flush")?;
        let mut state = self.state.lock().await;
        self.check_open("flush")?;
        self.check_write("flush")?;
        // Appends are durable when they resolve.
        if state.materialized {
            return Ok(());
        }
        self.ensure_lease(&mut state).await?;
        let backend = Arc::clone(&self.backend);
        let header = self.header.clone();
        let inherited = self.inherited_event_count;
        blocking(move || {
            let mut line = format::header_line(&header, inherited)?.into_bytes();
            line.push(b'\n');
            backend.materialize(&header, &line)
        })
        .await?;
        self.backend.tracker().pending.remove(&self.id);
        state.materialized = true;
        Ok(())
    }

    async fn close(&self) -> Result<(), PersistenceError> {
        self.closed.store(true, Ordering::Release);
        // Waits for the operations already running.
        let mut state = self.state.lock().await;
        state.lease = None;
        self.release_claim();
        Ok(())
    }

    fn release_claim(&self) {
        if !self.released.swap(true, Ordering::AcqRel) {
            self.backend.release(&self.id, self.access);
        }
    }
}

/// The handle [`JsonlPersistence`] hands out. (dsh: `JsonlSessionHandle`.)
struct JsonlHandle(Arc<Core>);

impl Drop for JsonlHandle {
    fn drop(&mut self) {
        // Closing is the way to learn whether giving the handle up worked;
        // a dropped handle still frees its claim. Its lock goes with it.
        self.0.closed.store(true, Ordering::Release);
        self.0.release_claim();
    }
}

impl SessionHandle for JsonlHandle {
    fn id(&self) -> &SessionId {
        &self.0.id
    }

    fn header(&self) -> &SessionHeader {
        &self.0.header
    }

    fn inherited_event_count(&self) -> u64 {
        self.0.inherited_event_count
    }

    fn access(&self) -> SessionAccess {
        self.0.access
    }

    fn read(&self, offset: usize, length: Option<usize>) -> PersistenceFuture<'_, Vec<Entry>> {
        Box::pin(self.0.read(offset, length))
    }

    fn append(&self, events: Vec<Entry>) -> PersistenceFuture<'_, ()> {
        Box::pin(self.0.append(events))
    }

    fn flush(&self) -> PersistenceFuture<'_, ()> {
        Box::pin(self.0.flush())
    }

    fn close(&self) -> PersistenceFuture<'_, ()> {
        Box::pin(self.0.close())
    }
}

impl JsonlPersistence {
    fn handle(&self, core: Core) -> Box<dyn SessionHandle> {
        let core = Arc::new(core);
        if core.access == SessionAccess::Write {
            self.backend
                .tracker()
                .writers
                .insert(core.id.clone(), Some(Arc::downgrade(&core)));
        }
        Box::new(JsonlHandle(core))
    }

    fn core(
        &self,
        header: SessionHeader,
        access: SessionAccess,
        inherited: u64,
        state: State,
    ) -> Core {
        Core {
            backend: Arc::clone(&self.backend),
            id: header.id.clone(),
            header,
            access,
            inherited_event_count: inherited,
            state: tokio::sync::Mutex::new(state),
            closed: AtomicBool::new(false),
            released: AtomicBool::new(false),
        }
    }

    async fn create_handle(
        &self,
        header: SessionHeader,
        options: CreateOptions,
    ) -> Result<Box<dyn SessionHandle>, PersistenceError> {
        assert_create_header(&header)?;
        // Fails fast on a header the first write couldn't store.
        format::header_line(&header, options.inherited_event_count)?;
        let backend = Arc::clone(&self.backend);
        let id = header.id.clone();
        let stored = blocking(move || {
            backend.ensure_root_encoding()?;
            backend.find_log(&id).map(|found| found.is_some())
        })
        .await?;
        {
            let mut tracker = self.backend.tracker();
            if stored
                || tracker.pending.contains_key(&header.id)
                || tracker.writers.contains_key(&header.id)
            {
                return Err(PersistenceError::AlreadyExists(header.id.clone()));
            }
            tracker.counter += 1;
            let revision = format!("memory:{NAME}:{}", tracker.counter);
            tracker.writers.insert(header.id.clone(), None);
            tracker.pending.insert(
                header.id.clone(),
                Pending {
                    header: header.clone(),
                    revision,
                    inherited_event_count: options.inherited_event_count,
                },
            );
        }
        // No lock yet: nothing is on disk for another process to contend
        // over until the first write.
        let state = State {
            cursor: 0,
            materialized: false,
            torn_truncate_to: None,
            primed: None,
            observed: 0,
            lease: None,
        };
        let core = self.core(
            header,
            SessionAccess::Write,
            options.inherited_event_count,
            state,
        );
        Ok(self.handle(core))
    }

    async fn open_handle(
        &self,
        id: &SessionId,
        access: SessionAccess,
    ) -> Result<Box<dyn SessionHandle>, PersistenceError> {
        let backend = Arc::clone(&self.backend);
        blocking(move || backend.ensure_root_encoding()).await?;
        if access == SessionAccess::Read {
            let pending = self.backend.tracker().pending.get(id).cloned();
            if let Some(pending) = pending {
                let state = State {
                    cursor: 0,
                    materialized: false,
                    torn_truncate_to: None,
                    primed: None,
                    observed: 0,
                    lease: None,
                };
                let core = self.core(pending.header, access, pending.inherited_event_count, state);
                return Ok(self.handle(core));
            }
            let backend = Arc::clone(&self.backend);
            let id = id.clone();
            let (stored, _) = blocking(move || backend.require_stored(&id)).await?;
            let state = State {
                cursor: 0,
                materialized: true,
                torn_truncate_to: None,
                primed: None,
                observed: 0,
                lease: None,
            };
            let core = self.core(stored.header, access, stored.inherited_event_count, state);
            return Ok(self.handle(core));
        }
        // A pending session always has an active creator, so its claim
        // refuses this as owned.
        {
            let mut tracker = self.backend.tracker();
            if tracker.writers.contains_key(id) {
                return Err(PersistenceError::AlreadyOwned(id.clone()));
            }
            tracker.writers.insert(id.clone(), None);
        }
        let backend = Arc::clone(&self.backend);
        let owned = id.clone();
        let opened = blocking(move || {
            let generation = backend
                .find_log(&owned)?
                .ok_or_else(|| PersistenceError::NotFound(owned.clone()))?;
            // Refused before the lock, so an older or newer generation's
            // directory is left exactly as it was.
            if generation.version != SESSION_FORMAT_VERSION {
                return Err(backend.refuse_generation(&owned, &generation));
            }
            let dir = generation
                .path
                .parent()
                .map(Path::to_owned)
                .unwrap_or_default();
            let lease = Lease::acquire(&dir, &owned)?;
            let stored = backend.read_stored(&generation.path, &owned)?;
            Ok((stored, lease))
        })
        .await;
        let (stored, lease) = match opened {
            Ok(opened) => opened,
            Err(error) => {
                self.backend.tracker().writers.remove(id);
                return Err(error);
            }
        };
        let state = State {
            cursor: stored.entries.len() as u64,
            materialized: true,
            torn_truncate_to: stored.torn_truncate_to,
            observed: stored.entries.len(),
            primed: Some(stored.entries),
            lease: Some(lease),
        };
        let core = self.core(stored.header, access, stored.inherited_event_count, state);
        Ok(self.handle(core))
    }

    async fn stat_session(
        &self,
        id: &SessionId,
    ) -> Result<Option<SessionSnapshot>, PersistenceError> {
        let pending = self.backend.tracker().pending.get(id).cloned();
        if let Some(pending) = pending {
            return Ok(Some(SessionSnapshot {
                header: pending.header,
                format_status: None,
                revision: pending.revision,
                size_bytes: None,
            }));
        }
        let backend = Arc::clone(&self.backend);
        let id = id.clone();
        blocking(move || {
            backend.ensure_root_encoding()?;
            let Some(generation) = backend.find_log(&id)? else {
                return Ok(None);
            };
            let Some(header) = backend.generation_header(&generation, Some(&id))? else {
                return Ok(None);
            };
            snapshot(header, &generation.path)
        })
        .await
    }

    async fn list_sessions(&self) -> Result<Vec<SessionSnapshot>, PersistenceError> {
        // Pending sessions are taken before the scan, so one whose first
        // write lands meanwhile is never missed.
        let pending: Vec<Pending> = self.backend.tracker().pending.values().cloned().collect();
        let backend = Arc::clone(&self.backend);
        let mut snapshots = blocking(move || {
            backend.ensure_root_encoding()?;
            let mut snapshots: Vec<SessionSnapshot> = Vec::new();
            for project in backend.project_dirs()? {
                for dir in Backend::session_dirs(&project)? {
                    let Some(generation) = Backend::generation_in(&dir)? else {
                        continue;
                    };
                    // An unreadable or unsupported header is skipped, not a
                    // failure of the whole listing.
                    let header = match backend.generation_header(&generation, None) {
                        Ok(Some(header)) => header,
                        Ok(None)
                        | Err(
                            PersistenceError::FormatUnsupported { .. }
                            | PersistenceError::Corruption(_),
                        ) => {
                            continue;
                        }
                        Err(error) => return Err(error),
                    };
                    if snapshots.iter().any(|listed| listed.header.id == header.id) {
                        return Err(layout_error(format!(
                            "duplicate JSONL session id \"{}\" appears in multiple project \
                             directories",
                            header.id
                        )));
                    }
                    if let Some(snapshot) = snapshot(header, &generation.path)? {
                        snapshots.push(snapshot);
                    }
                }
            }
            Ok(snapshots)
        })
        .await?;
        for pending in pending {
            if !snapshots
                .iter()
                .any(|listed| listed.header.id == pending.header.id)
            {
                snapshots.push(SessionSnapshot {
                    header: pending.header,
                    format_status: None,
                    revision: pending.revision,
                    size_bytes: None,
                });
            }
        }
        Ok(snapshots)
    }

    async fn flush_all(&self) -> Result<(), PersistenceError> {
        let writers: Vec<Arc<Core>> = self
            .backend
            .tracker()
            .writers
            .values()
            .filter_map(|writer| writer.as_ref().and_then(Weak::upgrade))
            .collect();
        let mut errors = Vec::new();
        for writer in writers {
            match writer.flush().await {
                // One that closed meanwhile counts as flushed.
                Ok(()) | Err(PersistenceError::HandleClosed { .. }) => {}
                Err(error) => errors.push(error),
            }
        }
        match errors.len() {
            0 => Ok(()),
            1 => Err(errors.remove(0)),
            _ => Err(PersistenceError::Aggregate {
                message: format!("{NAME} flush failed"),
                errors,
            }),
        }
    }

    /// Makes the next append's sync fail after writing its bytes.
    #[cfg(test)]
    fn fail_next_sync(&self) {
        self.backend.fail_next_sync.store(true, Ordering::SeqCst);
    }
}

/// The snapshot of a stored session; `None` when its file went away.
fn snapshot(
    header: SessionHeader,
    path: &Path,
) -> Result<Option<SessionSnapshot>, PersistenceError> {
    match file::identity(path) {
        Ok(identity) => Ok(Some(SessionSnapshot {
            header,
            format_status: Some(FormatStatus::Current),
            revision: identity.revision(),
            size_bytes: Some(identity.size),
        })),
        Err(error) if not_found(&error) => Ok(None),
        Err(error) => Err(io_error(format!("stat {}", path.display()), error)),
    }
}

impl SessionPersistence for JsonlPersistence {
    fn create(
        &self,
        header: SessionHeader,
        options: CreateOptions,
    ) -> PersistenceFuture<'_, Box<dyn SessionHandle>> {
        Box::pin(self.create_handle(header, options))
    }

    fn open<'a>(
        &'a self,
        id: &'a SessionId,
        access: SessionAccess,
    ) -> PersistenceFuture<'a, Box<dyn SessionHandle>> {
        Box::pin(self.open_handle(id, access))
    }

    fn stat<'a>(&'a self, id: &'a SessionId) -> PersistenceFuture<'a, Option<SessionSnapshot>> {
        Box::pin(self.stat_session(id))
    }

    fn list(&self) -> PersistenceFuture<'_, Vec<SessionSnapshot>> {
        Box::pin(self.list_sessions())
    }

    fn flush(&self) -> PersistenceFuture<'_, ()> {
        Box::pin(self.flush_all())
    }
}

#[cfg(test)]
mod tests {
    use karaka_session::{Seq, SessionEvent, Time};

    use super::*;

    fn entry(seq: u64, event: SessionEvent) -> Entry {
        Entry {
            seq: Seq(seq),
            time: Time(seq as i64 + 1),
            ignorable: false,
            event,
        }
    }

    // jsonl.spec.ts: "a failed appendLines truncates partial bytes so a
    // retry has no seq gap"
    #[tokio::test]
    async fn a_failed_append_rolls_back_so_a_retry_has_no_seq_gap() {
        let root = std::env::temp_dir().join(format!(
            "karaka-jsonl-backend-rollback-{}",
            std::process::id()
        ));
        let persistence = JsonlPersistence::new(JsonlConfig { root: root.clone() }).unwrap();
        let header = SessionHeader::new(SessionId::new("truncate-retry"), 1000);
        let path = persistence.backend.log_path(&header).unwrap();
        let handle = persistence
            .create(header, CreateOptions::default())
            .await
            .unwrap();
        handle
            .append(vec![
                entry(0, SessionEvent::turn_start(1)),
                entry(1, SessionEvent::step_start(1, 1)),
            ])
            .await
            .unwrap();
        let size = fs::metadata(&path).unwrap().len();

        let batch = vec![
            entry(2, SessionEvent::step_end(1, 1)),
            entry(3, SessionEvent::turn_start(2)),
        ];
        persistence.fail_next_sync();
        let failure = handle.append(batch.clone()).await.unwrap_err();
        assert!(failure.to_string().contains("simulated fsync ENOSPC"));
        // The partial bytes are gone, and the cursor didn't move.
        assert_eq!(fs::metadata(&path).unwrap().len(), size);

        handle.append(batch).await.unwrap();
        let seqs: Vec<u64> = handle
            .read(0, None)
            .await
            .unwrap()
            .iter()
            .map(|entry| entry.seq.0)
            .collect();
        assert_eq!(seqs, [0, 1, 2, 3]);
        handle.close().await.unwrap();
        fs::remove_dir_all(root).unwrap();
    }
}
