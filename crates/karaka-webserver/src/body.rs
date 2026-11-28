//! Response bodies: whole, empty, or written as they happen, as an SSE
//! stream is. (dsh: a route writes `ServerResponse` itself; here it returns
//! a response whose body is one of these.)

use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use http_body::{Frame, SizeHint};
use http_body_util::{BodyExt, Empty, Full};
use tokio::sync::mpsc;

use crate::BoxError;

/// Any response body.
pub type Body = http_body_util::combinators::BoxBody<Bytes, BoxError>;

/// An empty body.
pub fn empty() -> Body {
    Empty::<Bytes>::new()
        .map_err(|never| match never {})
        .boxed()
}

/// A body of `bytes`, whole.
pub fn full(bytes: impl Into<Bytes>) -> Body {
    Full::new(bytes.into())
        .map_err(|never| match never {})
        .boxed()
}

/// A body written through the returned sender, one chunk at a time, each
/// sent as soon as the connection takes it: an SSE stream's. The body ends
/// when every sender is dropped. `capacity` chunks may wait unsent; a full
/// queue makes `send` wait, as a Node response's `drain` does.
pub fn channel(capacity: usize) -> (BodySender, Body) {
    let (sender, receiver) = mpsc::channel(capacity.max(1));
    (BodySender { sender }, ChannelBody { receiver }.boxed())
}

/// Writes a [`channel`] body.
#[derive(Clone, Debug)]
pub struct BodySender {
    sender: mpsc::Sender<Result<Bytes, BoxError>>,
}

/// The peer is gone: the response was dropped, its connection closed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Closed;

impl std::fmt::Display for Closed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the response is closed")
    }
}

impl std::error::Error for Closed {}

impl BodySender {
    /// Sends one chunk, waiting while the queue is full.
    ///
    /// # Errors
    ///
    /// The response is closed.
    pub async fn send(&self, chunk: impl Into<Bytes>) -> Result<(), Closed> {
        self.sender.send(Ok(chunk.into())).await.map_err(|_| Closed)
    }

    /// Ends the body with an error: the connection is cut, as dsh destroys
    /// a response whose headers are out.
    pub async fn abort(self, error: BoxError) {
        let _ = self.sender.send(Err(error)).await;
    }

    /// Resolves once the response is closed.
    pub async fn closed(&self) {
        self.sender.closed().await;
    }

    pub fn is_closed(&self) -> bool {
        self.sender.is_closed()
    }
}

struct ChannelBody {
    receiver: mpsc::Receiver<Result<Bytes, BoxError>>,
}

impl http_body::Body for ChannelBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        self.receiver
            .poll_recv(cx)
            .map(|chunk| chunk.map(|chunk| chunk.map(Frame::data)))
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::default()
    }
}
