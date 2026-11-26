//! The `ask_user_question` tool: the model asks the user brief questions,
//! and the answer comes back as the tool's result. (dsh:
//! `@deepseek-ai/dsh-tool-ask-user`, 0.2.1-rc.2: `index.ts` and
//! `timed.ts`.)
//!
//! It asks through the `karaka-user-questions` service, whose answerers are
//! the UIs. There are two tools of one name, and [`AskUserConfig::mode`]
//! picks one:
//!
//! - **legacy**, the default: the call blocks until the user answers.
//! - **timed**: the call waits a window, `timeout` seconds, which the model
//!   may set per call. When the window closes unanswered, the result says
//!   the questions are pending, and the user can still answer later, as a
//!   steered user message. A `timeout` of -1 waits for the answer. The
//!   schema's [`TIMED_WAIT_PARAMETER`] is what tells the `userQuestions`
//!   projection the calls are timed.
//!
//! Results are the JSON text of the answer, or of the pending payload, as
//! dsh's `render` gives them. A failure is `Error: <message>`, as dsh's
//! tool layer shows a thrown error. dsh also puts the error's name and code
//! on the result; Karaka's results are text only.

use std::fmt;
use std::sync::Arc;

use karaka_kernel::{BoxError, Plugin, PluginContext, PortId};
use karaka_model::ToolSchema;
use karaka_tools::{Tool, ToolFuture, ToolOutput, ToolRegistry, ToolRun};
use karaka_user_questions::{
    AskUserQuestionItem, AskUserQuestionOption, QuestionRequest, QuestionWait,
    TIMED_WAIT_PARAMETER, TimedUserQuestionResult, UserQuestions,
};
use serde_json::{Value, json};

/// The tool's name.
pub const NAME: &str = "ask_user_question";

/// What the model is told the legacy tool does. (dsh: `index.ts`'s
/// `description`.)
pub const LEGACY_DESCRIPTION: &str = "Ask the user a concise question when you need \
                                      confirmation, a choice, or missing information before \
                                      proceeding.";

/// What the model is told the timed tool does. (dsh: `timed.ts`'s
/// `description`.)
pub const TIMED_DESCRIPTION: &str = "Ask brief, direct, self-contained questions about missing \
     information, preferences, or decisions. Use user-facing terms; assume no knowledge of \
     background work or internal names. Use distinct stable question IDs. A submitted skipped \
     question is an answer item with empty selected and no custom; pending instead means no \
     answer batch arrived before the timeout and the user can still answer.";

/// What a pending result tells the model, in its `message`. (dsh:
/// `pendingNotice`.)
pub const PENDING_NOTICE: &str = "No answer batch arrived before the timeout. This is pending, \
     not a skipped answer. Continue useful independent work. The user can still answer; their \
     reply will be a user message identified as answer_to_pending_question with this callId and \
     the original questions. Do not treat this as permission.";

/// The longest window, in seconds. (dsh: `2_147_483`.)
pub const MAX_TIMEOUT_SECONDS: i64 = 2_147_483;

/// Which tool is registered. (dsh: `Config.mode`.)
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    /// The blocking tool.
    #[default]
    Legacy,
    /// The tool with a foreground window.
    Timed,
}

/// The plugin's settings. (dsh: `Config`.)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AskUserConfig {
    pub mode: Mode,
    /// The timed tool's default window, in seconds: -1, to wait for the
    /// answer, or 1 to [`MAX_TIMEOUT_SECONDS`]. The legacy tool doesn't
    /// use it.
    pub timeout: i64,
}

impl Default for AskUserConfig {
    /// dsh's defaults: the legacy tool, and a 120 second window.
    fn default() -> Self {
        Self {
            mode: Mode::Legacy,
            timeout: 120,
        }
    }
}

/// dsh's `validateTimeout` message.
const BAD_TIMEOUT: &str = "timeout must be -1 or a positive integer up to 2147483 seconds";

/// A timeout dsh accepts: -1, or a whole number of seconds from 1 to
/// [`MAX_TIMEOUT_SECONDS`]. (dsh: `validateTimeout`.)
fn valid_timeout(timeout: f64) -> Option<i64> {
    let whole = timeout.fract() == 0.0;
    (timeout == -1.0 || (whole && (1.0..=MAX_TIMEOUT_SECONDS as f64).contains(&timeout)))
        .then_some(timeout as i64)
}

/// Why the plugin refused its config.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigError;

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(BAD_TIMEOUT)
    }
}

impl std::error::Error for ConfigError {}

/// A question's fields, as both tools declare them.
fn question_item() -> Value {
    json!({
        "type": "object",
        "additionalProperties": true,
        "properties": {
            "id": { "type": "string", "description": "Stable id for this question; echoed in the answer." },
            "question": { "type": "string", "description": "The specific question to ask the user." },
            "header": {
                "type": "string",
                "description": "Optional short heading for the question, such as \"Confirm\" or \"Choose Mode\".",
            },
            "options": {
                "type": "array",
                "description": "Optional choices to show the user. If you recommend one, put it first and append \"(Recommended)\" to that label.",
                "items": {
                    "type": "object",
                    "additionalProperties": true,
                    "properties": {
                        "label": { "type": "string", "description": "Short user-facing option label." },
                        "description": { "type": "string", "description": "One sentence explaining the tradeoff or impact." },
                    },
                    "required": ["label"],
                },
            },
            "multi_select": {
                "type": "boolean",
                "description": "Whether the user may select more than one option. Defaults to false.",
            },
        },
        "required": ["id", "question"],
    })
}

/// The parameters, as dsh's `defineTool` compiles each tool's spec: each
/// `required: true` becomes its parent's `required`, after `properties`;
/// an object's `additionalProperties` follows its `type` and annotations;
/// and the root stays open.
pub fn parameters(mode: Mode, default_timeout: i64) -> Value {
    match mode {
        Mode::Legacy => json!({
            "type": "object",
            "properties": {
                "questions": {
                    "type": "array",
                    "description": "Questions to ask the user before continuing.",
                    "items": question_item(),
                },
            },
            "required": ["questions"],
        }),
        Mode::Timed => json!({
            "type": "object",
            "properties": {
                "questions": {
                    "type": "array",
                    "description": "Questions to ask the user.",
                    "items": question_item(),
                },
                TIMED_WAIT_PARAMETER: {
                    "type": "integer",
                    "description": format!(
                        "Wait seconds for the entire batch (default {default_timeout}); omit unless the user specifies a duration. Use -1 only when an answer is required before proceeding."
                    ),
                },
            },
            "required": ["questions"],
        }),
    }
}

/// The tool. (dsh: the `defineTool` its `apply`, or `registerTimedAskUser`,
/// registers.)
pub struct AskUserTool {
    mode: Mode,
    default_timeout: i64,
    questions: Arc<UserQuestions>,
}

impl AskUserTool {
    /// The tool `config` picks, asking through `questions`.
    ///
    /// # Errors
    ///
    /// A default window dsh refuses, for the timed tool.
    pub fn new(config: AskUserConfig, questions: Arc<UserQuestions>) -> Result<Self, ConfigError> {
        if config.mode == Mode::Timed && valid_timeout(config.timeout as f64).is_none() {
            return Err(ConfigError);
        }
        Ok(Self {
            mode: config.mode,
            default_timeout: config.timeout,
            questions,
        })
    }

    async fn run(&self, arguments: Value, run: ToolRun) -> Result<Value, String> {
        let questions = questions_of(&arguments);
        match self.mode {
            Mode::Legacy => {
                let answer = self
                    .questions
                    .ask(QuestionRequest {
                        questions,
                        agent: run.agent,
                        ..QuestionRequest::default()
                    })
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(json!(TimedUserQuestionResult::Answered(answer)))
            }
            Mode::Timed => {
                let timeout = match arguments.get(TIMED_WAIT_PARAMETER) {
                    None => self.default_timeout,
                    Some(timeout) => timeout
                        .as_f64()
                        .and_then(valid_timeout)
                        .ok_or(BAD_TIMEOUT)?,
                };
                unique_ids(&questions)?;
                if timeout == -1 {
                    // `wait` without `timed`: the indefinite form still keys
                    // the UI's card by the call.
                    let answer = self
                        .questions
                        .ask(QuestionRequest {
                            questions,
                            agent: run.agent,
                            signal: None,
                            wait: Some(QuestionWait {
                                call_id: run.call_id,
                                timed: false,
                            }),
                        })
                        .await
                        .map_err(|error| error.to_string())?;
                    return Ok(json!(TimedUserQuestionResult::Answered(answer)));
                }
                let agent = run.agent.ok_or("timed questions require a live agent")?;
                let result = self
                    .questions
                    .ask_timed(agent, questions, None, &run.call_id, timeout as u64 * 1000)
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(match result {
                    TimedUserQuestionResult::Pending { call_id } => json!({
                        "pending": true,
                        "callId": call_id,
                        "message": PENDING_NOTICE,
                    }),
                    answered => json!(answered),
                })
            }
        }
    }
}

/// The questions in the arguments, in the service's words: `multi_select`
/// becomes `multiSelect`. The registry has checked their shape. Keys the
/// schema doesn't declare are dropped; dsh passes an option's through.
fn questions_of(arguments: &Value) -> Vec<AskUserQuestionItem> {
    let text = |value: &Value, key: &str| value.get(key).and_then(Value::as_str).map(str::to_owned);
    arguments
        .get("questions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|question| AskUserQuestionItem {
            header: text(question, "header"),
            options: question
                .get("options")
                .and_then(Value::as_array)
                .map(|options| {
                    options
                        .iter()
                        .map(|option| AskUserQuestionOption {
                            label: text(option, "label").unwrap_or_default(),
                            description: text(option, "description"),
                        })
                        .collect()
                }),
            multi_select: question.get("multi_select").and_then(Value::as_bool),
            ..AskUserQuestionItem::new(
                text(question, "id").unwrap_or_default(),
                text(question, "question").unwrap_or_default(),
            )
        })
        .collect()
}

/// dsh's `validateQuestionIds`.
fn unique_ids(questions: &[AskUserQuestionItem]) -> Result<(), String> {
    let mut seen = std::collections::HashSet::new();
    for question in questions {
        if !seen.insert(question.id.as_str()) {
            return Err(format!(
                "question id {} must be unique within this call",
                Value::String(question.id.clone())
            ));
        }
    }
    Ok(())
}

impl Tool for AskUserTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: NAME.into(),
            description: match self.mode {
                Mode::Legacy => LEGACY_DESCRIPTION,
                Mode::Timed => TIMED_DESCRIPTION,
            }
            .into(),
            parameters: parameters(self.mode, self.default_timeout).to_string(),
        }
    }

    fn execute<'a>(&'a self, arguments: Value, run: ToolRun) -> ToolFuture<'a> {
        Box::pin(async move {
            match self.run(arguments, run).await {
                // dsh's `render`: the value's JSON.
                Ok(value) => ToolOutput::ok(value.to_string()),
                // dsh's tool layer shows a thrown error as `Error: <message>`.
                Err(message) => ToolOutput::error(format!("Error: {message}")),
            }
        })
    }
}

/// Registers the tool `config` picks. It requires the tool registry and
/// the user-questions service. (dsh: the plugin's `apply`, with `inject`
/// of `tools` and `userQuestions`.)
///
/// dsh's registration is undone when its plugin unloads. Karaka's kernel
/// unloads no plugin, so the tool stays for the runtime's life.
#[derive(Clone, Copy, Debug, Default)]
pub struct AskUserPlugin {
    config: AskUserConfig,
}

impl AskUserPlugin {
    pub fn new(config: AskUserConfig) -> Self {
        Self { config }
    }
}

impl Plugin for AskUserPlugin {
    fn name(&self) -> &str {
        "tool-ask-user"
    }

    fn requires(&self) -> Vec<PortId> {
        vec![PortId::of::<ToolRegistry>(), PortId::of::<UserQuestions>()]
    }

    async fn setup(&self, ctx: &mut PluginContext<'_>) -> Result<(), BoxError> {
        let tool = AskUserTool::new(self.config, ctx.get::<UserQuestions>())?;
        ctx.get::<ToolRegistry>().register(Arc::new(tool))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::valid_timeout;

    #[test]
    fn a_timeout_is_minus_one_or_whole_seconds_in_range() {
        for (timeout, valid) in [
            (-1.0, Some(-1)),
            (1.0, Some(1)),
            (2_147_483.0, Some(2_147_483)),
            (0.0, None),
            (1.5, None),
            (-2.0, None),
            (2_147_484.0, None),
        ] {
            assert_eq!(valid_timeout(timeout), valid, "{timeout}");
        }
    }
}
