//! [`OpenAiModel`], which sends requests to an OpenAI-compatible server over
//! HTTP and reads back the streamed answer.

use std::collections::VecDeque;
use std::error::Error;
use std::fmt;
use std::time::{Duration, SystemTime};

use futures_util::stream::unfold;
use karaka_model::{
    Effort, ErrorKind, Event, EventStream, Model, ModelError, Request, parse_retry_after,
};
use karaka_sse::Sse;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderValue, RETRY_AFTER};
use reqwest::{Client, RequestBuilder, Response};
use serde::Deserialize;

use crate::decoder::{Decoder, WireError, classify};
use crate::request::{MaxTokensField, Settings, body};

/// A [`Model`] backed by an OpenAI-compatible Chat Completions API.
///
/// There's no default server, because there's no single right one. The
/// base URL is the part before `/chat/completions`:
///
/// - OpenAI: `https://api.openai.com/v1`
/// - DeepSeek: `https://api.deepseek.com`
/// - Ollama: `http://localhost:11434/v1`
///
/// DeepSeek works here, but dsh v4 talks to it in the Messages format, so
/// `karaka-anthropic`'s DeepSeek profile is the one that matches dsh.
///
/// ```no_run
/// let model = karaka_openai::OpenAiModel::new("https://api.deepseek.com")
///     .with_api_key("sk-...");
/// ```
///
/// Cheap to share: every call reuses the same HTTP client, and so the same
/// pool of open connections.
pub struct OpenAiModel {
    client: Client,
    /// The full URL requests go to, such as
    /// `https://api.openai.com/v1/chat/completions`.
    endpoint: String,
    /// `None` for a local server that needs no key.
    api_key: Option<String>,
    settings: Settings,
}

impl OpenAiModel {
    /// A model that talks to the server at `base_url`, with no API key.
    pub fn new(base_url: impl Into<String>) -> Self {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(10))
            // There's no limit on the whole call, because an answer can
            // stream for many minutes, and a local model can take a while
            // before its first token. The answer is streamed, though, so
            // two minutes without a single byte means the connection is
            // dead.
            .read_timeout(Duration::from_secs(120))
            .build()
            .expect("the HTTP client fails to build only if TLS can't start");
        Self {
            client,
            endpoint: endpoint(&base_url.into()),
            api_key: None,
            settings: Settings::default(),
        }
    }

    /// Sends `key` with every request, as `Authorization: Bearer <key>`.
    pub fn with_api_key(mut self, key: impl Into<String>) -> Self {
        self.api_key = Some(key.into());
        self
    }

    /// Sets the effort used when a request doesn't set its own. Without
    /// one, such a request sends no `reasoning_effort`.
    pub fn with_default_effort(mut self, effort: Effort) -> Self {
        self.settings.default_effort = Some(effort);
        self
    }

    /// Sends DeepSeek's `thinking` switch, on or off, with every request.
    /// Other servers don't know the field, so leave it unset for them.
    pub fn with_thinking(mut self, on: bool) -> Self {
        self.settings.thinking = Some(on);
        self
    }

    /// Chooses the field a request's `max_tokens` goes out as. The default
    /// is [`MaxTokensField::MaxTokens`].
    pub fn with_max_tokens_field(mut self, field: MaxTokensField) -> Self {
        self.settings.max_tokens_field = field;
        self
    }

    /// Sends system and developer messages from later in the history in
    /// place, as `{"role": "system"}` messages, instead of folding them
    /// into the system prompt. Only for a server whose model accepts them;
    /// off unless set, as in pi-ai.
    pub fn with_system_messages_in_history(mut self, on: bool) -> Self {
        self.settings.system_messages_in_history = on;
        self
    }

    /// Everything about the HTTP request except sending it.
    fn prepare(&self, request: &Request) -> Result<RequestBuilder, ModelError> {
        let json = serde_json::to_vec(&body(request, &self.settings)?).map_err(|error| {
            ModelError::new(
                ErrorKind::InvalidRequest,
                format!("the request couldn't be written as JSON: {error}"),
            )
        })?;
        let mut builder = self
            .client
            .post(&self.endpoint)
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "text/event-stream");
        if let Some(key) = &self.api_key {
            let mut value = HeaderValue::from_str(&format!("Bearer {key}")).map_err(|_| {
                ModelError::new(
                    ErrorKind::Auth,
                    "the API key has characters that can't go in an HTTP header",
                )
            })?;
            // Keeps the key out of reqwest's debug output.
            value.set_sensitive(true);
            builder = builder.header(AUTHORIZATION, value);
        }
        Ok(builder.body(json))
    }
}

/// Leaves out the API key, so the model can sit inside other types that
/// derive `Debug` without the key ending up in a log.
impl fmt::Debug for OpenAiModel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenAiModel")
            .field("endpoint", &self.endpoint)
            .field("has_api_key", &self.api_key.is_some())
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

impl Model for OpenAiModel {
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

/// An answer being read: the HTTP response, the two stages that turn its
/// bytes into events, and the events decoded but not yet yielded.
struct Reading {
    response: Response,
    sse: Sse,
    decoder: Decoder,
    /// One chunk can decode into several events, and the stream yields one
    /// at a time, so the rest wait here.
    pending: VecDeque<Event>,
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
                pending: VecDeque::new(),
            })
            .await
        }
        State::Reading(reading) => read(reading).await,
        State::Finished => None,
    }
}

/// Reads until the next event: first any already decoded, then any complete
/// chunk waiting in the buffer, and only then more of the body.
async fn read(mut reading: Reading) -> Step {
    loop {
        if let Some(event) = reading.pending.pop_front() {
            // `Done` is always last, so anything the server sends after it
            // is never read.
            let next = if matches!(event, Event::Done { .. }) {
                State::Finished
            } else {
                State::Reading(reading)
            };
            return Some((Ok(event), next));
        }
        let decoded = if let Some(data) = reading.sse.next_event() {
            reading.decoder.feed(&data)
        } else {
            match reading.response.chunk().await {
                Ok(Some(chunk)) => {
                    reading.sse.push(&chunk);
                    continue;
                }
                // Either finishes with `Done` or fails, so this branch is
                // never reached twice.
                Ok(None) => reading.decoder.end_of_body(),
                Err(error) => Err(transport(&error)),
            }
        };
        match decoded {
            Ok(events) => reading.pending.extend(events),
            Err(error) => return Some((Err(error), State::Finished)),
        }
    }
}

/// The error behind a response that isn't a success.
async fn http_error(response: Response) -> ModelError {
    let status = response.status();
    // Seconds, perhaps with a decimal part, or a date.
    let retry_after = response
        .headers()
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| parse_retry_after(value, SystemTime::now()));
    let text = response.text().await.unwrap_or_default();

    // OpenAI-style servers explain a failure as `{"error": {...}}`. Anything
    // else, such as a proxy's HTML page, gets the status code and the body.
    #[derive(Deserialize)]
    struct ErrorBody {
        error: WireError,
    }
    let wire = serde_json::from_str::<ErrorBody>(&text)
        .ok()
        .map(|body| body.error);

    // Without the error JSON, the body's text is all there is to read.
    let detail = wire
        .as_ref()
        .map_or_else(|| text.clone(), WireError::detail);
    let kind = classify(&detail, Some(status.as_u16()));
    let message = match wire.and_then(|wire| wire.message) {
        Some(message) => message,
        None => {
            let text = text.trim();
            if text.is_empty() {
                format!("HTTP {status}")
            } else {
                format!("HTTP {status}: {text}")
            }
        }
    };
    let mut error = ModelError::new(kind, message);
    error.retry_after = retry_after;
    error
}

/// An error from reqwest itself, rather than from the server.
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
    format!("{}/chat/completions", base_url.trim_end_matches('/'))
}

#[cfg(test)]
mod tests {
    use karaka_model::{ErrorKind, Message, Model, Request};

    use super::{OpenAiModel, endpoint};

    #[test]
    fn a_trailing_slash_on_the_base_url_is_dropped() {
        assert_eq!(
            endpoint("http://localhost:11434/v1/"),
            "http://localhost:11434/v1/chat/completions"
        );
    }

    #[test]
    fn debug_output_leaves_out_the_api_key() {
        let model = OpenAiModel::new("http://localhost").with_api_key("sk-secret");
        assert!(!format!("{model:?}").contains("sk-secret"));
    }

    #[test]
    fn a_key_that_cant_be_a_header_fails_before_sending() {
        let model = OpenAiModel::new("http://localhost").with_api_key("bad\nkey");
        let request = Request::new("test-model", vec![Message::user("hi")]);
        let Err(error) = model.prepare(&request) else {
            panic!("expected the request to fail");
        };
        assert_eq!(error.kind, ErrorKind::Auth);
    }

    /// Compiles only if a call can move between threads. Making one needs
    /// no async runtime either: that only matters once it's polled.
    #[test]
    fn a_call_works_across_threads() {
        fn assert_send<T: Send>(_: T) {}

        let model = OpenAiModel::new("http://localhost").with_api_key("test-key");
        assert_send(model.stream(Request::new("test-model", vec![Message::user("hi")])));
    }
}
