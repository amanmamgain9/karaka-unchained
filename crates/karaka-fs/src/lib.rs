//! The filesystem seam: one execution world's files, behind a port.
//! (dsh: `@deepseek-ai/dsh-fs`, 0.2.1-rc.2.)
//!
//! [`FileSystem`] is the port a backend provides. It resolves a path to a
//! stable [`FsTarget`], reads whole text files or streams them, reads raw
//! bytes under a cap or in a window, lists one directory level, and writes
//! or edits text atomically, guarded by an [`FsVersion`] when asked.
//! Failures are [`FsError`]s with stable [`FsErrorCode`]s. Each backend is
//! its own crate; `karaka-fs-local` is the first.
//!
//! The seam has no plugin of its own: a backend's plugin provides the
//! `dyn FileSystem` port.
//!
//! - `types.rs`: targets, versions, metadata, writes and edits.
//! - `error.rs`: the error and its codes.
//! - `abort.rs`: the abort signal calls take.
//! - `filesystem.rs`: the port.

mod abort;
mod error;
mod filesystem;
mod types;

pub use abort::{AbortController, AbortSignal};
pub use error::{FsError, FsErrorCode};
pub use filesystem::{FileSystem, FsFuture, FsWatch, TextStream, WatchListener};
pub use types::{
    ByteRange, FsDirEntry, FsEditOutcome, FsEditRequest, FsInfo, FsObservation, FsPathInfo,
    FsPathType, FsTarget, FsTargetKey, FsType, FsVersion, FsWriteIntent, FsWriteOperation,
    FsWriteOutcome,
};
