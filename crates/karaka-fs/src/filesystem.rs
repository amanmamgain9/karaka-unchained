//! [`FileSystem`]: the port a backend provides. (dsh: `FileSystem` in
//! `fs/src/index.ts`.)

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use futures_core::Stream;

use crate::{
    AbortSignal, ByteRange, FsDirEntry, FsEditOutcome, FsEditRequest, FsError, FsErrorCode, FsInfo,
    FsPathInfo, FsTarget, FsVersion, FsWriteIntent, FsWriteOutcome,
};

/// A future a filesystem call returns.
pub type FsFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, FsError>> + Send + 'a>>;

/// A streamed read: decoded text chunks, whose boundaries mean nothing.
pub type TextStream = Pin<Box<dyn Stream<Item = Result<String, FsError>> + Send>>;

/// Told that a watched target changed, or that the watch failed. (dsh:
/// `watch`'s `changed`.)
pub type WatchListener = Arc<dyn Fn(Option<FsError>) + Send + Sync>;

/// An active watch. [`close`](Self::close) ends it; dropping it does too.
pub struct FsWatch {
    close: Box<dyn FnOnce() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send>,
}

impl FsWatch {
    /// A watch that `close` ends.
    pub fn new(
        close: impl FnOnce() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + 'static,
    ) -> Self {
        Self {
            close: Box::new(close),
        }
    }

    /// Ends the watch, and waits until it has. (dsh: the close function
    /// `watch` resolves with.)
    pub async fn close(self) {
        (self.close)().await;
    }
}

impl std::fmt::Debug for FsWatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FsWatch")
    }
}

/// One execution world's filesystem. (dsh: the abstract `FileSystem`
/// service, `ctx.fs`.)
///
/// A backend owns target identity, process paths and file URIs,
/// containment, text decoding and binary refusal, and atomic mutations.
/// Targets keep one identity across aliases; reads give regular UTF-8 text
/// or a typed error; listings are ordered and never read content; writes
/// and edits are atomic. Their guards add stale protection without changing
/// the unguarded contract.
///
/// Every call that waits takes an optional [`AbortSignal`]; an aborted one
/// ends the call with `FS_ABORTED`.
pub trait FileSystem: Send + Sync {
    /// Watches one file, or a directory's direct entries, calling `changed`
    /// on each change and on a failure. Resolves once the watch is active;
    /// `signal` cancels only its start. (dsh: `watch`.)
    ///
    /// A backend that can't watch refuses with `FS_IO_ERROR`, as this
    /// default does.
    fn watch<'a>(
        &'a self,
        target: &'a FsTarget,
        changed: WatchListener,
        signal: &'a AbortSignal,
    ) -> FsFuture<'a, FsWatch> {
        let _ = (target, changed);
        Box::pin(async move {
            if signal.is_aborted() {
                return Err(FsError::aborted("watch"));
            }
            Err(FsError::new(
                FsErrorCode::IoError,
                "Filesystem watching is not supported by this provider.",
            ))
        })
    }

    /// Resolves `path` to a stable target; a relative path resolves against
    /// `cwd`, or the backend's own base. The same file gives the same key
    /// through any alias. (dsh: `resolve`.)
    fn resolve<'a>(
        &'a self,
        path: &'a str,
        cwd: Option<&'a str>,
        signal: Option<&'a AbortSignal>,
    ) -> FsFuture<'a, FsTarget>;

    /// The absolute path a subprocess in this world opens the target by.
    /// Not the target key, which stays opaque. (dsh: `processPath`.)
    fn process_path(&self, target: &FsTarget) -> String;

    /// The process path of the host's absolute `host_path`, when this world
    /// sees the same file there; `None`, as by default, when it can't.
    /// (dsh: `processPathFromHostPath`.)
    fn process_path_from_host_path(&self, host_path: &str) -> Option<String> {
        let _ = host_path;
        None
    }

    /// The target's `file:` URI in this world. (dsh: `fileUrl`.)
    fn file_url(&self, target: &FsTarget) -> String;

    /// Whether `child` is `parent` or under it, both from this backend.
    /// (dsh: `contains`.)
    fn contains(&self, parent: &FsTarget, child: &FsTarget) -> bool;

    /// The target's metadata, or `None` when it doesn't exist. (dsh:
    /// `stat`.)
    fn stat<'a>(
        &'a self,
        target: &'a FsTarget,
        signal: Option<&'a AbortSignal>,
    ) -> FsFuture<'a, Option<FsInfo>>;

    /// A path's metadata without following its last component, so a
    /// symbolic link can be refused before `resolve` follows it; `None`
    /// when absent. `cwd` is as `resolve`'s. (dsh: `lstat`.)
    fn lstat<'a>(
        &'a self,
        path: &'a str,
        cwd: Option<&'a str>,
        signal: Option<&'a AbortSignal>,
    ) -> FsFuture<'a, Option<FsPathInfo>>;

    /// The whole regular text file, decoded. (dsh: `readText`.)
    fn read_text<'a>(
        &'a self,
        target: &'a FsTarget,
        signal: Option<&'a AbortSignal>,
    ) -> FsFuture<'a, String>;

    /// The whole regular text file as decoded chunks, for large files,
    /// with `read_text`'s checks. The backend decodes across chunks. (dsh:
    /// `streamText`.)
    fn stream_text<'a>(
        &'a self,
        target: &'a FsTarget,
        signal: Option<&'a AbortSignal>,
    ) -> FsFuture<'a, TextStream>;

    /// The whole regular file's raw bytes, at most `max_bytes`: a larger
    /// one fails with `FS_TOO_LARGE`, never truncated. (dsh: `readBytes`.)
    fn read_bytes<'a>(
        &'a self,
        target: &'a FsTarget,
        signal: Option<&'a AbortSignal>,
        max_bytes: u64,
    ) -> FsFuture<'a, Vec<u8>>;

    /// The raw bytes of `range`: shorter where the file ends inside it,
    /// empty past the end. Never more than the window is held. (dsh:
    /// `readByteRange`.)
    fn read_byte_range<'a>(
        &'a self,
        target: &'a FsTarget,
        range: ByteRange,
        signal: Option<&'a AbortSignal>,
    ) -> FsFuture<'a, Vec<u8>>;

    /// A directory's direct children, in name order, with resolved targets
    /// and cheap metadata. (dsh: `listDir`.)
    fn list_dir<'a>(
        &'a self,
        target: &'a FsTarget,
        signal: Option<&'a AbortSignal>,
    ) -> FsFuture<'a, Vec<FsDirEntry>>;

    /// Atomically creates or replaces the file with `content`, under
    /// `expected` if given. (dsh: `writeText`; the sandbox policy it also
    /// takes waits for a sandboxing backend.)
    fn write_text<'a>(
        &'a self,
        target: &'a FsTarget,
        content: &'a str,
        expected: Option<&'a FsWriteIntent>,
        signal: Option<&'a AbortSignal>,
    ) -> FsFuture<'a, FsWriteOutcome>;

    /// Atomically applies a literal edit. With `expected`, the version is
    /// checked before matching, so stale content reports
    /// `FS_STALE_VERSION`. (dsh: `editText`.)
    fn edit_text<'a>(
        &'a self,
        target: &'a FsTarget,
        edit: &'a FsEditRequest,
        expected: Option<&'a FsVersion>,
        signal: Option<&'a AbortSignal>,
    ) -> FsFuture<'a, FsEditOutcome>;
}
