//! Why a request or a policy switch failed. (dsh: the `Error`s
//! `ApprovalService.request` and `setPolicy` throw.)

use std::error::Error;
use std::fmt;

use karaka_agent::InboxError;
use karaka_session::{LogError, SessionId};

/// dsh's message for a request outside an open turn.
pub const OUTSIDE_TURN: &str = "approval.request() outside an open turn: the approval/asked + \
     approval/decided audit pair must be turn-enclosed (a bare event between turns is \
     crash-tail garbage on reload). Ask from inside the turn that needs the decision.";

#[derive(Clone, Debug, PartialEq)]
pub enum ApprovalError {
    /// The agent's session has no open turn, so the audit pair can't be
    /// enclosed by one. Nothing was logged.
    OutsideTurn,
    /// The store holds no session for the agent. dsh's agent always
    /// carries its session.
    NoSession(SessionId),
    /// The log refused an audit or policy event.
    Log(LogError),
    /// The agent refused the policy switch's notice.
    Inbox(InboxError),
}

impl fmt::Display for ApprovalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutsideTurn => f.write_str(OUTSIDE_TURN),
            Self::NoSession(id) => write!(f, "approval: no session \"{id}\""),
            Self::Log(error) => error.fmt(f),
            Self::Inbox(error) => error.fmt(f),
        }
    }
}

impl Error for ApprovalError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Log(error) => Some(error),
            Self::Inbox(error) => Some(error),
            Self::OutsideTurn | Self::NoSession(_) => None,
        }
    }
}

impl From<LogError> for ApprovalError {
    fn from(error: LogError) -> Self {
        Self::Log(error)
    }
}

impl From<InboxError> for ApprovalError {
    fn from(error: InboxError) -> Self {
        Self::Inbox(error)
    }
}
