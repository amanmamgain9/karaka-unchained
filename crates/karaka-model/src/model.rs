use std::error::Error;
use std::fmt;
use std::future::poll_fn;
use std::pin::Pin;
use std::time::Duration;

use futures_core::Stream;

use crate::{Block, Message, Role};

/// What a model provider implements.
///
/// Plugins share it as `Arc<dyn Model>`. It isn't a port of its own: the
/// kernel allows one provider per port, and a runtime may hold several
/// providers. Each one registers its model in the [`ModelRegistry`] under a
/// route, and callers pick a model by route there.
///
/// [`ModelRegistry`]: crate::ModelRegistry
pub trait Model: Send + Sync {
    /// Starts one model call and returns its events as they arrive.
    ///
    /// Nothing is sent until the stream is first polled, and dropping the
    /// stream cancels the call. A successful stream ends with
    /// [`Event::Done`]; a failed one ends with an `Err`.
    ///
    /// Not `async`: the stream is the asynchronous part. It owns everything
    /// it needs (`'static`), so it can outlive the borrow of `self` and move
    /// between threads.
    fn stream(&self, request: Request) -> EventStream;
}

/// What [`Model::stream`] returns. Boxed so that every provider, whatever its
/// stream type, fits behind `dyn Model`.
pub type EventStream = Pin<Box<dyn Stream<Item = Result<Event, ModelError>> + Send>>;

/// One model call, fully assembled.
#[derive(Clone, Debug, PartialEq)]
pub struct Request {
    /// The model's id at its provider, such as `claude-opus-5-5`.
    pub model: String,
    pub system: Option<String>,
    pub messages: Vec<Message>,
    /// The tools the model may call.
    pub tools: Vec<ToolSchema>,
    /// The most the model may write. `None` leaves it to the provider.
    pub max_tokens: Option<u32>,
    /// How hard the model should work on this request. `None` uses the
    /// default in the provider's config, and if that is unset too, the
    /// provider's own default.
    pub effort: Option<Effort>,
}

impl Request {
    /// A request with no system prompt, no tools, no output limit and no
    /// effort of its own.
    pub fn new(model: impl Into<String>, messages: Vec<Message>) -> Self {
        Self {
            model: model.into(),
            system: None,
            messages,
            tools: Vec::new(),
            max_tokens: None,
            effort: None,
        }
    }
}

/// How much thinking and work a model puts into an answer, from the least to
/// the most. More effort usually means a better answer, more tokens and a
/// longer wait.
///
/// Every provider has a setting like this, but each names its levels
/// differently and supports only some of them. Each provider maps a level to
/// the closest value it supports, and documents its mapping.
///
/// Whether the model shows its reasoning is a separate question, and it is
/// not here. Providers answer it in very different ways: Anthropic always
/// thinks and only chooses whether to return a summary, while DeepSeek turns
/// thinking itself on or off. So it lives in each provider's config.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Deserialize),
    serde(rename_all = "lowercase")
)]
pub enum Effort {
    Low,
    Medium,
    High,
    /// Between `High` and `Max`.
    XHigh,
    Max,
}

/// A tool as the model sees it.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolSchema {
    pub name: String,
    /// Tells the model when to use the tool.
    pub description: String,
    /// A JSON Schema object for the arguments, as JSON text.
    pub parameters: String,
}

/// Something that happened during a model call.
///
/// Deltas are for showing the answer as it is written. Each block also
/// arrives whole as [`Event::Block`] once it is finished, so a caller that
/// only wants the result can ignore the deltas.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// More of the text block being written.
    TextDelta(String),
    /// More of the reasoning block being written.
    ReasoningDelta(String),
    /// A finished block, in message order.
    Block(Block),
    /// The call finished. Always the last event.
    Done { stop: StopReason, usage: Usage },
}

/// Why the model stopped writing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StopReason {
    /// It finished its answer.
    EndTurn,
    /// It wants tools to run; the results go back in the next request.
    ToolCalls,
    /// It reached `max_tokens` before finishing.
    MaxTokens,
    /// A reason only this provider has, such as a refusal, by its own name.
    Other(String),
}

/// Tokens counted for one call.
///
/// The counts don't overlap: `input_tokens` is only the input that wasn't
/// cached, so the billed input is the sum of the first and the two cache
/// fields.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
}

/// A finished call, as [`complete`] assembles it.
#[derive(Clone, Debug, PartialEq)]
pub struct Response {
    pub message: Message,
    pub stop: StopReason,
    pub usage: Usage,
}

/// Runs one call to the end and returns the assembled answer, for callers
/// that don't show it while it's written.
pub async fn complete(model: &dyn Model, request: Request) -> Result<Response, ModelError> {
    let mut stream = model.stream(request);
    let mut content = Vec::new();
    // `poll_fn` turns the stream's `poll_next` into something to `.await`,
    // which saves depending on a library for it.
    while let Some(event) = poll_fn(|cx| stream.as_mut().poll_next(cx)).await {
        match event? {
            Event::TextDelta(_) | Event::ReasoningDelta(_) => {}
            Event::Block(block) => content.push(block),
            Event::Done { stop, usage } => {
                return Ok(Response {
                    message: Message {
                        role: Role::Assistant,
                        content,
                    },
                    stop,
                    usage,
                });
            }
        }
    }
    Err(ModelError::new(
        ErrorKind::Network,
        "the stream ended before the model finished",
    ))
}

/// Why a model call failed.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelError {
    pub kind: ErrorKind,
    /// What went wrong, in the provider's words where it gave any.
    pub message: String,
    /// How long the provider asked us to wait before trying again.
    pub retry_after: Option<Duration>,
}

impl ModelError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            retry_after: None,
        }
    }
}

impl fmt::Display for ModelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.kind.describe(), self.message)
    }
}

impl Error for ModelError {}

/// The kind of failure, which decides whether trying again can help.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// The credentials were missing or refused.
    Auth,
    /// The provider rejected the request itself, for example as too long.
    InvalidRequest,
    /// Too many requests for now.
    RateLimited,
    /// The provider is overloaded or failed on its side.
    Unavailable,
    /// The connection failed, or the stream broke off.
    Network,
}

impl ErrorKind {
    /// Whether the same request might succeed later.
    pub fn is_retryable(self) -> bool {
        matches!(self, Self::RateLimited | Self::Unavailable | Self::Network)
    }

    fn describe(self) -> &'static str {
        match self {
            Self::Auth => "the provider refused the credentials",
            Self::InvalidRequest => "the provider rejected the request",
            Self::RateLimited => "rate limited",
            Self::Unavailable => "the provider is unavailable",
            Self::Network => "the connection failed",
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    use futures_core::Stream;

    use super::{
        ErrorKind, Event, EventStream, Model, ModelError, Request, StopReason, Usage, complete,
    };
    use crate::{Block, Message, Role};

    /// A model that plays back the same events on every call.
    struct Scripted(Vec<Result<Event, ModelError>>);

    impl Model for Scripted {
        fn stream(&self, _request: Request) -> EventStream {
            Box::pin(Playback(self.0.clone().into()))
        }
    }

    /// The smallest stream there is: every poll is immediately ready with
    /// the next event, and `None` ends it.
    struct Playback(VecDeque<Result<Event, ModelError>>);

    impl Stream for Playback {
        type Item = Result<Event, ModelError>;

        fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Poll::Ready(self.get_mut().0.pop_front())
        }
    }

    fn request() -> Request {
        Request::new("test-model", vec![Message::user("hi")])
    }

    fn done() -> Event {
        Event::Done {
            stop: StopReason::EndTurn,
            usage: Usage {
                input_tokens: 3,
                output_tokens: 2,
                ..Usage::default()
            },
        }
    }

    #[tokio::test]
    async fn complete_keeps_the_blocks_and_skips_the_deltas() {
        let model = Scripted(vec![
            Ok(Event::TextDelta("hel".into())),
            Ok(Event::TextDelta("lo".into())),
            Ok(Event::Block(Block::Text("hello".into()))),
            Ok(done()),
        ]);
        let response = complete(&model, request()).await.expect("the call works");
        assert_eq!(response.message.role, Role::Assistant);
        assert_eq!(response.message.text(), "hello");
        assert_eq!(response.message.content.len(), 1);
        assert_eq!(response.stop, StopReason::EndTurn);
        assert_eq!(response.usage.input_tokens, 3);
    }

    #[tokio::test]
    async fn complete_passes_on_a_failure() {
        let model = Scripted(vec![
            Ok(Event::TextDelta("hel".into())),
            Err(ModelError::new(ErrorKind::Unavailable, "overloaded")),
        ]);
        let error = complete(&model, request())
            .await
            .expect_err("the call fails");
        assert_eq!(error.kind, ErrorKind::Unavailable);
        assert!(error.kind.is_retryable());
        assert_eq!(error.to_string(), "the provider is unavailable: overloaded");
    }

    #[tokio::test]
    async fn complete_rejects_a_stream_that_ends_without_done() {
        let model = Scripted(vec![Ok(Event::Block(Block::Text("hel".into())))]);
        let error = complete(&model, request())
            .await
            .expect_err("the call fails");
        assert_eq!(error.kind, ErrorKind::Network);
    }

    /// Compiles only if a call can move between threads, as it must on a
    /// multi-threaded executor serving many connections.
    #[test]
    fn a_call_works_across_threads() {
        fn assert_send<T: Send>(_: T) {}

        let model = Scripted(Vec::new());
        assert_send(complete(&model, request()));
    }
}
