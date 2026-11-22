//! The file mechanics: publishing a new log without overwriting, durable
//! appends that roll back on failure, truncating a torn tail, and reading
//! a file whole or its first line. (dsh: the materialization, append and
//! discovery helpers of `session-persistence-jsonl/src/index.ts`, and
//! `readStableJsonlFile` in `generation.ts`.)
//!
//! Every function here blocks; the backend runs them off the async
//! executor.

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use karaka_session_persistence::PersistenceError;

use crate::layout::parse_generation_filename;

/// A file's physical identity: equal identities mean the same bytes, as
/// far as metadata can tell. (dsh: `JsonlPhysicalIdentity`.)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Identity {
    dev: u64,
    ino: u64,
    pub(crate) size: u64,
    mtime_ns: i128,
    ctime_ns: i128,
}

impl Identity {
    pub(crate) fn of(metadata: &fs::Metadata) -> Self {
        let nanos = |secs: i64, nsec: i64| i128::from(secs) * 1_000_000_000 + i128::from(nsec);
        Self {
            dev: metadata.dev(),
            ino: metadata.ino(),
            size: metadata.size(),
            mtime_ns: nanos(metadata.mtime(), metadata.mtime_nsec()),
            ctime_ns: nanos(metadata.ctime(), metadata.ctime_nsec()),
        }
    }

    /// The revision token `stat` and `list` give. (dsh: `fileRevision`.)
    pub(crate) fn revision(&self) -> String {
        format!(
            "{}:{}:{}:{}:{}",
            self.dev, self.ino, self.size, self.mtime_ns, self.ctime_ns
        )
    }
}

/// The identity of the file at `path`.
pub(crate) fn identity(path: &Path) -> io::Result<Identity> {
    fs::metadata(path).map(|metadata| Identity::of(&metadata))
}

/// Reads one stable revision of `path`, retrying once if it changed during
/// the read. When a writer keeps appending through both reads, gives the
/// prefix the file had before the second read, instead of starving.
/// (dsh: `readStableSnapshot`.)
pub(crate) fn read_stable(path: &Path) -> io::Result<(Vec<u8>, Identity)> {
    let mut before = identity(path)?;
    for attempt in 0..2 {
        let bytes = fs::read(path)?;
        let after = identity(path)?;
        if before == after {
            return Ok((bytes, after));
        }
        if attempt == 1 {
            let length = usize::try_from(before.size)
                .unwrap_or(usize::MAX)
                .min(bytes.len());
            return Ok((bytes[..length].to_vec(), before));
        }
        before = after;
    }
    unreachable!("the loop returns on its second pass")
}

/// The first line of `path` without its newline, read in small chunks so a
/// long log costs only its header; `None` when there is no complete first
/// line. (dsh: `readFirstLine`.)
pub(crate) fn read_first_line(path: &Path) -> io::Result<Option<Vec<u8>>> {
    let mut file = File::open(path)?;
    let mut line = Vec::new();
    let mut chunk = [0; 8192];
    loop {
        let read = file.read(&mut chunk)?;
        if read == 0 {
            return Ok(None);
        }
        if let Some(newline) = chunk[..read].iter().position(|byte| *byte == b'\n') {
            line.extend_from_slice(&chunk[..newline]);
            return Ok(Some(line));
        }
        line.extend_from_slice(&chunk[..read]);
    }
}

/// Makes a new entry in `dir` survive a power loss.
pub(crate) fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

fn make_dir(dir: &Path) -> io::Result<()> {
    DirBuilder::new().recursive(true).mode(0o700).create(dir)
}

/// A name no other temporary file in this process or another shares.
fn temporary_token() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.subsec_nanos())
        .unwrap_or(0);
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{:x}{nanos:08x}{count:x}", std::process::id())
}

/// Whether `dir` holds any canonical generation of a log.
pub(crate) fn has_generation(dir: &Path) -> io::Result<bool> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let name = entry?.file_name();
        if name.to_str().and_then(parse_generation_filename).is_some() {
            return Ok(true);
        }
    }
    Ok(false)
}

fn io_error(context: impl std::fmt::Display, error: io::Error) -> PersistenceError {
    PersistenceError::io(context.to_string(), error)
}

/// Publishes a new log at `final_path` holding `content`, durably, and
/// never over an existing one. (dsh: `materializePosix`.)
///
/// Each directory is made and its parent synced; the content goes to a
/// synced temporary file, published with `link`, which fails if the path
/// exists, where `rename` would replace it. Then the directory is synced,
/// and the temporary name removed.
pub(crate) fn materialize(
    root: &Path,
    project: &Path,
    dir: &Path,
    final_path: &Path,
    id: &str,
    content: &[u8],
) -> Result<(), PersistenceError> {
    let step = |what: &str, path: &Path| format!("{what} {}", path.display());
    make_dir(root).map_err(|error| io_error(step("create", root), error))?;
    if let Some(parent) = root.parent() {
        sync_dir(parent).map_err(|error| io_error(step("sync", parent), error))?;
    }
    make_dir(project).map_err(|error| io_error(step("create", project), error))?;
    sync_dir(root).map_err(|error| io_error(step("sync", root), error))?;
    make_dir(dir).map_err(|error| io_error(step("create", dir), error))?;
    sync_dir(project).map_err(|error| io_error(step("sync", project), error))?;
    // A file here is another session with this id on disk; `create` checks
    // first, so this only guards a race.
    if has_generation(dir).map_err(|error| io_error(step("read", dir), error))? {
        return Err(PersistenceError::Other(format!(
            "refusing to materialize \"{id}\": a log already exists on disk (open it instead)"
        )));
    }
    let temporary = final_path.with_file_name(format!(
        "{}.{}.tmp",
        final_path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default(),
        temporary_token()
    ));
    let write = || -> io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(content)?;
        file.sync_all()
    };
    if let Err(error) = write() {
        let _ = fs::remove_file(&temporary);
        return Err(io_error(step("write", &temporary), error));
    }
    if let Err(error) = fs::hard_link(&temporary, final_path) {
        let _ = fs::remove_file(&temporary);
        return Err(io_error(step("publish", final_path), error));
    }
    // Published; the new name isn't durable until its directory is synced.
    sync_dir(dir).map_err(|error| io_error(step("sync", dir), error))?;
    // The log is already durable, so a failure to remove the spare name
    // mustn't fail the append.
    let _ = fs::remove_file(&temporary);
    Ok(())
}

/// Writes `content` to the file and syncs it: what an append does.
pub(crate) fn write_and_sync(file: &mut File, content: &[u8]) -> io::Result<()> {
    file.write_all(content)?;
    file.sync_all()
}

/// Cuts the file at `path` back to `size` bytes, durably.
pub(crate) fn truncate(path: &Path, size: u64) -> io::Result<()> {
    let file = OpenOptions::new().write(true).open(path)?;
    file.set_len(size)?;
    file.sync_all()
}

/// Appends `content` to `path` through `write`, which writes and syncs. On
/// a failure the file is cut back to its earlier length with `rollback`
/// before the error is returned: the batch will be retried at the same seq,
/// and partial bytes would duplicate it. Both failures are reported when
/// the rollback fails too. (dsh: `appendLines` and `rollbackAppend`.)
pub(crate) fn append(
    path: &Path,
    content: &[u8],
    write: impl FnOnce(&mut File, &[u8]) -> io::Result<()>,
    rollback: impl FnOnce(&Path, u64) -> io::Result<()>,
) -> Result<(), PersistenceError> {
    let context = || format!("append to \"{}\"", path.display());
    let mut file = OpenOptions::new()
        .append(true)
        .open(path)
        .map_err(|error| io_error(context(), error))?;
    let before = file
        .metadata()
        .map_err(|error| io_error(context(), error))?
        .len();
    let Err(error) = write(&mut file, content) else {
        return Ok(());
    };
    drop(file);
    match rollback(path, before) {
        Ok(()) => Err(io_error(context(), error)),
        Err(rollback_error) => Err(PersistenceError::Aggregate {
            message: format!("failed to roll back append to \"{}\"", path.display()),
            errors: vec![
                io_error(context(), error),
                io_error(format!("roll back \"{}\"", path.display()), rollback_error),
            ],
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("karaka-jsonl-file-{name}-{}", temporary_token()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_failed_append_is_rolled_back_to_the_earlier_length() {
        let dir = scratch("rollback");
        let path = dir.join("log");
        fs::write(&path, b"header\n").unwrap();
        // The write puts bytes on disk, then its sync fails, as a full disk
        // would make it.
        let failure = append(
            &path,
            b"partial row\n",
            |file, content| {
                file.write_all(&content[..4])?;
                Err(io::Error::other("simulated fsync ENOSPC"))
            },
            truncate,
        )
        .unwrap_err();
        assert!(failure.to_string().contains("simulated fsync ENOSPC"));
        assert_eq!(fs::read(&path).unwrap(), b"header\n");
        // So a retry lands where the batch should.
        append(&path, b"row\n", write_and_sync, truncate).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"header\nrow\n");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_failed_rollback_is_reported_with_the_append_failure() {
        let dir = scratch("rollback-failure");
        let path = dir.join("log");
        fs::write(&path, b"header\n").unwrap();
        let failure = append(
            &path,
            b"row\n",
            |_, _| Err(io::Error::other("simulated append fsync failure")),
            |_, _| Err(io::Error::other("simulated rollback failure")),
        )
        .unwrap_err();
        let PersistenceError::Aggregate { message, errors } = failure else {
            panic!("both failures are reported");
        };
        assert_eq!(
            message,
            format!("failed to roll back append to \"{}\"", path.display())
        );
        assert_eq!(errors.len(), 2);
        assert!(
            errors[0]
                .to_string()
                .contains("simulated append fsync failure")
        );
        assert!(errors[1].to_string().contains("simulated rollback failure"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn materializing_never_overwrites_a_log() {
        let root = scratch("materialize");
        let project = root.join("_no-cwd");
        let dir = project.join("s");
        let path = dir.join("session.v4.jsonl");
        materialize(&root, &project, &dir, &path, "s", b"first\n").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"first\n");
        let refusal = materialize(&root, &project, &dir, &path, "s", b"second\n").unwrap_err();
        assert!(refusal.to_string().contains("a log already exists on disk"));
        assert_eq!(fs::read(&path).unwrap(), b"first\n");
        // Only the log is left: the temporary name is gone.
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_first_line_is_read_without_the_rest() {
        let dir = scratch("first-line");
        let path = dir.join("log");
        let long = "x".repeat(20_000);
        fs::write(&path, format!("{long}\nrest\n")).unwrap();
        assert_eq!(read_first_line(&path).unwrap(), Some(long.into_bytes()));
        fs::write(&path, b"no newline").unwrap();
        assert_eq!(read_first_line(&path).unwrap(), None);
        fs::remove_dir_all(dir).unwrap();
    }
}
