use std::error::Error;
use std::fmt;
use std::future::poll_fn;
use std::pin::Pin;
use std::time::Duration;

use futures_core::Stream;

use crate::{Block, Message, RetryPolicy, Role};

/// What a model provider implements.
///
/// Plugins share it as `Arc<dyn Model>`. It isn't a port of its own: the
/// kernel allows one provider per port, and a runtime may hold several
/// providers. Each one registers its model in the [`ModelRegistry`] under a
/// route, and callers pick a model by route there.
///
/// [`ModelRegistry`]: crate::ModelRegistry
pub trait Model: Send + Sync {
    /// The provider's own name, such as `anthropic` or `deepseek`: the name
    /// it writes as [`Origin::provider`](crate::Origin::provider) in the
    /// replay data it attaches.
    ///
    /// The loop records it as the provider of each answer, as dsh records
    /// `request.provider` in an assistant message's `source.provider`
    /// (`agent-loop/src/agent.ts:529-536`). It isn't the route: a runtime may
    /// register Anthropic under `anthropic-work`, but a signature's origin
    /// still says `anthropic`, and a signature is sent back only when the
    /// message's provider matches its origin. Writing the route there would
    /// lose every signature when the session is read back.
    fn provider(&self) -> &str;

    /// Starts one model call and returns its events as they arrive.
    ///
    /// Nothing is sent until the stream is first polled, and dropping the
    /// stream cancels the call. A successful stream ends with
    /// [`Event::Done`]; a failed one ends with an `Err`.
    ///
    /// A call can fail after part of the answer has arrived. A refusal, for
    /// one, can come after some text has streamed, and the stream then ends
    /// in an `Err` after those deltas and blocks. They belong to a failed
    /// call: show them if you like, but don't save them as the model's turn.
    ///
    /// Not `async`: the stream is the asynchronous part. It owns everything
    /// it needs (`'static`), so it can outlive the borrow of `self` and move
    /// between threads.
    fn stream(&self, request: Request) -> EventStream;

    /// How this provider wants its failed requests retried. (dsh: each
    /// adapter's `providerRetryPolicy`, from its config's `retryPolicy`.)
    ///
    /// Only read: whatever retries requests follows it. A provider whose
    /// config sets no policy gets dsh's default, which this method returns.
    fn retry_policy(&self) -> RetryPolicy {
        RetryPolicy::default()
    }
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
///
/// A call that finishes with [`StopReason::MaxTokens`] never yields a
/// [`ToolCall`](crate::ToolCall) block. The limit can cut a call off partway, and running a
/// tool on arguments that may be incomplete isn't safe, so providers drop
/// such calls, as dsh does (`llm/src/assembler.ts:131-141`). A provider
/// that learns why the model stopped only after a call's block is finished
/// holds that block back until it knows. It holds back the blocks after it
/// too, so the blocks still arrive in message order. Deltas are never held
/// back.
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

/// Why the model stopped writing, when it stopped without failing.
///
/// Some reasons a provider gives are failures, even when they arrive in a
/// successful response: a refusal, a content filter, a full context window,
/// or a reason the provider has added since this was written. Those end the
/// call with a [`ModelError`] instead, so none of them can pass for an
/// answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopReason {
    /// It finished its answer, or reached a stop sequence.
    EndTurn,
    /// It wants tools to run; the results go back in the next request.
    ToolCalls,
    /// It reached `max_tokens` before finishing. Any tool call it had
    /// started is dropped (see [`Event`]).
    MaxTokens,
    /// The provider paused a long turn of its own server-side tools, such as
    /// Anthropic's web search, and the turn isn't over. Anthropic resumes it
    /// when the conversation is sent back as it is.
    ///
    /// Karaka sends no server tools yet, so this shouldn't happen. It also
    /// doesn't keep those tools' blocks, which resuming would need. Until it
    /// does, treat a paused answer as unfinished, like
    /// [`StopReason::MaxTokens`].
    Paused,
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
                // Providers already drop these (see `Event`). Doing it here
                // as well keeps the rule for any `Model`, including one
                // written without it in mind.
                if stop == StopReason::MaxTokens {
                    content.retain(|block| !matches!(block, Block::ToolCall(_)));
                }
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
    /// What the provider said about a refusal. Only ever set with
    /// [`ErrorKind::Refused`], and only when the provider said anything.
    pub refusal: Option<Refusal>,
}

impl ModelError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            retry_after: None,
            refusal: None,
        }
    }
}

impl fmt::Display for ModelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.kind.describe(), self.message)?;
        if let Some(category) = self.refusal.as_ref().and_then(|r| r.category.as_ref()) {
            write!(f, " (category: {category})")?;
        }
        Ok(())
    }
}

impl Error for ModelError {}

/// What a provider said about a refusal: why it declined, as far as it
/// says.
///
/// Anthropic sends this as `stop_details`. It is for showing and logging,
/// not for deciding what to do: Anthropic says to branch on the refusal
/// itself, because the details can be missing even on a real refusal.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Refusal {
    /// The policy area, in the provider's words, such as `cyber` or `bio`.
    pub category: Option<String>,
    /// A sentence explaining the refusal, when the provider gives one.
    pub explanation: Option<String>,
}

/// The kind of failure, which decides whether trying again can help.
///
/// dsh names failures with string codes. Each kind stands for these codes,
/// and is retried as dsh's default policy retries them
/// (`llm/src/retry-policy.ts:193-199`):
///
/// | Kind                    | dsh code                                       | Retryable |
/// |-------------------------|------------------------------------------------|-----------|
/// | `Auth`                  | `AUTH`, `INVALID_CREDENTIAL`                   | no        |
/// | `Quota`                 | `QUOTA`                                        | no        |
/// | `InvalidRequest`        | `INVALID_REQUEST`, `HTTP_<status>` such as 404 | no        |
/// | `ContextWindowExceeded` | `CONTEXT_WINDOW_EXCEEDED`                      | no        |
/// | `Refused`               | none; see below                                | no        |
/// | `Malformed`             | `MALFORMED_RESPONSE`, `UNSUPPORTED_CONTENT`    | no        |
/// | `EmptyResponse`         | `EMPTY_RESPONSE`                               | yes       |
/// | `RateLimited`           | `RATE_LIMIT`                                   | yes       |
/// | `Unavailable`           | `SERVER`                                       | yes       |
/// | `Network`               | `TRANSPORT`, `TIMEOUT`, `STREAM_CLOSED`        | yes       |
///
/// Two places differ from dsh:
///
/// - **Refusals** have no dsh code. pi-ai reports one as a plain error, and
///   dsh files that as `PI_AI_ERROR`, which it doesn't retry either. Karaka
///   names it, because the loop has to keep a refused turn out of the
///   history, and a user should hear that the model declined rather than
///   that something broke.
/// - **A body that ends early but cleanly** is `STREAM_CLOSED` in dsh, which
///   it doesn't retry (`llm-retry/tests/transport-recovery.spec.ts:192`).
///   Karaka calls it `Network`, like a dropped connection, and so retries
///   it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// The credentials were missing or refused.
    Auth,
    /// The account has run out of quota, balance or credit. Someone has to
    /// pay or raise a limit before it works again.
    Quota,
    /// The provider rejected the request itself.
    InvalidRequest,
    /// The conversation is too long for the model's context window. The
    /// same request will fail again, but a shorter one may not, so whoever
    /// compacts the conversation watches for this.
    ContextWindowExceeded,
    /// The model, or a filter in front of it, declined to answer. Any
    /// [`Refusal`] details are on the error.
    Refused,
    /// The provider's answer couldn't be read: it broke the protocol, or
    /// used something this version doesn't know, such as a new stop reason.
    Malformed,
    /// The model finished normally but wrote nothing at all. That says
    /// nothing about the request, so trying again usually works.
    EmptyResponse,
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
        matches!(
            self,
            Self::EmptyResponse | Self::RateLimited | Self::Unavailable | Self::Network
        )
    }

    /// dsh's code for this kind, as dsh's adapters write it into a failure,
    /// such as `RATE_LIMIT`. Where a kind stands for several codes (see the
    /// table above), this is the one dsh writes most.
    ///
    /// A refusal has no dsh code: pi-ai reports one as a plain error, which
    /// dsh files as `PI_AI_ERROR`, a code that names an adapter Karaka
    /// doesn't have. Karaka writes `REFUSED`, so a reader can tell a
    /// refusal from a breakage.
    ///
    /// These are the codes a [`RetryPolicy`] lists, so the default policy
    /// retries exactly the kinds that are [retryable](Self::is_retryable).
    pub fn code(self) -> &'static str {
        match self {
            Self::Auth => "AUTH",
            Self::Quota => "QUOTA",
            Self::InvalidRequest => "INVALID_REQUEST",
            Self::ContextWindowExceeded => "CONTEXT_WINDOW_EXCEEDED",
            Self::Refused => "REFUSED",
            Self::Malformed => "MALFORMED_RESPONSE",
            Self::EmptyResponse => "EMPTY_RESPONSE",
            Self::RateLimited => "RATE_LIMIT",
            Self::Unavailable => "SERVER",
            Self::Network => "TRANSPORT",
        }
    }

    fn describe(self) -> &'static str {
        match self {
            Self::Auth => "the provider refused the credentials",
            Self::Quota => "the account is out of quota or credit",
            Self::InvalidRequest => "the provider rejected the request",
            Self::ContextWindowExceeded => "the conversation is too long for the model",
            Self::Refused => "the model declined to answer",
            Self::Malformed => "the provider's answer couldn't be read",
            Self::EmptyResponse => "the model answered with nothing",
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
        ErrorKind, Event, EventStream, Model, ModelError, Refusal, Request, StopReason, Usage,
        complete,
    };
    use crate::{Block, Message, Role, ToolCall};

    /// A model that plays back the same events on every call.
    struct Scripted(Vec<Result<Event, ModelError>>);

    impl Model for Scripted {
        fn provider(&self) -> &str {
            "scripted"
        }

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

    #[tokio::test]
    async fn complete_drops_tool_calls_from_a_max_tokens_finish() {
        let model = Scripted(vec![
            Ok(Event::Block(Block::Reasoning {
                text: "look it up".into(),
                replay: None,
            })),
            Ok(Event::Block(Block::Text("Let me check.".into()))),
            Ok(Event::Block(Block::ToolCall(ToolCall {
                id: "call_1".into(),
                name: "search".into(),
                arguments: r#"{"query": "wea"#.into(),
            }))),
            Ok(Event::Done {
                stop: StopReason::MaxTokens,
                usage: Usage::default(),
            }),
        ]);
        let response = complete(&model, request()).await.expect("the call works");
        assert_eq!(response.stop, StopReason::MaxTokens);
        assert_eq!(
            response.message.content,
            [
                Block::Reasoning {
                    text: "look it up".into(),
                    replay: None,
                },
                Block::Text("Let me check.".into()),
            ]
        );
    }

    #[test]
    fn a_refusal_says_its_category() {
        let mut error =
            ModelError::new(ErrorKind::Refused, "That's not something I can help with.");
        error.refusal = Some(Refusal {
            category: Some("cyber".into()),
            explanation: Some("That's not something I can help with.".into()),
        });
        assert_eq!(
            error.to_string(),
            "the model declined to answer: That's not something I can help with. (category: cyber)"
        );
    }

    /// The table on `ErrorKind`, checked: dsh's default policy retries
    /// exactly `EMPTY_RESPONSE`, `RATE_LIMIT`, `SERVER`, `TIMEOUT` and
    /// `TRANSPORT`.
    #[test]
    fn only_the_kinds_dsh_retries_are_retryable() {
        let retryable = [
            ErrorKind::EmptyResponse,
            ErrorKind::RateLimited,
            ErrorKind::Unavailable,
            ErrorKind::Network,
        ];
        let not_retryable = [
            ErrorKind::Auth,
            ErrorKind::Quota,
            ErrorKind::InvalidRequest,
            ErrorKind::ContextWindowExceeded,
            ErrorKind::Refused,
            ErrorKind::Malformed,
        ];
        for kind in retryable {
            assert!(kind.is_retryable(), "{kind:?}");
        }
        for kind in not_retryable {
            assert!(!kind.is_retryable(), "{kind:?}");
        }
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
