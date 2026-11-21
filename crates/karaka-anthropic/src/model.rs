//! [`AnthropicModel`], which sends requests to Anthropic over HTTP and reads
//! back the streamed answer.

use std::collections::VecDeque;
use std::error::Error;
use std::fmt;
use std::time::{Duration, SystemTime};

use futures_util::stream::unfold;
use karaka_model::{
    Effort, ErrorKind, Event, EventStream, Model, ModelError, Request, parse_retry_after,
};
use karaka_sse::Sse;
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderValue, RETRY_AFTER};
use reqwest::redirect::Policy;
use reqwest::{Client, RequestBuilder, Response};
use serde::Deserialize;

use crate::decoder::{Decoder, WireError, classify};
use crate::request::{CacheMode, Settings, ThinkingDisplay, body};
use crate::{DeepSeekThinking, Profile, deepseek};

const BASE_URL: &str = "https://api.anthropic.com";

/// The version of the Messages API this crate speaks. This version rejects
/// `temperature`, `top_p` and `top_k`, which is why [`Request`] has none.
const API_VERSION: &str = "2026-10-21";

/// Used when a request leaves `max_tokens` to the provider. Anthropic
/// requires a value, and since the answer is streamed, a long one doesn't
/// run into HTTP timeouts, so a high default costs nothing.
const DEFAULT_MAX_TOKENS: u32 = 64_000;

/// A [`Model`] backed by Anthropic's Messages API, or by DeepSeek's.
///
/// Cheap to share: every call reuses the same HTTP client, and so the same
/// pool of open connections.
pub struct AnthropicModel {
    client: Client,
    /// The full URL requests go to, such as
    /// `https://api.anthropic.com/v1/messages`.
    endpoint: String,
    api_key: String,
    settings: ProfileSettings,
}

/// The settings for the profile in use. Each profile writes its own body
/// from its own settings.
#[derive(Debug)]
enum ProfileSettings {
    Anthropic(Settings),
    DeepSeek(deepseek::Settings),
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
            settings: ProfileSettings::Anthropic(Settings {
                default_max_tokens: DEFAULT_MAX_TOKENS,
                default_effort: None,
                thinking_display: None,
                cache: CacheMode::default(),
            }),
        }
    }

    /// A model that talks to DeepSeek's Messages API with `api_key`, in the
    /// DeepSeek profile: thinking on, effort `high` and `max_tokens`
    /// 256,000 unless set, as in dsh.
    pub fn deepseek(api_key: impl Into<String>) -> Self {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(deepseek::IDLE_TIMEOUT)
            // dsh refuses redirects, so the API key never reaches a server
            // it wasn't meant for (`adapter.ts:139`). reqwest would follow
            // them and send the key along.
            .redirect(Policy::none())
            .build()
            .expect("the HTTP client fails to build only if TLS can't start");
        Self {
            client,
            endpoint: deepseek::endpoint(deepseek::BASE_URL),
            api_key: api_key.into(),
            settings: ProfileSettings::DeepSeek(deepseek::Settings::default()),
        }
    }

    /// Which profile this model speaks.
    pub fn profile(&self) -> Profile {
        match self.settings {
            ProfileSettings::Anthropic(_) => Profile::Anthropic,
            ProfileSettings::DeepSeek(_) => Profile::DeepSeek,
        }
    }

    /// Sends requests to another server, such as a proxy or a test server.
    /// `url` is the part before `/v1/messages`. The DeepSeek profile, like
    /// dsh, doesn't add a `/v1` that `url` already ends in.
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        let url = url.into();
        self.endpoint = match self.settings {
            ProfileSettings::Anthropic(_) => endpoint(&url),
            ProfileSettings::DeepSeek(_) => deepseek::endpoint(&url),
        };
        self
    }

    /// Changes the `max_tokens` used when a request doesn't set its own.
    pub fn with_default_max_tokens(mut self, max_tokens: u32) -> Self {
        match &mut self.settings {
            ProfileSettings::Anthropic(settings) => settings.default_max_tokens = max_tokens,
            ProfileSettings::DeepSeek(settings) => settings.default_max_tokens = max_tokens,
        }
        self
    }

    /// Sets the effort used when a request doesn't set its own. Without
    /// one, such a request sends no effort and gets the model's default; in
    /// the DeepSeek profile it sends `high`, as dsh does.
    pub fn with_default_effort(mut self, effort: Effort) -> Self {
        match &mut self.settings {
            ProfileSettings::Anthropic(settings) => settings.default_effort = Some(effort),
            ProfileSettings::DeepSeek(settings) => settings.default_effort = Some(effort),
        }
        self
    }

    /// Turns on adaptive thinking, returned as `display` says. Without it no
    /// thinking setting is sent, and each model does what it does by
    /// default.
    ///
    /// Anthropic only. DeepSeek's thinking is a plain switch, set with
    /// [`AnthropicModel::with_deepseek_thinking`], so the DeepSeek profile
    /// ignores this.
    pub fn with_thinking_display(mut self, display: ThinkingDisplay) -> Self {
        if let ProfileSettings::Anthropic(settings) = &mut self.settings {
            settings.thinking_display = Some(display);
        }
        self
    }

    /// Changes how long the start of a conversation stays cached, or turns
    /// caching off. The default is [`CacheMode::Short`].
    ///
    /// Anthropic only. The DeepSeek profile sends no cache markers, as dsh
    /// sends none, so it ignores this.
    pub fn with_cache(mut self, cache: CacheMode) -> Self {
        if let ProfileSettings::Anthropic(settings) = &mut self.settings {
            settings.cache = cache;
        }
        self
    }

    /// Turns DeepSeek's thinking on or off. It is on unless set, as in dsh.
    /// With it off, a request with an effort is refused.
    ///
    /// DeepSeek only; the Anthropic profile ignores it.
    pub fn with_deepseek_thinking(mut self, thinking: DeepSeekThinking) -> Self {
        if let ProfileSettings::DeepSeek(settings) = &mut self.settings {
            settings.thinking = thinking;
        }
        self
    }

    /// Everything about the HTTP request except sending it.
    fn prepare(&self, request: &Request) -> Result<RequestBuilder, ModelError> {
        // The body, and the headers that go with it, differ by profile.
        let (json, version, beta) = match &self.settings {
            ProfileSettings::Anthropic(settings) => {
                let body = body(request, settings)?;
                (to_json(&body)?, API_VERSION, body.beta())
            }
            // DeepSeek needs no beta for anything Karaka sends. dsh sends
            // one only for files and for tool changes (`adapter.ts:132-137`).
            ProfileSettings::DeepSeek(settings) => {
                let body = deepseek::body(request, settings)?;
                (to_json(&body)?, deepseek::API_VERSION, None)
            }
        };
        let mut api_key = HeaderValue::from_str(&self.api_key).map_err(|_| {
            ModelError::new(
                ErrorKind::Auth,
                "the API key has characters that can't go in an HTTP header",
            )
        })?;
        // Keeps the key out of reqwest's debug output.
        api_key.set_sensitive(true);

        // DeepSeek takes the key as `x-api-key` too (`llm-deepseek-api-key/
        // src/index.ts:41`). dsh also sends its own user agent and
        // `x-deepseek-harness-*` headers that identify dsh and its user
        // (`adapter.ts:141-148`). Those name dsh, not Karaka, and DeepSeek's
        // API doesn't need them, so they are left out.
        let mut builder = self
            .client
            .post(&self.endpoint)
            .header("x-api-key", api_key)
            .header("anthropic-version", version)
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "text/event-stream");
        if let Some(beta) = beta {
            builder = builder.header("anthropic-beta", beta);
        }
        Ok(builder.body(json))
    }

    /// A decoder for the answer to `request`.
    fn decoder(&self, request: &Request) -> Decoder {
        match self.settings {
            ProfileSettings::Anthropic(_) => Decoder::anthropic(request.model.clone()),
            ProfileSettings::DeepSeek(_) => Decoder::deepseek(request.model.clone()),
        }
    }
}

/// `body` written out as JSON.
fn to_json(body: &impl serde::Serialize) -> Result<Vec<u8>, ModelError> {
    serde_json::to_vec(body).map_err(|error| {
        ModelError::new(
            ErrorKind::InvalidRequest,
            format!("the request couldn't be written as JSON: {error}"),
        )
    })
}

/// Leaves out the API key, so the model can sit inside other types that
/// derive `Debug` without the key ending up in a log.
impl fmt::Debug for AnthropicModel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AnthropicModel")
            .field("endpoint", &self.endpoint)
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

impl Model for AnthropicModel {
    /// The provider in the origin of every signature this model receives:
    /// `anthropic`, or `deepseek` for the DeepSeek profile. See
    /// `Decoder::anthropic` and `Decoder::deepseek`.
    fn provider(&self) -> &str {
        match self.profile() {
            Profile::Anthropic => crate::request::PROVIDER,
            Profile::DeepSeek => deepseek::PROVIDER,
        }
    }

    fn stream(&self, request: Request) -> EventStream {
        // The body is written out now, while `request` is still here to
        // borrow. The stream then owns plain bytes, so it can be `'static`.
        // A request builder only describes the request; nothing goes out
        // until it is sent, on the stream's first poll.
        let start = match self.prepare(&request) {
            Ok(builder) => State::Send(builder, self.decoder(&request)),
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
    /// Ready to send, with the decoder for the answer.
    Send(RequestBuilder, Decoder),
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
    /// One event can decode into several, when held blocks are released at
    /// the end, and the stream yields one at a time, so the rest wait here.
    pending: VecDeque<Event>,
}

/// One item out of the stream, and the state to carry on from.
type Step = Option<(Result<Event, ModelError>, State)>;

async fn step(state: State) -> Step {
    match state {
        State::Failed(error) => Some((Err(error), State::Finished)),
        State::Send(builder, decoder) => {
            let response = match builder.send().await {
                Ok(response) => response,
                Err(error) => return Some((Err(transport(&error)), State::Finished)),
            };
            if !response.status().is_success() {
                let error = http_error(response, decoder.profile()).await;
                return Some((Err(error), State::Finished));
            }
            read(Reading {
                response,
                sse: Sse::default(),
                decoder,
                pending: VecDeque::new(),
            })
            .await
        }
        State::Reading(reading) => read(reading).await,
        State::Finished => None,
    }
}

/// Reads until the next event: first any already decoded, then any complete
/// event waiting in the buffer, and only then more of the body.
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
        if let Some(data) = reading.sse.next_event() {
            // Pings and the like decode into no events, and the loop goes on.
            match reading.decoder.feed(&data) {
                Ok(events) => reading.pending.extend(events),
                Err(error) => return Some((Err(error), State::Finished)),
            }
            continue;
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
async fn http_error(response: Response, profile: Profile) -> ModelError {
    let status = response.status();
    // The DeepSeek profile follows no redirects. dsh's `fetch` fails on one
    // as a transport error (`adapter.ts:139,80`), so this is a network one.
    if profile == Profile::DeepSeek && status.is_redirection() {
        return ModelError::new(
            ErrorKind::Network,
            format!(
                "DeepSeek answered HTTP {status}, a redirect, which isn't followed so the API key stays with the server it was meant for"
            ),
        );
    }
    // Anthropic gives a whole number of seconds, but the header may also
    // hold a decimal or a date.
    let retry_after = response
        .headers()
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| parse_retry_after(value, SystemTime::now()));
    let text = response.text().await.unwrap_or_default();

    // Anthropic explains a failure as `{"type": "error", "error": {...}}`.
    // DeepSeek's `error` may lack any of its parts (`transport.ts:22-27`).
    // Anything else, such as a proxy's HTML page, goes by the status code
    // and the page's text.
    #[derive(Deserialize)]
    struct ErrorBody {
        error: WireError,
    }
    #[derive(Deserialize)]
    struct DeepSeekErrorBody {
        error: deepseek::WireError,
    }
    let explained = match profile {
        Profile::Anthropic => serde_json::from_str::<ErrorBody>(&text)
            .ok()
            .map(|body| body.error.into_model_error(Some(status.as_u16()))),
        Profile::DeepSeek => serde_json::from_str::<DeepSeekErrorBody>(&text)
            .ok()
            .map(|body| body.error.into_model_error(Some(status.as_u16()))),
    };
    let mut error = match explained {
        Some(error) => error,
        None => {
            let text = text.trim();
            let message = if text.is_empty() {
                format!("HTTP {status}")
            } else {
                format!("HTTP {status}: {text}")
            };
            ModelError::new(classify(None, &message, Some(status.as_u16())), message)
        }
    };
    error.retry_after = retry_after;
    error
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
    use crate::Profile;

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

    #[test]
    fn the_deepseek_profile_talks_to_deepseek_by_default() {
        let model = AnthropicModel::deepseek("sk-secret");
        assert_eq!(model.profile(), Profile::DeepSeek);
        assert_eq!(
            model.endpoint,
            "https://api.deepseek.com/anthropic/v1/messages"
        );
        assert!(!format!("{model:?}").contains("sk-secret"));
        assert_eq!(AnthropicModel::new("k").profile(), Profile::Anthropic);
    }

    /// Each profile names the provider its signatures' origin names, so an
    /// answer logged under that name keeps its signatures when read back.
    #[test]
    fn each_profile_names_the_provider_in_its_origins() {
        // The decoders build their origins from these two constants.
        assert_eq!(AnthropicModel::new("k").provider(), "anthropic");
        assert_eq!(
            AnthropicModel::new("k").provider(),
            crate::request::PROVIDER
        );
        assert_eq!(AnthropicModel::deepseek("k").provider(), "deepseek");
        assert_eq!(
            AnthropicModel::deepseek("k").provider(),
            crate::deepseek::PROVIDER
        );
    }

    #[test]
    fn each_profile_builds_its_endpoint_its_own_way() {
        let base = "http://localhost:8080/v1";
        assert_eq!(
            AnthropicModel::new("k").with_base_url(base).endpoint,
            "http://localhost:8080/v1/v1/messages"
        );
        assert_eq!(
            AnthropicModel::deepseek("k").with_base_url(base).endpoint,
            "http://localhost:8080/v1/messages"
        );
    }
}
