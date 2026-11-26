//! The questions, their answers, and what the projection shows of them, in
//! dsh's JSON. (dsh: `user-questions/src/types.ts`.)
//!
//! Every type reads and writes dsh's camelCase field names, and leaves an
//! absent optional field out, as dsh's objects do. The types that a
//! checkpoint restores are strict: an unknown field is refused, as dsh's
//! restore schemas refuse one (`projection.ts`).

use std::fmt;
use std::sync::Arc;

use karaka_agent::Agent;
use karaka_fs::AbortSignal;
use karaka_session::UserSource;
use serde::de::Error as _;
use serde::ser::SerializeStruct;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Value, json};

/// The `kind` of a late reply's message source. (dsh: the
/// `user-question-reply` entry of `MessageSourceMap`.)
pub const REPLY_SOURCE_KIND: &str = "user-question-reply";

/// One choice a question offers. (dsh: `AskUserQuestionOption`.)
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AskUserQuestionOption {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl AskUserQuestionOption {
    /// An option with no description.
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            description: None,
        }
    }
}

/// How a UI should present a question. (dsh: `AskUserQuestionIntent`.)
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub enum AskUserQuestionIntent {
    /// The question asks the user to approve a plan, which is the
    /// question's `detail`. `approve` is the label of the option that
    /// approves it. (dsh: `{ kind: 'plan-review', approve, callId? }`.)
    #[serde(rename = "plan-review", rename_all = "camelCase")]
    PlanReview {
        approve: String,
        /// The call that produced the plan, when there is one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_id: Option<String>,
    },
}

impl AskUserQuestionIntent {
    /// The intent's `kind` tag, such as `plan-review`.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::PlanReview { .. } => "plan-review",
        }
    }

    /// The label of the option that approves.
    pub fn approve(&self) -> &str {
        match self {
            Self::PlanReview { approve, .. } => approve,
        }
    }
}

/// One question. (dsh: `AskUserQuestionItem`.)
///
/// dsh's restore schema for the projection has no `intent`, since the
/// projection never records one; this type takes it either way.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AskUserQuestionItem {
    /// Names the question in its answer.
    pub id: String,
    pub question: String,
    /// More to read before answering, such as the plan a plan-review
    /// approves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// A short label for the question.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<Vec<AskUserQuestionOption>>,
    /// Whether more than one option may be chosen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub multi_select: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent: Option<AskUserQuestionIntent>,
}

impl AskUserQuestionItem {
    /// A question with nothing but its id and text.
    pub fn new(id: impl Into<String>, question: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            question: question.into(),
            detail: None,
            header: None,
            options: None,
            multi_select: None,
            intent: None,
        }
    }
}

/// The answer to one question. (dsh: `AskUserQuestionAnswerItem`.)
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AskUserQuestionAnswerItem {
    /// The question's id.
    pub id: String,
    /// The labels of the options chosen.
    pub selected: Vec<String>,
    /// What the user typed instead, or as well.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom: Option<String>,
}

/// The answers to one call's questions. (dsh: `AskUserQuestionAnswer`.)
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskUserQuestionAnswer {
    pub answers: Vec<AskUserQuestionAnswerItem>,
}

/// Whether a tracked question is still in its call, or past it. (dsh:
/// `UserQuestionState`.)
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UserQuestionState {
    /// Its call is still running.
    Open,
    /// Its call ended without an answer, so a late reply may answer it.
    Continued,
}

/// A question the user can still answer. (dsh: `PendingUserQuestion`.)
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PendingUserQuestion {
    #[serde(deserialize_with = "non_empty_id")]
    pub call_id: String,
    #[serde(deserialize_with = "some_questions")]
    pub questions: Vec<AskUserQuestionItem>,
    pub state: UserQuestionState,
}

/// A question that was answered, with what it was answered with. (dsh:
/// `SettledUserQuestion`.)
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SettledUserQuestion {
    #[serde(deserialize_with = "non_empty_id")]
    pub call_id: String,
    pub answers: Vec<AskUserQuestionAnswerItem>,
}

/// The `userQuestions` projection's view: open and continued calls in the
/// order they were asked, and settled ones in the order they settled.
/// (dsh: `UserQuestionProjectionView`.)
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserQuestionProjectionView {
    #[serde(deserialize_with = "unique_calls")]
    pub active: Vec<PendingUserQuestion>,
    pub settled: Vec<SettledUserQuestion>,
}

/// dsh: `z.string().min(1)` on a call id.
fn non_empty_id<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    let id = String::deserialize(deserializer)?;
    if id.is_empty() {
        return Err(D::Error::custom("callId must not be empty"));
    }
    Ok(id)
}

/// dsh: `z.array(questionSchema).min(1)`.
fn some_questions<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<AskUserQuestionItem>, D::Error> {
    let questions = Vec::<AskUserQuestionItem>::deserialize(deserializer)?;
    if questions.is_empty() {
        return Err(D::Error::custom("questions must not be empty"));
    }
    Ok(questions)
}

/// dsh: the `superRefine` on `pendingQuestionsSchema`.
fn unique_calls<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<PendingUserQuestion>, D::Error> {
    let active = Vec::<PendingUserQuestion>::deserialize(deserializer)?;
    let mut seen = std::collections::HashSet::new();
    if !active.iter().all(|question| seen.insert(&question.call_id)) {
        return Err(D::Error::custom("callIds must be unique"));
    }
    Ok(active)
}

/// The end of a timed ask: the answer, or the call left pending when its
/// window closed. (dsh: `TimedUserQuestionResult`.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TimedUserQuestionResult {
    Answered(AskUserQuestionAnswer),
    /// dsh: `{ pending: true, callId }`.
    Pending {
        call_id: String,
    },
}

impl Serialize for TimedUserQuestionResult {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Answered(answer) => answer.serialize(serializer),
            Self::Pending { call_id } => {
                let mut pending = serializer.serialize_struct("Pending", 2)?;
                pending.serialize_field("pending", &true)?;
                pending.serialize_field("callId", call_id)?;
                pending.end()
            }
        }
    }
}

/// A request for the user's answer. (dsh: `AskUserQuestionRequestEvent`.)
#[derive(Clone, Default)]
pub struct QuestionRequest {
    pub questions: Vec<AskUserQuestionItem>,
    /// The agent asking, which must be the live one its registry holds.
    pub agent: Option<Arc<dyn Agent>>,
    /// Cancels the request.
    pub signal: Option<AbortSignal>,
    /// The tool call a timed ask waits in.
    pub wait: Option<QuestionWait>,
}

impl QuestionRequest {
    /// A request for `questions`, with no agent, signal or wait.
    pub fn new(questions: Vec<AskUserQuestionItem>) -> Self {
        Self {
            questions,
            ..Self::default()
        }
    }
}

impl fmt::Debug for QuestionRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QuestionRequest")
            .field("questions", &self.questions)
            .field("agent", &self.agent.as_ref().map(|agent| agent.id()))
            .field("signal", &self.signal.as_ref().map(AbortSignal::is_aborted))
            .field("wait", &self.wait)
            .finish()
    }
}

/// The call a request waits in. (dsh: the request event's `wait`.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuestionWait {
    pub call_id: String,
    /// Whether the wait has a window. (dsh: `timed?`.)
    pub timed: bool,
}

/// The source of a late reply to `call_id`: `{ kind:
/// "user-question-reply", callId, outcome: "answered" }`.
pub fn reply_source(call_id: &str) -> UserSource {
    UserSource::Other(json!({
        "kind": REPLY_SOURCE_KIND,
        "callId": call_id,
        "outcome": "answered",
    }))
}

/// The call a message's source replies to, when it is a late reply.
pub fn reply_call_id(source: &UserSource) -> Option<&str> {
    match source {
        UserSource::Other(value) => reply_call_id_of(value),
        _ => None,
    }
}

/// As [`reply_call_id`], for a source as JSON.
pub(crate) fn reply_call_id_of(source: &Value) -> Option<&str> {
    if source.get("kind")?.as_str()? != REPLY_SOURCE_KIND {
        return None;
    }
    source.get("callId")?.as_str()
}
