//! [`FsError`] and its codes. (dsh: `FsError` and `FsErrorCode` in
//! `fs/src/types.ts`.)

use std::error::Error;
use std::fmt;
use std::sync::Arc;

/// A stable, machine-routable reason a filesystem call failed, so callers
/// branch on it rather than on the message. (dsh: `FsErrorCode`.)
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FsErrorCode {
    NotFound,
    NotDirectory,
    NotText,
    NotRegularFile,
    TooLarge,
    PermissionDenied,
    SandboxDenied,
    IoError,
    StaleVersion,
    NotObserved,
    AmbiguousEdit,
    EditNotFound,
    Aborted,
}

impl FsErrorCode {
    /// dsh's name for it, such as `FS_NOT_FOUND`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotFound => "FS_NOT_FOUND",
            Self::NotDirectory => "FS_NOT_DIRECTORY",
            Self::NotText => "FS_NOT_TEXT",
            Self::NotRegularFile => "FS_NOT_REGULAR_FILE",
            Self::TooLarge => "FS_TOO_LARGE",
            Self::PermissionDenied => "FS_PERMISSION_DENIED",
            Self::SandboxDenied => "FS_SANDBOX_DENIED",
            Self::IoError => "FS_IO_ERROR",
            Self::StaleVersion => "FS_STALE_VERSION",
            Self::NotObserved => "FS_NOT_OBSERVED",
            Self::AmbiguousEdit => "FS_AMBIGUOUS_EDIT",
            Self::EditNotFound => "FS_EDIT_NOT_FOUND",
            Self::Aborted => "FS_ABORTED",
        }
    }
}

impl fmt::Display for FsErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A filesystem failure: a code, a message, and the error under it, if
/// any. Backends and the layers over them raise the same codes. (dsh:
/// `FsError`.)
#[derive(Clone, Debug)]
pub struct FsError {
    code: FsErrorCode,
    message: String,
    source: Option<Arc<dyn Error + Send + Sync>>,
}

impl FsError {
    pub fn new(code: FsErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            source: None,
        }
    }

    /// One caused by `source`. (dsh: `{ cause }`.)
    pub fn with_source(
        code: FsErrorCode,
        message: impl Into<String>,
        source: impl Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            code,
            message: message.into(),
            source: Some(Arc::new(source)),
        }
    }

    /// `FS_ABORTED`, with dsh's message: "`verb` aborted".
    pub fn aborted(verb: &str) -> Self {
        Self::new(FsErrorCode::Aborted, format!("{verb} aborted"))
    }

    pub fn code(&self) -> FsErrorCode {
        self.code
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for FsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for FsError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn Error + 'static))
    }
}
