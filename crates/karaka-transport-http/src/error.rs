//! Errors, and how the routes expose them: a code from a fixed set, a
//! fixed message, never the internal one. (Karaka: `exposeError` in
//! `transport-http/src/index.ts`.)

use std::fmt;

use karaka_identity::IdentityError;

/// An operation's failure: an internal code, which decides what is
/// exposed, and an internal message, which never is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppError {
    pub code: Option<String>,
    pub message: String,
}

impl AppError {
    pub fn coded(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: Some(code.into()),
            message: message.into(),
        }
    }

    /// An error with no code, exposed as `INTERNAL_ERROR`.
    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            code: None,
            message: message.into(),
        }
    }

    /// A request that doesn't parse or fit. (Karaka: `badRequest`, and
    /// `ZodError`.)
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::coded("BAD_REQUEST", message)
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for AppError {}

impl From<IdentityError> for AppError {
    fn from(error: IdentityError) -> Self {
        Self::coded(error.code.as_str(), error.message)
    }
}

/// What a client is told: the status, the code and the message. (Karaka:
/// `exposeError`.)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Exposed {
    pub status: u16,
    pub code: &'static str,
    pub message: &'static str,
}

/// (Karaka: `exposeError`, its table.)
pub fn expose(error: &AppError) -> Exposed {
    let exposed = |status, code, message| Exposed {
        status,
        code,
        message,
    };
    match error.code.as_deref() {
        Some("forbidden") => exposed(403, "CHAT_FORBIDDEN", "Chat access is forbidden"),
        Some("unavailable") => exposed(503, "CHAT_UNAVAILABLE", "Chat storage is unavailable"),
        Some("CHAT_FORBIDDEN") => exposed(403, "CHAT_FORBIDDEN", "Chat access is forbidden"),
        Some("SESSION_QUERY_SESSION_NOT_FOUND" | "session/not-found") => {
            exposed(404, "CHAT_NOT_FOUND", "Chat not found")
        }
        Some("agent-preset/not-found") => exposed(404, "AGENT_NOT_FOUND", "Agent not found"),
        Some("session/conflict" | "agent-preset/conflict" | "session/agent-busy") => exposed(
            409,
            "CHAT_CONFLICT",
            "Chat state conflicts with this operation",
        ),
        Some(
            "BAD_REQUEST"
            | "gateway/bad-request"
            | "session/model-unavailable"
            | "session/attachment-invalid"
            | "agent-preset/invalid",
        ) => exposed(400, "BAD_REQUEST", "Invalid request"),
        _ => exposed(500, "INTERNAL_ERROR", "Internal server error"),
    }
}
