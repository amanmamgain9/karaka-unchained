//! The seam's errors: one kind per dsh error class, with dsh's messages.
//! (dsh: `session-persistence/src/errors.ts`.)

use std::error::Error;
use std::fmt;
use std::io;
use std::path::PathBuf;

use karaka_session::{SESSION_FORMAT_VERSION, SessionId};

/// Where a backend keeps a session's raw log, named in a refusal so an
/// operator can find the file a build refused. (dsh: `SessionLocation`.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionLocation {
    /// The backend's kind of artifact, such as `"jsonl"`.
    pub kind: &'static str,
    /// The absolute path of the raw log.
    pub path: PathBuf,
}

/// Why a persistence call failed.
#[derive(Debug)]
#[non_exhaustive]
pub enum PersistenceError {
    /// No stored session has this id. (dsh:
    /// `SessionPersistenceNotFoundError`.)
    NotFound(SessionId),
    /// `create` named an id that is already stored, or already being
    /// created. (dsh: `SessionAlreadyExistsError`.)
    AlreadyExists(SessionId),
    /// Another write handle owns the session: in this process, or, through
    /// the backend's lock, in another. (dsh: `SessionAlreadyOwnedError`.)
    AlreadyOwned(SessionId),
    /// A write operation on a read handle. (dsh: `SessionReadOnlyError`.)
    ReadOnly {
        id: SessionId,
        operation: &'static str,
    },
    /// The handle lost its write ownership. No backend here raises it yet;
    /// it is dsh's kind for a revoked lease. (dsh:
    /// `SessionOwnershipLostError`.)
    OwnershipLost(SessionId),
    /// An operation on a closed handle. (dsh: `SessionHandleClosedError`.)
    HandleClosed {
        id: SessionId,
        operation: &'static str,
    },
    /// Committed bytes the backend can't interpret: damage, not a format
    /// it doesn't know. (dsh: `SessionPersistenceCorruptionError`.)
    Corruption(String),
    /// A log this build won't interpret: a format version it doesn't read,
    /// or an event type it doesn't know. The message names the raw log when
    /// there is one. (dsh: `SessionFormatUnsupportedError`.)
    FormatUnsupported {
        message: String,
        location: Option<SessionLocation>,
    },
    /// A storage call failed.
    Io { context: String, source: io::Error },
    /// Several failures at once, such as a failed append and its failed
    /// rollback. (dsh: `AggregateError`.)
    Aggregate {
        message: String,
        errors: Vec<PersistenceError>,
    },
    /// Any other refusal, with dsh's wording; dsh throws a plain `Error`
    /// for these, such as a batch that doesn't continue the log.
    Other(String),
}

impl PersistenceError {
    /// A format refusal naming the raw log at `location`, as dsh's
    /// `unsupported` helper builds it.
    pub fn unsupported(reason: impl Into<String>, location: Option<SessionLocation>) -> Self {
        let reason = reason.into();
        let message = match &location {
            Some(location) => format!("{reason} (raw log: {})", location.path.display()),
            None => reason,
        };
        Self::FormatUnsupported { message, location }
    }

    /// An I/O failure while doing `context`.
    pub fn io(context: impl Into<String>, source: io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }
}

impl fmt::Display for PersistenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(id) => write!(f, "session \"{id}\" not found"),
            Self::AlreadyExists(id) => write!(f, "session \"{id}\" already exists"),
            Self::AlreadyOwned(id) => {
                write!(
                    f,
                    "session \"{id}\" is already owned by an active write handle"
                )
            }
            Self::ReadOnly { id, operation } => write!(
                f,
                "session \"{id}\": {operation} is not available on a read handle"
            ),
            Self::OwnershipLost(id) => write!(
                f,
                "session \"{id}\": write ownership was lost; close this handle and reopen"
            ),
            Self::HandleClosed { id, operation } => {
                write!(f, "session \"{id}\": {operation} on a closed handle")
            }
            Self::Corruption(message) | Self::Other(message) => f.write_str(message),
            Self::FormatUnsupported { message, .. } => f.write_str(message),
            Self::Io { context, source } => write!(f, "{context}: {source}"),
            Self::Aggregate { message, errors } => {
                write!(f, "{message}")?;
                for error in errors {
                    write!(f, "; {error}")?;
                }
                Ok(())
            }
        }
    }
}

impl Error for PersistenceError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Why a stored format version is refused: dsh's two wordings, one for a
/// log from a newer harness and one for an older format this build can't
/// upgrade. (dsh: `sessionFormatVersionRefusal`.)
pub fn format_version_refusal(id: &str, version: u64) -> String {
    if version > SESSION_FORMAT_VERSION {
        format!(
            "session \"{id}\" uses log format v{version}, but this harness reads only \
             v{SESSION_FORMAT_VERSION}: the log was written by a newer harness — upgrade the \
             harness to open it"
        )
    } else {
        format!(
            "session \"{id}\" uses log format v{version}, older than the supported \
             v{SESSION_FORMAT_VERSION}, and this build ships no upgrade path for it"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refusals_name_the_direction_and_the_raw_log() {
        assert!(format_version_refusal("s", 42).contains("upgrade the harness"));
        assert!(format_version_refusal("s", 3).contains("older than the supported v4"));
        let refusal = PersistenceError::unsupported(
            "no",
            Some(SessionLocation {
                kind: "jsonl",
                path: PathBuf::from("/r/s.jsonl"),
            }),
        );
        assert_eq!(refusal.to_string(), "no (raw log: /r/s.jsonl)");
    }
}
