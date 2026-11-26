//! The fold of `ask_user_question` events into the questions the user can
//! answer, and the `userQuestions` projection that serves it. (dsh:
//! `user-questions/src/projection.ts`.)
//!
//! A `request/header` says, from the tool schemas it records, whether the
//! `ask_user_question` calls that follow are timed: only the timed tool's
//! schema declares [`TIMED_WAIT_PARAMETER`]. A call made under the
//! blocking legacy tool is never tracked. A tracked question stays
//! answerable, as `continued`, in exactly two cases: its tool returned the
//! pending payload, or a resume's repair gave it the synthetic
//! `TOOL_OUTCOME_UNKNOWN` result. An answer batch settles it with that
//! batch, and any failure drops it. A PTC sub-call joins when its recorded
//! result is pending. A late reply settles a question only once the agent
//! admits its `user/message`: a reply still in the inbox can be dropped.
//!
//! dsh's zod schemas are checked by hand here, so a field zod calls
//! optional may be absent but not `null`, as in dsh.

use karaka_session::{
    ContentBlock, Entry, Passthrough, SessionEvent, SessionHeader, TOOL_OUTCOME_UNKNOWN,
};
use karaka_session_projection::{ClientView, Projection};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::types::{
    AskUserQuestionAnswerItem, AskUserQuestionItem, AskUserQuestionOption, PendingUserQuestion,
    SettledUserQuestion, UserQuestionProjectionView, UserQuestionState, reply_call_id,
};

/// The tool whose calls the projection tracks.
pub const ASK_USER_QUESTION_TOOL: &str = "ask_user_question";

/// The parameter only the timed `ask_user_question` schema declares. Its
/// presence in the logged request header is what tells the fold that the
/// calls that follow can be continued past a timeout.
pub const TIMED_WAIT_PARAMETER: &str = "timeout";

/// The PTC event a sub-call's result is logged in.
const PTC_DISPATCH: &str = "tool/ptc-dispatch";

/// The fold: whether the header in effect declares the timed schema, and
/// the questions that schema's calls built. (dsh: `UserQuestionFold`.)
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UserQuestionFold {
    pub timed: bool,
    pub questions: UserQuestionProjectionView,
}

/// Whether one logged tool schema is the timed `ask_user_question` tool's:
/// its name, and a `parameters.properties` object that declares
/// [`TIMED_WAIT_PARAMETER`]. (dsh: `isTimedAskUserQuestionSchema`.)
pub fn is_timed_ask_user_question_schema(name: &str, parameters: &Value) -> bool {
    name == ASK_USER_QUESTION_TOOL
        && parameters
            .get("properties")
            .and_then(Value::as_object)
            .is_some_and(|properties| properties.contains_key(TIMED_WAIT_PARAMETER))
}

/// The questions in a call's logged arguments, in the service's words, or
/// `None` when the arguments don't read. (dsh: `questionsOf`.)
pub fn questions_of(arguments: &str) -> Option<Vec<AskUserQuestionItem>> {
    // Malformed model arguments never reached the tool; there is no card
    // to show.
    questions_of_value(&serde_json::from_str(arguments).ok()?)
}

/// As [`questions_of`], for arguments already parsed. dsh's schema is
/// loose: a question or option may carry other keys.
fn questions_of_value(arguments: &Value) -> Option<Vec<AskUserQuestionItem>> {
    let questions = arguments.as_object()?.get("questions")?.as_array()?;
    if questions.is_empty() {
        return None;
    }
    questions.iter().map(question_of).collect()
}

/// One question in the tool's words: `multi_select` becomes `multiSelect`,
/// and no `detail` or `intent` is read.
fn question_of(value: &Value) -> Option<AskUserQuestionItem> {
    let question = value.as_object()?;
    let options = match question.get("options") {
        None => None,
        Some(options) => Some(
            options
                .as_array()?
                .iter()
                .map(|option| {
                    let option = option.as_object()?;
                    Some(AskUserQuestionOption {
                        label: string(option, "label")?,
                        description: optional(option, "description", Value::as_str)?
                            .map(str::to_owned),
                    })
                })
                .collect::<Option<Vec<_>>>()?,
        ),
    };
    Some(AskUserQuestionItem {
        header: optional(question, "header", Value::as_str)?.map(str::to_owned),
        options,
        multi_select: optional(question, "multi_select", Value::as_bool)?,
        ..AskUserQuestionItem::new(string(question, "id")?, string(question, "question")?)
    })
}

fn string(object: &Map<String, Value>, key: &str) -> Option<String> {
    object.get(key)?.as_str().map(str::to_owned)
}

/// A zod `optional()` field: absent is `Some(None)`, the right type is
/// `Some(Some(_))`, and anything else, `null` included, is `None`.
fn optional<'a, T>(
    object: &'a Map<String, Value>,
    key: &str,
    read: impl Fn(&'a Value) -> Option<T>,
) -> Option<Option<T>> {
    match object.get(key) {
        None => Some(None),
        Some(value) => read(value).map(Some),
    }
}

/// The first text block's text. (dsh: `content.find(block => block.type
/// === 'text')`.)
fn first_text(content: &[ContentBlock]) -> Option<&str> {
    content.iter().find_map(|block| match block {
        ContentBlock::Text(text) => Some(text.text.as_str()),
        _ => None,
    })
}

/// Whether a result's first text is the pending payload, an object whose
/// `pending` is `true`. (dsh: `isPendingResult`.)
fn is_pending_result(content: &[ContentBlock]) -> bool {
    // A non-JSON result text is a failure message, never the pending
    // payload.
    first_text(content)
        .and_then(|text| serde_json::from_str::<Value>(text).ok())
        .and_then(|parsed| parsed.as_object()?.get("pending").cloned())
        == Some(Value::Bool(true))
}

/// The answer batch in a tool result or a late reply: the first text, read
/// as an object whose `answers` are strict answer items. (dsh:
/// `answerBatchOf`.)
fn answer_batch_of(content: &[ContentBlock]) -> Option<Vec<AskUserQuestionAnswerItem>> {
    // A failure result, a dismissal, and any other text that is not an
    // answer JSON carries no batch.
    let parsed: Value = serde_json::from_str(first_text(content)?).ok()?;
    let answers = parsed.as_object()?.get("answers")?;
    serde_json::from_value(answers.clone()).ok()
}

/// The view with `call_id` closed and settled with `answers`, or `None`
/// when that call isn't answerable. (dsh: `settleQuestion`.)
fn settle_question(
    view: &UserQuestionProjectionView,
    call_id: &str,
    answers: Vec<AskUserQuestionAnswerItem>,
) -> Option<UserQuestionProjectionView> {
    let question = view
        .active
        .iter()
        .find(|question| question.call_id == call_id)?;
    let mut settled = view.settled.clone();
    settled.push(SettledUserQuestion {
        call_id: question.call_id.clone(),
        answers,
    });
    Some(UserQuestionProjectionView {
        active: without(&view.active, call_id),
        settled,
    })
}

fn without(active: &[PendingUserQuestion], call_id: &str) -> Vec<PendingUserQuestion> {
    active
        .iter()
        .filter(|question| question.call_id != call_id)
        .cloned()
        .collect()
}

/// `active` with `call_id` asked again at the end.
fn reasked(
    view: &UserQuestionProjectionView,
    call_id: &str,
    questions: Vec<AskUserQuestionItem>,
    state: UserQuestionState,
) -> UserQuestionProjectionView {
    let mut active = without(&view.active, call_id);
    active.push(PendingUserQuestion {
        call_id: call_id.to_owned(),
        questions,
        state,
    });
    UserQuestionProjectionView {
        active,
        settled: view.settled.clone(),
    }
}

/// Folds one event, or gives `None` when it changes nothing. (dsh:
/// `applyUserQuestionEvent`, whose unchanged fold is the same object.)
pub fn apply_user_question_event(
    fold: &UserQuestionFold,
    event: &SessionEvent,
) -> Option<UserQuestionFold> {
    apply(fold.timed, &fold.questions, event)
}

/// [`apply_user_question_event`] on the fold's parts, so the projection
/// copies its view only for an event that changes it.
fn apply(
    timed_now: bool,
    view: &UserQuestionProjectionView,
    event: &SessionEvent,
) -> Option<UserQuestionFold> {
    let with = |questions| {
        Some(UserQuestionFold {
            timed: timed_now,
            questions,
        })
    };
    match event {
        SessionEvent::RequestHeader(data) => {
            // The log records the exact schemas the model saw; a header
            // with none reads as the legacy tool.
            let timed = data.header.tools.as_ref().is_some_and(|tools| {
                tools.iter().any(|tool| {
                    let Some(tool) = tool.as_object() else {
                        return false;
                    };
                    match (tool.get("name"), tool.get("parameters")) {
                        (Some(Value::String(name)), Some(parameters)) if parameters.is_object() => {
                            is_timed_ask_user_question_schema(name, parameters)
                        }
                        _ => false,
                    }
                })
            });
            (timed != timed_now).then(|| UserQuestionFold {
                timed,
                questions: view.clone(),
            })
        }
        SessionEvent::ToolCall(call) => {
            if !timed_now || call.name != ASK_USER_QUESTION_TOOL {
                return None;
            }
            let questions = questions_of(&call.arguments)?;
            with(reasked(
                view,
                &call.call_id,
                questions,
                UserQuestionState::Open,
            ))
        }
        SessionEvent::ToolResult { data, .. } => {
            let call_id = &data.message.tool_call_id;
            if !view
                .active
                .iter()
                .any(|question| &question.call_id == call_id)
            {
                return None;
            }
            let unknown = data
                .error
                .as_ref()
                .is_some_and(|error| error.code == TOOL_OUTCOME_UNKNOWN);
            if is_pending_result(&data.message.content) || unknown {
                let active = view
                    .active
                    .iter()
                    .map(|question| {
                        let mut question = question.clone();
                        if &question.call_id == call_id {
                            question.state = UserQuestionState::Continued;
                        }
                        question
                    })
                    .collect();
                return with(UserQuestionProjectionView {
                    active,
                    settled: view.settled.clone(),
                });
            }
            let answers = (data.error.is_none() && data.message.is_error != Some(true))
                .then(|| answer_batch_of(&data.message.content))
                .flatten();
            with(match answers {
                Some(answers) => settle_question(view, call_id, answers)?,
                None => UserQuestionProjectionView {
                    active: without(&view.active, call_id),
                    settled: view.settled.clone(),
                },
            })
        }
        SessionEvent::Passthrough(Passthrough {
            event_type, data, ..
        }) if event_type == PTC_DISPATCH => {
            let is_error = data.get("isError").is_some_and(truthy);
            if data.get("name").and_then(Value::as_str) != Some(ASK_USER_QUESTION_TOOL) || is_error
            {
                return None;
            }
            let content: Vec<ContentBlock> =
                serde_json::from_value(data.get("content")?.clone()).ok()?;
            if !is_pending_result(&content) {
                return None;
            }
            let questions = questions_of_value(data.get("arguments")?)?;
            let call_id = data.get("subCallId")?.as_str()?;
            with(reasked(
                view,
                call_id,
                questions,
                UserQuestionState::Continued,
            ))
        }
        SessionEvent::UserMessage { message, .. } => {
            let call_id = reply_call_id(&message.source)?;
            let answers = answer_batch_of(&message.content).unwrap_or_default();
            with(settle_question(view, call_id, answers)?)
        }
        _ => None,
    }
}

/// JavaScript's truthiness, for a field the fold only tests.
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(number) => number.as_f64().is_some_and(|n| n != 0.0 && !n.is_nan()),
        Value::String(text) => !text.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// Folds a whole log: open and continued timed calls in the order they
/// were asked, and settled ones in the order they settled. (dsh:
/// `foldUserQuestions`.)
pub fn fold_user_questions(events: &[Entry]) -> UserQuestionProjectionView {
    events
        .iter()
        .fold(UserQuestionFold::default(), |fold, entry| {
            apply_user_question_event(&fold, &entry.event).unwrap_or(fold)
        })
        .questions
}

/// The projection's state: the fold, and how many events the session
/// inherited, which it skips. (dsh: `UserQuestionProjectionState`.)
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UserQuestionsState {
    pub inherited_event_count: u64,
    pub timed: bool,
    pub questions: UserQuestionProjectionView,
}

/// The `userQuestions` projection: the open, continued and settled timed
/// questions, shown to every client. (dsh:
/// `userQuestionProjectionDefinition`.)
///
/// A fork doesn't inherit its parent's questions: events before the
/// inherited count change nothing.
#[derive(Clone, Copy, Debug, Default)]
pub struct UserQuestionsProjection;

impl Projection for UserQuestionsProjection {
    const KEY: &'static str = "userQuestions";
    type State = UserQuestionsState;

    fn state_version(&self) -> u64 {
        2
    }

    fn init(&self, _: &SessionHeader, inherited_event_count: u64) -> UserQuestionsState {
        UserQuestionsState {
            inherited_event_count,
            ..UserQuestionsState::default()
        }
    }

    fn apply(&self, state: &UserQuestionsState, entry: &Entry) -> Option<UserQuestionsState> {
        if entry.seq.0 < state.inherited_event_count {
            return None;
        }
        let next = apply(state.timed, &state.questions, &entry.event)?;
        Some(UserQuestionsState {
            inherited_event_count: state.inherited_event_count,
            timed: next.timed,
            questions: next.questions,
        })
    }
}

impl ClientView for UserQuestionsProjection {
    type View = UserQuestionProjectionView;

    fn view(&self, state: &UserQuestionsState) -> UserQuestionProjectionView {
        state.questions.clone()
    }
}
