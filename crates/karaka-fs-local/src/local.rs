//! [`LocalFileSystem`]: the host filesystem behind the port. (dsh:
//! `LocalFileSystem` in `fs-local/src/index.ts`.)

use std::collections::HashMap;
use std::fmt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use karaka_fs::{
    AbortSignal, ByteRange, FileSystem, FsDirEntry, FsEditOutcome, FsEditRequest, FsError,
    FsErrorCode, FsFuture, FsInfo, FsPathInfo, FsTarget, FsType, FsVersion, FsWatch, FsWriteIntent,
    FsWriteOperation, FsWriteOutcome, TextStream, WatchListener,
};
use notify::Watcher;

use crate::fsio::{
    self, Internals, LocalTarget, apply_literal_edit, check, list_directory, local_display_path,
    normalize_line_endings, probe, probe_no_follow, raw, read_byte_window, read_for_edit,
    read_text_for_diff, read_whole_bytes, read_whole_text, resolve_local_target,
    restore_line_endings, stream_whole_text, write_file_atomic,
};
use crate::paths::{dirname, file_url, is_absolute, normalize_absolute, process_cwd};

/// The diff basis's default bound, 10 MiB. (dsh:
/// `DEFAULT_DIFF_BASIS_MAX_BYTES`.)
pub const DEFAULT_DIFF_BASIS_MAX_BYTES: u64 = 10 * 1024 * 1024;

/// The local backend's settings. (dsh: `Config`.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalFsConfig {
    /// What relative paths resolve against: a default, not a boundary that
    /// contains anything. The process's directory by default.
    pub cwd: String,
    /// The exclusive bound, in UTF-8 bytes, on each side of an overwrite's
    /// diff basis: at or over it, a write reports no `before`.
    pub diff_basis_max_bytes: u64,
}

impl Default for LocalFsConfig {
    fn default() -> Self {
        Self {
            cwd: process_cwd(),
            diff_basis_max_bytes: DEFAULT_DIFF_BASIS_MAX_BYTES,
        }
    }
}

/// A setting the backend refuses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigError(String);

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ConfigError {}

type Locks = HashMap<String, Arc<tokio::sync::Mutex<()>>>;

/// The host filesystem. Targets are keyed by realpath, so aliases share
/// stale guards, and a write through a symbolic link updates its file and
/// keeps the link. (dsh: `LocalFileSystem`.)
///
/// It contains nothing: `cwd` is only where relative paths start. Writes
/// and edits of one target take turns, first come first served, so a guard
/// is checked and the file written with no other mutation between; of two
/// at one version, one wins and the other is stale.
pub struct LocalFileSystem {
    config: LocalFsConfig,
    internals: Internals,
    locks: Mutex<Locks>,
}

impl LocalFileSystem {
    /// # Errors
    ///
    /// A diff limit of 0.
    pub fn new(config: LocalFsConfig) -> Result<Self, ConfigError> {
        if config.diff_basis_max_bytes == 0 {
            return Err(ConfigError(
                "fs-local: diff_basis_max_bytes must be a positive integer".into(),
            ));
        }
        Ok(Self {
            config,
            internals: Internals::default(),
            locks: Mutex::default(),
        })
    }

    /// The same backend, with test hooks into its writes and reads. (dsh:
    /// `internals`.)
    pub fn with_internals(mut self, internals: Internals) -> Self {
        self.internals = internals;
        self
    }

    pub fn config(&self) -> &LocalFsConfig {
        &self.config
    }

    /// Waits for `key`'s turn. (dsh: `withLock`.)
    async fn lock(&self, key: &str) -> KeyLock<'_> {
        let mutex = Arc::clone(self.locks().entry(key.to_owned()).or_default());
        let guard = mutex.lock_owned().await;
        KeyLock {
            fs: self,
            key: key.to_owned(),
            guard: Some(guard),
        }
    }

    fn locks(&self) -> MutexGuard<'_, Locks> {
        self.locks.lock().unwrap_or_else(PoisonError::into_inner)
    }

    async fn write(
        &self,
        target: &FsTarget,
        content: &str,
        expected: Option<&FsWriteIntent>,
        signal: Option<&AbortSignal>,
    ) -> Result<FsWriteOutcome, FsError> {
        let key = target.target_key.as_str();
        let display = &target.display_path;
        let _turn = self.lock(key).await;
        let existing = probe(key)
            .await
            .map_err(|error| raw("write", display, error))?;
        if existing
            .as_ref()
            .is_some_and(|existing| existing.kind != FsType::File)
        {
            return Err(FsError::new(
                FsErrorCode::NotRegularFile,
                format!("cannot write \"{display}\": not a regular file"),
            ));
        }
        match expected {
            Some(FsWriteIntent::ReplaceIfVersion(version)) => match &existing {
                None => {
                    return Err(FsError::new(
                        FsErrorCode::StaleVersion,
                        format!("cannot write \"{display}\": file no longer exists"),
                    ));
                }
                Some(existing) if existing.version != *version => {
                    return Err(FsError::new(
                        FsErrorCode::StaleVersion,
                        format!("cannot write \"{display}\": file changed since it was read"),
                    ));
                }
                Some(_) => {}
            },
            Some(FsWriteIntent::CreateIfAbsent) if existing.is_some() => {
                return Err(FsError::new(
                    FsErrorCode::NotObserved,
                    format!("cannot overwrite existing \"{display}\" without reading it first"),
                ));
            }
            _ => {}
        }
        // The diff basis is best-effort, bounded on the opened file itself,
        // and declined when either side reaches the limit.
        let max = self.config.diff_basis_max_bytes;
        let before = if existing.is_some() && (content.len() as u64) < max {
            read_text_for_diff(key, max, signal).await?
        } else {
            None
        };
        let create = matches!(expected, Some(FsWriteIntent::CreateIfAbsent));
        write_file_atomic(
            key,
            content,
            existing.as_ref().map(|existing| existing.mode),
            signal,
            &self.internals,
            create.then_some(display.as_str()),
        )
        .await?;
        let after = probe(key)
            .await
            .map_err(|error| raw("write", display, error))?;
        Ok(FsWriteOutcome {
            operation: if existing.is_some() {
                FsWriteOperation::Update
            } else {
                FsWriteOperation::Create
            },
            version: version_after(after.map(|after| after.version), key),
            before,
            // LF, to share `before`'s basis: a CRLF rewrite isn't every line
            // changed.
            after: normalize_line_endings(content),
        })
    }

    async fn edit(
        &self,
        target: &FsTarget,
        edit: &FsEditRequest,
        expected: Option<&FsVersion>,
        signal: Option<&AbortSignal>,
    ) -> Result<FsEditOutcome, FsError> {
        let key = target.target_key.as_str();
        let display = &target.display_path;
        let _turn = self.lock(key).await;
        let existing = probe(key)
            .await
            .map_err(|error| raw("edit", display, error))?;
        let stale = || {
            FsError::new(
                FsErrorCode::StaleVersion,
                format!("cannot edit \"{display}\": file changed since it was read"),
            )
        };
        // Freshness before the match, so an edit from an old read is
        // stale rather than not found. A missing target is stale either way.
        let Some(existing) = existing else {
            return Err(stale());
        };
        if existing.kind != FsType::File {
            return Err(FsError::new(
                FsErrorCode::NotRegularFile,
                format!("cannot edit \"{display}\": not a regular file"),
            ));
        }
        if expected.is_some_and(|version| *version != existing.version) {
            return Err(stale());
        }
        let original = read_for_edit(key, display, signal).await?;
        let edited = apply_literal_edit(
            &original.content,
            &edit.old_string,
            &edit.new_string,
            edit.replace_all,
            display,
        )?;
        let content = restore_line_endings(&edited.content, original.line_endings);
        write_file_atomic(
            key,
            &content,
            Some(existing.mode),
            signal,
            &self.internals,
            None,
        )
        .await?;
        let after = probe(key)
            .await
            .map_err(|error| raw("edit", display, error))?;
        Ok(FsEditOutcome {
            version: version_after(after.map(|after| after.version), key),
            before: original.content,
            after: edited.content,
        })
    }

    async fn start_watch(
        &self,
        target: &FsTarget,
        changed: WatchListener,
        signal: &AbortSignal,
    ) -> Result<FsWatch, FsError> {
        check(Some(signal), "watch")?;
        let path = self.process_path(target);
        let directory = self
            .stat(target, Some(signal))
            .await?
            .is_some_and(|info| info.kind == FsType::Directory);
        check(Some(signal), "watch")?;
        let root = if directory {
            path.clone()
        } else {
            dirname(&path)
        };
        let display = target.display_path.clone();
        let filter = WatchFilter {
            directory,
            watched: path,
            display_path: display.clone(),
            closed: Arc::new(AtomicBool::new(false)),
            changed: Arc::clone(&changed),
        };
        let closed = Arc::clone(&filter.closed);
        let handler = move |event| filter.hear(event);
        let mut watcher = notify::recommended_watcher(handler).map_err(|error| {
            let error = watch_error(&display, error);
            changed(Some(error.clone()));
            error
        })?;
        if let Err(error) = watcher.watch(Path::new(&root), notify::RecursiveMode::NonRecursive) {
            let error = watch_error(&display, error);
            changed(Some(error.clone()));
            return Err(error);
        }
        if signal.is_aborted() {
            closed.store(true, Ordering::SeqCst);
            return Err(FsError::aborted("watch"));
        }
        Ok(FsWatch::new(move || {
            Box::pin(async move {
                // Nothing is heard once closing starts.
                closed.store(true, Ordering::SeqCst);
                drop(watcher);
            })
        }))
    }
}

/// What a watch passes on to its listener. (dsh: chokidar's `ignored`
/// filter and its `all` and `error` handlers.)
struct WatchFilter {
    /// A directory hears all its direct entries; a file only its own.
    directory: bool,
    watched: String,
    display_path: String,
    /// Set as the watch closes: nothing is heard after.
    closed: Arc<AtomicBool>,
    changed: WatchListener,
}

impl WatchFilter {
    fn hear(&self, event: notify::Result<notify::Event>) {
        if self.closed.load(Ordering::SeqCst) {
            return;
        }
        match event {
            // Reads aren't changes, as chokidar doesn't report them.
            Ok(event) if matches!(event.kind, notify::EventKind::Access(_)) => {}
            Ok(event) => {
                let watched = Path::new(&self.watched);
                if self.directory || event.paths.iter().any(|entry| entry == watched) {
                    (self.changed)(None);
                }
            }
            Err(error) => (self.changed)(Some(watch_error(&self.display_path, error))),
        }
    }
}

fn watch_error(display_path: &str, error: notify::Error) -> FsError {
    FsError::with_source(
        FsErrorCode::IoError,
        format!("cannot watch \"{display_path}\": {error}"),
        error,
    )
}

/// The version a write or edit gives back; a sentinel when the file was
/// unlinked between publication and the probe. (dsh: `versionAfterWrite`.)
fn version_after(version: Option<FsVersion>, key: &str) -> FsVersion {
    version.unwrap_or_else(|| FsVersion::new(format!("missing:{key}")))
}

fn local(target: &FsTarget) -> LocalTarget {
    LocalTarget {
        display_path: target.display_path.clone(),
        target_key: target.target_key.clone(),
    }
}

/// A target's turn. Dropping it, even with the call's future, ends the
/// turn, and forgets the key's lock once no one else waits on it.
struct KeyLock<'a> {
    fs: &'a LocalFileSystem,
    key: String,
    guard: Option<tokio::sync::OwnedMutexGuard<()>>,
}

impl Drop for KeyLock<'_> {
    fn drop(&mut self) {
        drop(self.guard.take());
        let mut locks = self.fs.locks();
        if locks
            .get(&self.key)
            .is_some_and(|lock| Arc::strong_count(lock) == 1)
        {
            locks.remove(&self.key);
        }
    }
}

impl FileSystem for LocalFileSystem {
    /// Watches the directory itself, or a file's parent, without
    /// recursion, and hears a file's own entry alone. (dsh: `watch`, with
    /// chokidar at depth 0.)
    fn watch<'a>(
        &'a self,
        target: &'a FsTarget,
        changed: WatchListener,
        signal: &'a AbortSignal,
    ) -> FsFuture<'a, FsWatch> {
        Box::pin(self.start_watch(target, changed, signal))
    }

    fn resolve<'a>(
        &'a self,
        path: &'a str,
        cwd: Option<&'a str>,
        signal: Option<&'a AbortSignal>,
    ) -> FsFuture<'a, FsTarget> {
        Box::pin(async move {
            check(signal, "resolve")?;
            let local = resolve_local_target(cwd.unwrap_or(&self.config.cwd), path).await?;
            check(signal, "resolve")?;
            Ok(FsTarget {
                target_key: local.target_key,
                display_path: local.display_path,
            })
        })
    }

    fn process_path(&self, target: &FsTarget) -> String {
        target.target_key.as_str().to_owned()
    }

    fn process_path_from_host_path(&self, host_path: &str) -> Option<String> {
        is_absolute(host_path).then(|| normalize_absolute(host_path))
    }

    fn file_url(&self, target: &FsTarget) -> String {
        file_url(&self.process_path(target))
    }

    fn contains(&self, parent: &FsTarget, child: &FsTarget) -> bool {
        Path::new(child.target_key.as_str()).starts_with(parent.target_key.as_str())
    }

    fn stat<'a>(
        &'a self,
        target: &'a FsTarget,
        signal: Option<&'a AbortSignal>,
    ) -> FsFuture<'a, Option<FsInfo>> {
        Box::pin(async move {
            check(signal, "stat")?;
            let info = probe(target.target_key.as_str())
                .await
                .map_err(|error| raw("stat", &target.display_path, error))?;
            check(signal, "stat")?;
            Ok(info.map(|info| FsInfo {
                version: info.version,
                kind: info.kind,
                size: Some(info.size),
            }))
        })
    }

    fn lstat<'a>(
        &'a self,
        path: &'a str,
        cwd: Option<&'a str>,
        signal: Option<&'a AbortSignal>,
    ) -> FsFuture<'a, Option<FsPathInfo>> {
        Box::pin(async move {
            check(signal, "lstat")?;
            if path.trim().is_empty() {
                return Err(FsError::new(
                    FsErrorCode::NotFound,
                    "file_path must be a non-empty string",
                ));
            }
            let display = local_display_path(cwd.unwrap_or(&self.config.cwd), path);
            let info = probe_no_follow(&display)
                .await
                .map_err(|error| raw("lstat", &display, error))?;
            check(signal, "lstat")?;
            Ok(info.map(|info| FsPathInfo {
                version: info.version,
                kind: info.kind,
                size: Some(info.size),
            }))
        })
    }

    fn read_text<'a>(
        &'a self,
        target: &'a FsTarget,
        signal: Option<&'a AbortSignal>,
    ) -> FsFuture<'a, String> {
        Box::pin(async move { read_whole_text(&local(target), signal).await })
    }

    fn stream_text<'a>(
        &'a self,
        target: &'a FsTarget,
        signal: Option<&'a AbortSignal>,
    ) -> FsFuture<'a, TextStream> {
        Box::pin(async move { Ok(stream_whole_text(local(target), signal.cloned())) })
    }

    fn read_bytes<'a>(
        &'a self,
        target: &'a FsTarget,
        signal: Option<&'a AbortSignal>,
        max_bytes: u64,
    ) -> FsFuture<'a, Vec<u8>> {
        Box::pin(async move {
            read_whole_bytes(&local(target), signal, max_bytes, &self.internals).await
        })
    }

    fn read_byte_range<'a>(
        &'a self,
        target: &'a FsTarget,
        range: ByteRange,
        signal: Option<&'a AbortSignal>,
    ) -> FsFuture<'a, Vec<u8>> {
        Box::pin(async move { read_byte_window(&local(target), range, signal).await })
    }

    fn list_dir<'a>(
        &'a self,
        target: &'a FsTarget,
        signal: Option<&'a AbortSignal>,
    ) -> FsFuture<'a, Vec<FsDirEntry>> {
        Box::pin(async move {
            let entries = list_directory(&local(target), signal).await?;
            Ok(entries
                .into_iter()
                .map(|entry: fsio::LocalDirEntry| FsDirEntry {
                    name: entry.name,
                    kind: entry.kind,
                    target: FsTarget {
                        target_key: entry.target.target_key,
                        display_path: entry.target.display_path,
                    },
                    version: entry.version,
                    size: entry.size,
                })
                .collect())
        })
    }

    fn write_text<'a>(
        &'a self,
        target: &'a FsTarget,
        content: &'a str,
        expected: Option<&'a FsWriteIntent>,
        signal: Option<&'a AbortSignal>,
    ) -> FsFuture<'a, FsWriteOutcome> {
        Box::pin(self.write(target, content, expected, signal))
    }

    fn edit_text<'a>(
        &'a self,
        target: &'a FsTarget,
        edit: &'a FsEditRequest,
        expected: Option<&'a FsVersion>,
        signal: Option<&'a AbortSignal>,
    ) -> FsFuture<'a, FsEditOutcome> {
        Box::pin(self.edit(target, edit, expected, signal))
    }
}

impl fmt::Debug for LocalFileSystem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalFileSystem")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use karaka_fs::FsWriteIntent;
    use notify::event::{AccessKind, CreateKind, DataChange, EventKind, ModifyKind};

    use super::*;

    /// A filter for `watched`, and what it passed on: `None` for a change.
    fn filter(directory: bool) -> (WatchFilter, Arc<Mutex<Vec<Option<FsErrorCode>>>>) {
        let heard = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&heard);
        let filter = WatchFilter {
            directory,
            watched: "/workspace/file.txt".into(),
            display_path: "file.txt".into(),
            closed: Arc::new(AtomicBool::new(false)),
            changed: Arc::new(move |error: Option<FsError>| {
                sink.lock().unwrap().push(error.map(|error| error.code()));
            }),
        };
        (filter, heard)
    }

    fn event(kind: EventKind, path: &str) -> notify::Result<notify::Event> {
        Ok(notify::Event::new(kind).add_path(path.into()))
    }

    fn change() -> EventKind {
        EventKind::Modify(ModifyKind::Data(DataChange::Content))
    }

    // watch.spec: "waits for the parent watch of a file target": the
    // parent's own events and a sibling's aren't heard, the file's are.
    #[test]
    fn a_file_watch_hears_its_file_alone() {
        let (filter, heard) = filter(false);
        filter.hear(event(EventKind::Create(CreateKind::Folder), "/workspace"));
        filter.hear(event(change(), "/workspace/unrelated.txt"));
        assert!(heard.lock().unwrap().is_empty());
        filter.hear(event(change(), "/workspace/file.txt"));
        assert_eq!(*heard.lock().unwrap(), [None]);
    }

    // "watches a directory itself without filtering its direct entries"
    #[test]
    fn a_directory_watch_hears_every_entry() {
        let (filter, heard) = filter(true);
        filter.hear(event(change(), "/workspace/file.txt/child.txt"));
        assert_eq!(*heard.lock().unwrap(), [None]);
    }

    // Reads aren't changes, as chokidar's `all` doesn't report them.
    #[test]
    fn a_read_isnt_a_change() {
        let (filter, heard) = filter(false);
        filter.hear(event(
            EventKind::Access(AccessKind::Any),
            "/workspace/file.txt",
        ));
        assert!(heard.lock().unwrap().is_empty());
    }

    // "normalizes watcher errors after initialization", and nothing is
    // heard once the watch closes
    #[test]
    fn a_failure_is_heard_as_an_io_error_until_the_watch_closes() {
        let (filter, heard) = filter(false);
        filter.hear(Err(notify::Error::generic("watch failed")));
        assert_eq!(*heard.lock().unwrap(), [Some(FsErrorCode::IoError)]);
        filter.closed.store(true, Ordering::SeqCst);
        filter.hear(event(change(), "/workspace/file.txt"));
        filter.hear(Err(notify::Error::generic("watch unavailable")));
        assert_eq!(heard.lock().unwrap().len(), 1);
    }

    struct Dir(std::path::PathBuf);

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    // dsh's `lockCount` checks: "releases per-target mutation locks after
    // success and failure", and after an abort and a race.
    #[tokio::test]
    async fn every_targets_lock_is_released() {
        let dir = Dir(std::env::temp_dir().join(format!(
            "karaka-fs-locks-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )));
        std::fs::create_dir(&dir.0).unwrap();
        let fs = LocalFileSystem::new(LocalFsConfig {
            cwd: dir.0.to_str().unwrap().to_owned(),
            ..LocalFsConfig::default()
        })
        .unwrap();
        let target = fs.resolve("a.txt", None, None).await.unwrap();
        let create = FsWriteIntent::CreateIfAbsent;
        fs.write_text(&target, "created", Some(&create), None)
            .await
            .unwrap();
        assert!(fs.locks().is_empty());
        let refused = fs.write_text(&target, "again", Some(&create), None).await;
        assert_eq!(refused.unwrap_err().code(), FsErrorCode::NotObserved);
        assert!(fs.locks().is_empty());
        let aborted = fs
            .write_text(&target, "x", None, Some(&AbortSignal::aborted()))
            .await;
        assert_eq!(aborted.unwrap_err().code(), FsErrorCode::Aborted);
        assert!(fs.locks().is_empty());
        let version = fs.stat(&target, None).await.unwrap().unwrap().version;
        let replace = FsWriteIntent::ReplaceIfVersion(version);
        let _ = tokio::join!(
            fs.write_text(&target, "one", Some(&replace), None),
            fs.write_text(&target, "two", Some(&replace), None),
        );
        assert!(fs.locks().is_empty());
    }
}
