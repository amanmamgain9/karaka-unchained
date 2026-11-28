//! The protocol's constants and errors, as dsh's MCP SDK has them.
//! (`@modelcontextprotocol/client` 2.2.0 and `@modelcontextprotocol/core`
//! 2.2.0.)

use std::fmt;

use serde_json::{Value, json};

/// The newest 2025-era revision, the one `initialize` offers. (SDK:
/// `LATEST_PROTOCOL_VERSION`.)
pub const LATEST_PROTOCOL_VERSION: &str = "2025-11-25";

/// The 2025-era revisions a server may answer `initialize` with, newest
/// first. (SDK: `SUPPORTED_PROTOCOL_VERSIONS`.)
pub const SUPPORTED_PROTOCOL_VERSIONS: [&str; 5] = [
    "2025-11-25",
    "2025-06-18",
    "2025-03-26",
    "2024-11-05",
    "2024-10-07",
];

/// The first revision of the modern era, negotiated by `server/discover`.
/// (SDK: `FIRST_MODERN_PROTOCOL_VERSION` and
/// `SUPPORTED_MODERN_PROTOCOL_VERSIONS`.)
pub const MODERN_PROTOCOL_VERSION: &str = "2026-07-28";

/// A request's default timeout, and the probe's. (SDK:
/// `DEFAULT_REQUEST_TIMEOUT_MSEC`.)
pub const DEFAULT_REQUEST_TIMEOUT_MS: u64 = 60_000;

/// The most a stdio server's unread output may hold. (SDK:
/// `STDIO_DEFAULT_MAX_BUFFER_SIZE`.)
pub const STDIO_MAX_BUFFER_BYTES: usize = 10 * 1024 * 1024;

/// dsh's client identity: `{ name: 'dsh-mcp-client', version: '0.0.1' }`.
pub const CLIENT_NAME: &str = "dsh-mcp-client";
pub const CLIENT_VERSION: &str = "0.0.1";

/// JSON-RPC's error codes, and MCP's own.
pub mod code {
    pub const PARSE_ERROR: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL_ERROR: i64 = -32603;
    pub const UNSUPPORTED_PROTOCOL_VERSION: i64 = -32022;
}

/// The `_meta` keys of a modern request's envelope.
pub mod meta {
    pub const PROTOCOL_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
    pub const CLIENT_INFO: &str = "io.modelcontextprotocol/clientInfo";
    pub const CLIENT_CAPABILITIES: &str = "io.modelcontextprotocol/clientCapabilities";
    pub const SERVER_INFO: &str = "io.modelcontextprotocol/serverInfo";
    pub const SUBSCRIPTION_ID: &str = "io.modelcontextprotocol/subscriptionId";
}

/// The requests a modern connection may send. (SDK: the 2026 wire's request
/// set.)
pub const MODERN_METHODS: [&str; 10] = [
    "tools/call",
    "tools/list",
    "prompts/get",
    "prompts/list",
    "resources/list",
    "resources/templates/list",
    "resources/read",
    "completion/complete",
    "server/discover",
    "subscriptions/listen",
];

/// The client's identity, as `clientInfo`.
pub fn client_info() -> Value {
    json!({ "name": CLIENT_NAME, "version": CLIENT_VERSION })
}

/// A modern request's envelope. (SDK: `outboundEnvelope`.)
pub fn envelope() -> Value {
    json!({
        meta::PROTOCOL_VERSION: MODERN_PROTOCOL_VERSION,
        meta::CLIENT_INFO: client_info(),
        meta::CLIENT_CAPABILITIES: {},
    })
}

/// What kind of failure an [`McpError`] is: the server's JSON-RPC error, or
/// one the client raised itself. (SDK: `ProtocolError` and `SdkErrorCode`.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// The server answered with a JSON-RPC error of this code.
    Protocol(i64),
    /// No transport, or not started. (`NOT_CONNECTED`.)
    NotConnected,
    /// The request timed out, or was cancelled. (`REQUEST_TIMEOUT`, which
    /// the SDK uses for both.)
    RequestTimeout,
    /// The connection closed with the request pending.
    /// (`CONNECTION_CLOSED`.)
    ConnectionClosed,
    /// Version negotiation failed. (`ERA_NEGOTIATION_FAILED`.)
    EraNegotiationFailed,
    /// A method the negotiated era has no place for.
    /// (`METHOD_NOT_SUPPORTED_BY_PROTOCOL_VERSION`.)
    MethodNotSupportedByProtocolVersion,
    /// An HTTP endpoint answered with this status.
    Http(u16),
    /// The transport failed: a spawn, a write, a network error.
    Transport,
    /// Anything else, such as an `initialize` answer that doesn't fit.
    Other,
}

/// A failed request, connection or transport. Its message is the SDK's,
/// and for a server's error exactly the server's `error.message`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpError {
    pub kind: ErrorKind,
    pub message: String,
    /// A JSON-RPC error's `data`, or an HTTP error's body.
    pub data: Option<Value>,
}

impl McpError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            data: None,
        }
    }

    pub fn other(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Other, message)
    }

    pub fn transport(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Transport, message)
    }

    /// The server's JSON-RPC error. (SDK: `ProtocolError.fromError`.)
    pub fn protocol(error: &Value) -> Self {
        let code = error
            .get("code")
            .and_then(Value::as_i64)
            .unwrap_or(code::INTERNAL_ERROR);
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        Self {
            kind: ErrorKind::Protocol(code),
            message,
            data: error.get("data").cloned(),
        }
    }

    pub(crate) fn not_connected() -> Self {
        Self::new(ErrorKind::NotConnected, "Not connected")
    }

    pub(crate) fn closed() -> Self {
        Self::new(ErrorKind::ConnectionClosed, "Connection closed")
    }

    /// The server's error code, for a [`ErrorKind::Protocol`] error.
    pub fn code(&self) -> Option<i64> {
        match self.kind {
            ErrorKind::Protocol(code) => Some(code),
            _ => None,
        }
    }

    /// The name JavaScript's `String(error)` puts before the message:
    /// `SdkError` for the client's own, `ProtocolError` for a server's.
    fn name(&self) -> &'static str {
        match self.kind {
            ErrorKind::Protocol(_) => "ProtocolError",
            ErrorKind::Http(_) => "SdkHttpError",
            ErrorKind::Transport | ErrorKind::Other => "Error",
            _ => "SdkError",
        }
    }

    /// The error as JavaScript's `String(error)` shows it, `Name: message`,
    /// as dsh's supervisor logs it.
    pub fn display_with_name(&self) -> String {
        format!("{}: {}", self.name(), self.message)
    }
}

impl fmt::Display for McpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for McpError {}
