//! The lock that keeps other processes from writing a session this process
//! writes. (dsh: `session-persistence-jsonl/src/lease.ts`.)
//!
//! It is a non-blocking `flock(2)` on `session.lock` beside the log, held
//! for the write handle's life. The kernel arbitrates, and drops the lock
//! when its descriptor closes, so a crashed holder never blocks the next
//! writer. A live but stuck holder keeps it until its process exits: there
//! is no expiry that could take the session from a writer whose later
//! appends would then tear the log. Readers never touch the lock.
//!
//! A POSIX lock names an inode, not a path, so after locking, the holder
//! checks the locked file is still the one at the path, and tries again if
//! not. Removing a live session's lock file therefore gives up its
//! exclusion; nothing here does that, and releasing never removes the file.
//!
//! The lock is Rust's `File::try_lock`, which is `flock` on Unix. dsh's
//! Windows lock is a named kernel semaphore; that isn't ported, and this
//! crate builds on Unix only for now. Advisory `flock` is unreliable on
//! some network filesystems, such as NFSv3.

use std::fs::{DirBuilder, File, OpenOptions, TryLockError};
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::Path;

use karaka_session::SessionId;
use karaka_session_persistence::PersistenceError;

use crate::layout::LOCK_FILENAME;

/// A held write lock. Dropping it releases the lock.
#[derive(Debug)]
pub(crate) struct Lease {
    _file: File,
}

impl Lease {
    /// Takes the lock in the session directory `dir`, creating it if
    /// needed.
    ///
    /// # Errors
    ///
    /// [`PersistenceError::AlreadyOwned`] while another holder keeps it.
    pub(crate) fn acquire(dir: &Path, id: &SessionId) -> Result<Self, PersistenceError> {
        let path = dir.join(LOCK_FILENAME);
        // Owner-only, as materialization makes them: the lock may be what
        // creates the session's directory.
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(|error| io_error(&path, error))?;
        // A bounded retry: the locked inode may have been replaced at the
        // path between the open and the lock. A steady state needs one pass.
        for _ in 0..3 {
            let file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .open(&path)
                .map_err(|error| io_error(&path, error))?;
            match file.try_lock() {
                Ok(()) => {}
                Err(TryLockError::WouldBlock) => {
                    return Err(PersistenceError::AlreadyOwned(id.clone()));
                }
                Err(TryLockError::Error(error)) => return Err(io_error(&path, error)),
            }
            let held = file.metadata().map_err(|error| io_error(&path, error))?;
            match std::fs::metadata(&path) {
                Ok(current) if current.ino() == held.ino() && current.dev() == held.dev() => {
                    return Ok(Self { _file: file });
                }
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(io_error(&path, error)),
            }
            // The locked inode isn't the file at the path any more: start
            // over against whatever is there now.
        }
        Err(PersistenceError::AlreadyOwned(id.clone()))
    }
}

fn io_error(path: &Path, error: io::Error) -> PersistenceError {
    PersistenceError::io(format!("session lock {}", path.display()), error)
}
