//! The transport seam: what carries JSON-RPC messages between the client and
//! one server. (SDK: `Transport`.)
//!
//! The client starts a transport with a channel, and the transport puts on
//! it every message the server sends, every error that isn't fatal, and
//! once the connection is gone, [`Inbound::Closed`]. Karaka ships two:
//! stdio (`stdio.rs`) and Streamable HTTP (`http.rs`). A caller with needs
//! of its own, such as an application whose HTTP requests carry
//! per-request headers, implements the trait and hands the supervisor a
//! factory of its own.

use std::future::Future;
use std::pin::Pin;

use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;

use crate::protocol::McpError;

/// What a transport's methods resolve to.
pub type TransportFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// What a transport puts on the client's channel. (SDK: `onmessage`,
/// `onerror` and `onclose`.)
#[derive(Clone, Debug, PartialEq)]
pub enum Inbound {
    /// A JSON-RPC message from the server.
    Message(Value),
    /// A failure that doesn't end the connection, such as a line that isn't
    /// a JSON-RPC message.
    Error(String),
    /// The connection is gone: the server exited, or the transport closed.
    /// It comes once, last.
    Closed,
}

/// How the client treats a transport: version negotiation and cancellation
/// differ between the two. (SDK: its structural check, `"stderr" in
/// transport && "pid" in transport ? "stdio" : "http"`.)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportKind {
    Stdio,
    Http,
}

/// One connection's channel to a server. A transport is started once; the
/// supervisor makes a fresh one for every attempt.
pub trait Transport: Send + Sync {
    fn kind(&self) -> TransportKind;

    /// Opens the channel, putting what arrives on `inbound`.
    ///
    /// # Errors
    ///
    /// The channel didn't open, such as a command that doesn't spawn. The
    /// transport then puts nothing more on `inbound`.
    fn start(&self, inbound: UnboundedSender<Inbound>)
    -> TransportFuture<'_, Result<(), McpError>>;

    /// Sends one message. An HTTP transport delivers the reply through
    /// `inbound`, and its error status as the error.
    ///
    /// # Errors
    ///
    /// Not started, or the message didn't go.
    fn send(&self, message: Value) -> TransportFuture<'_, Result<(), McpError>>;

    /// The revision the connection negotiated, for a transport that sends
    /// it with each message, as Streamable HTTP does in a header.
    fn set_protocol_version(&self, _version: &str) {}

    /// Closes the channel. [`Inbound::Closed`] follows once it is closed:
    /// for stdio, once the server's process has exited.
    ///
    /// # Errors
    ///
    /// The close failed; the caller can't count on the channel being shut.
    fn close(&self) -> TransportFuture<'_, Result<(), McpError>>;

    /// A disposable twin, made from the same parameters, for the
    /// `server/discover` probe, so the probe doesn't spend this transport's
    /// one life. The SDK makes one for its base stdio transport only, with
    /// the twin's stderr discarded; anything else probes in place.
    fn sibling(&self) -> Option<Box<dyn Transport>> {
        None
    }
}
