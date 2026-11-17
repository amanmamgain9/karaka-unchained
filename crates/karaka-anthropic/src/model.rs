//! [`AnthropicModel`], which sends requests to Anthropic over HTTP and reads
//! back the streamed answer.

use std::error::Error;
use std::fmt;
use std::time::Duration;

use futures_util::stream::unfold;
use karaka_model::{ErrorKind, Event, EventStream, Model, ModelError, Request};
use karaka_sse::Sse;
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderValue, RETRY_AFTER};
use reqwest::{Client, RequestBuilder, Response, StatusCode};
use serde::Deserialize;

use crate::decoder::{Decoder, WireError, error_kind};
use crate::request::body;

const BASE_URL: &str = "https://api.anthropic.com";

/// The version of the Messages API this crate speaks. This version rejects
/// `temperature`, `top_p` and `top_k`, which is why [`Request`] has none.
const API_VERSION: &str = "2026-10-21";

/// Used when a request leaves `max_tokens` to the provider. Anthropic
/// requires a value, and since the answer is streamed, a long one doesn't
/// run into HTTP timeouts, so a high default costs nothing.
const DEFAULT_MAX_TOKENS: u32 = 64_000;

/// A [`Model`] backed by Anthropic's Messages API.
///
/// Cheap to share: every call reuses the same HTTP client, and so the same
/// pool of open connections.
pub struct AnthropicModel {
    client: Client,
    /// The full URL requests go to, such as
    /// `https://api.anthropic.com/v1/messages`.
    endpoint: String,
    api_key: String,
    default_max_tokens: u32,
}

impl AnthropicModel {
    /// A model that talks to Anthropic's own servers with `api_key`.
    pub fn new(api_key: impl Into<String>) -> Self {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(10))
            // There's no limit on the whole call, because an answer can
            // stream for many minutes. Anthropic sends pings while it
            // works, though, so two minutes of silence means the connection
            // is dead.
            .read_timeout(Duration::from_secs(120))
            .build()
            .expect("the HTTP client fails to build only if TLS can't start");
        Self {
            client,
            endpoint: endpoint(BASE_URL),
            api_key: api_key.into(),
            default_max_tokens: DEFAULT_MAX_TOKENS,
        }
    }

    /// Sends requests to another server, such as a proxy or a test server.
    /// `url` is the part before `/v1/messages`.
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.endpoint = endpoint(&url.into());
        self
    }

    /// Changes the `max_tokens` used when a request doesn't set its own.
    pub fn with_default_max_tokens(mut self, max_tokens: u32) -> Self {
        self.default_max_tokens = max_tokens;
        self
    }

    /// Everything about the HTTP request except sending it.
    fn prepare(&self, request: &Request) -> Result<RequestBuilder, ModelError> {
        let json =
            serde_json::to_vec(&body(request, self.default_max_tokens)?).map_err(|error| {
                ModelError::new(
                    ErrorKind::InvalidRequest,
                    format!("the request couldn't be written as JSON: {error}"),
                )
            })?;
        let mut api_key = HeaderValue::from_str(&self.api_key).map_err(|_| {
            ModelError::new(
                ErrorKind::Auth,
                "the API key has characters that can't go in an HTTP header",
            )
        })?;
        // Keeps the key out of reqwest's debug output.
        api_key.set_sensitive(true);

        Ok(self
            .client
            .post(&self.endpoint)
            .header("x-api-key", api_key)
            .header("anthropic-version", API_VERSION)
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "text/event-stream")
            .body(json))
    }
}

/// Leaves out the API key, so the model can sit inside other types that
/// derive `Debug` without the key ending up in a log.
impl fmt::Debug for AnthropicModel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AnthropicModel")
            .field("endpoint", &self.endpoint)
            .field("default_max_tokens", &self.default_max_tokens)
            .finish_non_exhaustive()
    }
}

impl Model for AnthropicModel {
    fn stream(&self, request: Request) -> EventStream {
        // The body is written out now, while `request` is still here to
        // borrow. The stream then owns plain bytes, so it can be `'static`.
        // A request builder only describes the request; nothing goes out
        // until it is sent, on the stream's first poll.
        let start = match self.prepare(&request) {
            Ok(builder) => State::Send(builder),
            Err(error) => State::Failed(error),
        };
        // `unfold` turns one async step into a stream: each poll runs `step`
        // on the current state until it gives back an item and the next
        // state, or `None` to end the stream.
        Box::pin(unfold(start, step))
    }
}

/// Where a call has got to.
enum State {
    /// The call failed before it was sent. The error is the only item.
    Failed(ModelError),
    /// Ready to send.
    Send(RequestBuilder),
    /// The answer is streaming in.
    Reading(Reading),
    /// Nothing more to yield.
    Finished,
}

/// An answer being read: the HTTP response, and the two stages that turn
/// its bytes into events.
struct Reading {
    response: Response,
    sse: Sse,
    decoder: Decoder,
}

/// One item out of the stream, and the state to carry on from.
type Step = Option<(Result<Event, ModelError>, State)>;

async fn step(state: State) -> Step {
    match state {
        State::Failed(error) => Some((Err(error), State::Finished)),
        State::Send(builder) => {
            let response = match builder.send().await {
                Ok(response) => response,
                Err(error) => return Some((Err(transport(&error)), State::Finished)),
            };
            if !response.status().is_success() {
                return Some((Err(http_error(response).await), State::Finished));
            }
            read(Reading {
                response,
                sse: Sse::default(),
                decoder: Decoder::default(),
            })
            .await
        }
        State::Reading(reading) => read(reading).await,
        State::Finished => None,
    }
}

/// Reads until the next event, pulling more of the body only when no
/// complete event is waiting.
async fn read(mut reading: Reading) -> Step {
    loop {
        while let Some(data) = reading.sse.next_event() {
            match reading.decoder.feed(&data) {
                // Pings and the like make no event of their own.
                Ok(None) => {}
                Ok(Some(event)) => {
                    // `Done` is always last, so anything the server sends
                    // after it is never read.
                    let next = if matches!(event, Event::Done { .. }) {
                        State::Finished
                    } else {
                        State::Reading(reading)
                    };
                    return Some((Ok(event), next));
                }
                Err(error) => return Some((Err(error), State::Finished)),
            }
        }
        match reading.response.chunk().await {
            Ok(Some(chunk)) => reading.sse.push(&chunk),
            Ok(None) => {
                let error = ModelError::new(
                    ErrorKind::Network,
                    "the stream ended before the model finished",
                );
                return Some((Err(error), State::Finished));
            }
            Err(error) => return Some((Err(transport(&error)), State::Finished)),
        }
    }
}

/// The error behind a response that isn't a success.
async fn http_error(response: Response) -> ModelError {
    let status = response.status();
    // Anthropic gives a whole number of seconds. The header may also hold a
    // date, which isn't worth parsing here.
    let retry_after = response
        .headers()
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse().ok())
        .map(Duration::from_secs);
    let text = response.text().await.unwrap_or_default();

    // Anthropic explains a failure as `{"type": "error", "error": {...}}`.
    // Anything else, such as a proxy's HTML page, gets the status code.
    #[derive(Deserialize)]
    struct ErrorBody {
        error: WireError,
    }
    let mut error = match serde_json::from_str::<ErrorBody>(&text) {
        Ok(body) => ModelError::new(error_kind(&body.error.kind), body.error.message),
        Err(_) => {
            let text = text.trim();
            let message = if text.is_empty() {
                format!("HTTP {status}")
            } else {
                format!("HTTP {status}: {text}")
            };
            ModelError::new(status_kind(status), message)
        }
    };
    error.retry_after = retry_after;
    error
}

/// The kind of failure a status code means, for responses that don't say.
fn status_kind(status: StatusCode) -> ErrorKind {
    match status.as_u16() {
        401..=403 => ErrorKind::Auth,
        400 | 404 | 413 => ErrorKind::InvalidRequest,
        429 => ErrorKind::RateLimited,
        _ => ErrorKind::Unavailable,
    }
}

/// An error from reqwest itself, rather than from Anthropic.
fn transport(error: &reqwest::Error) -> ModelError {
    // reqwest's own message is only the outline, such as "error sending
    // request". The reason, such as "connection refused", is further down
    // the chain of sources.
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    // A builder error means the request couldn't be put together at all,
    // for example because the base URL isn't a URL. Trying again won't help.
    let kind = if error.is_builder() {
        ErrorKind::InvalidRequest
    } else {
        ErrorKind::Network
    };
    ModelError::new(kind, message)
}

fn endpoint(base_url: &str) -> String {
    format!("{}/v1/messages", base_url.trim_end_matches('/'))
}

#[cfg(test)]
mod tests {
    use karaka_model::{Message, Model, Request};

    use super::{AnthropicModel, endpoint};

    #[test]
    fn a_trailing_slash_on_the_base_url_is_dropped() {
        assert_eq!(
            endpoint("http://localhost:8080/"),
            "http://localhost:8080/v1/messages"
        );
    }

    #[test]
    fn debug_output_leaves_out_the_api_key() {
        let model = AnthropicModel::new("sk-secret");
        assert!(!format!("{model:?}").contains("sk-secret"));
    }

    /// Compiles only if a call can move between threads. Making one needs
    /// no async runtime either: that only matters once it's polled.
    #[test]
    fn a_call_works_across_threads() {
        fn assert_send<T: Send>(_: T) {}

        let model = AnthropicModel::new("test-key");
        assert_send(model.stream(Request::new("test-model", vec![Message::user("hi")])));
    }
}
