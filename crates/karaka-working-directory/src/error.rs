//! [`WorkingDirectoryError`], in dsh's words.

use std::fmt;

use karaka_fs::FsError;
use karaka_session::{LogError, SessionId};

/// Why a directory couldn't be read, checked or changed.
#[derive(Clone, Debug)]
pub enum WorkingDirectoryError {
    /// The configured default isn't absolute. (dsh: "working-directory:
    /// defaultDirectory must be absolute".)
    RelativeDefault,
    /// `set` was given an empty path. (dsh: "working-directory: cd must not
    /// be empty".)
    EmptyPath,
    /// The path isn't an existing directory. (dsh: "working-directory:
    /// directory does not exist: "path"".)
    Missing(String),
    /// The caller's signal aborted the operation. dsh rejects with the
    /// signal's reason; Karaka's signals carry none.
    Cancelled,
    /// The service was stopped. (dsh: "working-directory service
    /// disposed".)
    Disposed,
    /// The store has no session for the agent.
    NoSession(SessionId),
    /// The filesystem refused.
    Fs(FsError),
    /// The log refused the change.
    Log(LogError),
}

impl fmt::Display for WorkingDirectoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RelativeDefault => {
                f.write_str("working-directory: defaultDirectory must be absolute")
            }
            Self::EmptyPath => f.write_str("working-directory: cd must not be empty"),
            Self::Missing(path) => write!(
                f,
                "working-directory: directory does not exist: {}",
                quoted(path)
            ),
            Self::Cancelled => f.write_str("working-directory: cancelled"),
            Self::Disposed => f.write_str("working-directory service disposed"),
            Self::NoSession(id) => write!(f, "working-directory: no session \"{id}\""),
            Self::Fs(error) => error.fmt(f),
            Self::Log(error) => error.fmt(f),
        }
    }
}

/// Equal by kind and message; a filesystem error by its code and message.
impl PartialEq for WorkingDirectoryError {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Fs(a), Self::Fs(b)) => a.code() == b.code() && a.message() == b.message(),
            (Self::Missing(a), Self::Missing(b)) => a == b,
            (Self::NoSession(a), Self::NoSession(b)) => a == b,
            (Self::Log(a), Self::Log(b)) => a == b,
            (a, b) => std::mem::discriminant(a) == std::mem::discriminant(b),
        }
    }
}

impl std::error::Error for WorkingDirectoryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Fs(error) => Some(error),
            Self::Log(error) => Some(error),
            _ => None,
        }
    }
}

impl From<LogError> for WorkingDirectoryError {
    fn from(error: LogError) -> Self {
        Self::Log(error)
    }
}

/// `path` as a JSON string. (dsh: `JSON.stringify(path)`.)
pub(crate) fn quoted(path: &str) -> String {
    serde_json::to_string(path).unwrap_or_else(|_| format!("\"{path}\""))
}
