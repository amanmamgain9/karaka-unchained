//! An MCP client: the protocol, its two transports, and dsh's connection
//! supervisor. (dsh: `@deepseek-ai/dsh-mcp-client`, 0.2.1-rc.2, built on
//! `@modelcontextprotocol/client` 2.2.0.)
//!
//! - `protocol.rs`: the SDK's constants, and [`McpError`].
//! - `transport.rs`: the [`Transport`] seam.
//! - `stdio.rs`: a server as a child process ([`StdioTransport`]).
//! - `http.rs`: Streamable HTTP ([`HttpTransport`]).
//! - `client.rs`: one connection: version negotiation, the handshake,
//!   requests, and what the server sends unasked ([`Client`]).
//! - `connection.rs`: the supervisor: generations, reconnecting with
//!   backoff, and the server's instructions ([`start_connection`]).
//! - `tools.rs`: a server's tools as Karaka tools, under
//!   `mcp__<server>__<tool>` names, synced per generation ([`ToolBridge`]).
//! - `plugin.rs`: the plugin, global or in a preset, with each server's
//!   instructions as a prompt section ([`McpClientsPlugin`],
//!   [`McpServerPresetPlugin`]).
//!
//! A caller with its own tools, such as an application's catalog, drives a
//! connection through a [`ConnectionObserver`], and may bring its own
//! transport through a [`TransportFactory`].

mod client;
mod connection;
mod http;
mod plugin;
mod protocol;
mod stdio;
mod tools;
mod transport;

pub use client::{
    CLOSE_CONFIRM_TIMEOUT, Callback, Client, ClientOptions, Era, ErrorCallback, RequestOptions,
    ToolsPage,
};
pub use connection::{
    ConnectionConfig, ConnectionHandle, ConnectionObserver, DEFAULT_MAX_INSTRUCTION_BYTES,
    DEFAULT_TOOL_CALL_TIMEOUT_MS, Level, MAX_TIMER_DELAY_MS, Outcome, RECONNECT_DEFAULTS,
    ReconnectPolicy, SyncFuture, TransportConfig, TransportFactory, resolve_reconnect_policy,
    start_connection, transport_factory,
};
pub use http::{HeaderFuture, HeaderSource, HttpOptions, HttpParams, HttpTransport};
pub use plugin::{
    MCP_SERVERS_SECTION_ORDER, McpClients, McpClientsPlugin, McpServer, McpServerConfig,
    McpServerPresetPlugin, McpStartError,
};
pub use protocol::{
    CLIENT_NAME, CLIENT_VERSION, DEFAULT_REQUEST_TIMEOUT_MS, ErrorKind, LATEST_PROTOCOL_VERSION,
    MODERN_METHODS, MODERN_PROTOCOL_VERSION, McpError, STDIO_MAX_BUFFER_BYTES,
    SUPPORTED_PROTOCOL_VERSIONS, client_info, code, envelope, meta,
};
pub use stdio::{Stderr, StdioParams, StdioTransport, child_env};
pub use tools::{
    BridgeExtensions, Disposer, Log, McpTool, MetadataFuture, ToolBridge, extract_text,
    public_tool_name,
};
pub use transport::{Inbound, Transport, TransportFuture, TransportKind};
