//! [`InboxError`]: why queuing input failed. (dsh: the errors
//! `ReactLoopInbox` throws, in `agent-loop/src/inbox.ts`.)

use std::error::Error;
use std::fmt;

use karaka_session::{LogError, Seq};

/// Why an agent couldn't queue input, or read what is queued.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InboxError {
    /// A message with this id is queued already. (dsh: "message "id" is
    /// already pending".)
    AlreadyPending(String),
    /// The session's history of queued input doesn't fold: the splice at
    /// `seq` doesn't fit what was queued before it. (dsh: "invalid
    /// persisted inbox splice at session seq N", with its cause.)
    Invalid { seq: Seq, cause: SpliceCause },
    /// The queue can't be read at all, such as with no inbox projection
    /// registered. The message is dsh's.
    Unavailable(String),
    /// The log refused the splice.
    Log(LogError),
}

/// Why a stored splice doesn't fold, in dsh's words, such as "invalid inbox
/// splice". The source of an [`InboxError::Invalid`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpliceCause(pub String);

impl fmt::Display for SpliceCause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Error for SpliceCause {}

impl fmt::Display for InboxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyPending(id) => write!(f, "message \"{id}\" is already pending"),
            Self::Invalid { seq, .. } => {
                write!(f, "invalid persisted inbox splice at session seq {seq}")
            }
            Self::Unavailable(message) => f.write_str(message),
            Self::Log(error) => error.fmt(f),
        }
    }
}

impl Error for InboxError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Invalid { cause, .. } => Some(cause),
            Self::Log(error) => Some(error),
            _ => None,
        }
    }
}

impl From<LogError> for InboxError {
    fn from(error: LogError) -> Self {
        Self::Log(error)
    }
}
