//! What the loop logs around each model call: the request's header, the
//! route's context, and an attempt that left no message.
//! (dsh v4: `request/header`, `request/context` and `assistant/attempt` in
//! `SessionEventMap`, `packages/core/session/src/types.ts`; the header's
//! helpers in `request-header.ts`; the call config in
//! `packages/llm/llm/src/call-config.ts`.)
//!
//! None of these change the history the model sees. The header records the
//! settings and tools a request was sent with, so anyone holding the log can
//! tell what each request was built under: the latest header is the one in
//! force. The context records facts about the route, and an attempt records
//! a model call that failed or was cancelled before it wrote anything.
//!
//! Like the other event data, each struct keeps the fields it doesn't name
//! in `extra`, so a v4 log writes back unchanged.

use karaka_model::ToolSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Number, Value};

use crate::data::{is_false, only_true};
use crate::message::Extra;

/// The `systemPromptUpdate` of a route that reads the latest system message,
/// wherever it is in the history, as the prompt in effect. The only value v4
/// defines (`SystemPromptUpdate` in `llm/src/types.ts`).
pub const IN_HISTORY: &str = "in-history";

/// dsh v4: `request/header`, `{ header, reason, startsSeries? }`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestHeaderData {
    pub header: RequestHeader,
    pub reason: RequestHeaderReason,
    /// This request begins a new series of messages, whatever the reason.
    /// v4 writes it only as `true`.
    #[serde(
        default,
        skip_serializing_if = "is_false",
        deserialize_with = "only_true"
    )]
    pub starts_series: bool,
    #[serde(flatten)]
    pub extra: Extra,
}

/// Why a `request/header` was logged. (dsh v4: `RequestHeaderReason`.)
///
/// v4 refuses any other reason when it loads a log, so this type does too.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RequestHeaderReason {
    /// The log's first header: a new conversation.
    Initial,
    /// A loop's first request over a log that already has a header, as
    /// after a restart.
    Resume,
    /// A later request used a different header.
    Change,
    /// The header is unchanged, but a new series of messages began, such as
    /// after part of the history was replaced.
    Series,
}

/// The settings and tools of a request. (dsh v4: `EpochHeader`.)
///
/// The system prompt isn't here: it is a `system/message` in the history.
/// v4 refuses a header that still carries the retired `system` field.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestHeader {
    pub config: CallConfig,
    /// Which config fields the provider filled in itself, rather than the
    /// caller asking for them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adapter_defaults: Option<AdapterDefaults>,
    /// The tools, in v4's JSON: `{ name, description, parameters,
    /// deferLoading? }`. Kept as raw JSON, so each keeps its key order: v4
    /// compares tools by their JSON text. Absent when there are none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Value>>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// The provider, model and sampling settings of a request.
/// (dsh: `LlmCallConfig` in `llm/src/call-config.ts`.)
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallConfig {
    /// The provider's own name, such as `anthropic`. In dsh this is the
    /// route; Karaka's loop writes the provider's name instead, as it does
    /// in an answer's `source.provider`.
    pub provider: String,
    pub model: String,
    /// The provider's own name for an effort level, such as `high`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// Kept as the number it was read as, so `1` doesn't come back `1.0`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<Number>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop: Option<Vec<String>>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// dsh: `LlmCallConfigAdapterDefaults`. Each marker is absent or `true`,
/// and v4 refuses any other key.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AdapterDefaults {
    #[serde(
        default,
        skip_serializing_if = "is_false",
        deserialize_with = "only_true"
    )]
    pub reasoning_effort: bool,
    #[serde(
        default,
        skip_serializing_if = "is_false",
        deserialize_with = "only_true"
    )]
    pub max_tokens: bool,
}

/// dsh v4: `request/context`, a route's facts: `{ provider, model,
/// contextWindow?, systemPromptUpdate? }`. Logged only when one of them
/// changes; it plays no part in rebuilding a request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestContext {
    pub provider: String,
    pub model: String,
    /// The most tokens a request and its answer may hold together, when the
    /// route says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    /// [`IN_HISTORY`] when the route reads a system message anywhere in the
    /// history as the prompt in effect. Kept as text, so a value v4 adds
    /// later reads back unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt_update: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// dsh v4: `assistant/attempt`, `{ turn, step, stream }`: a model call that
/// committed no message, because it failed, or was cancelled before anything
/// worth keeping arrived.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AssistantAttemptData {
    pub turn: u64,
    pub step: u64,
    /// The stream the model sent, in v4's compact records, as raw JSON. A
    /// failed call's ends with a `finish` of kind `error`; see `stream.rs`.
    pub stream: Vec<Value>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl CallConfig {
    /// A config naming only the provider and the model.
    pub fn new(provider: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
            reasoning_effort: None,
            temperature: None,
            max_tokens: None,
            stop: None,
            extra: Extra::new(),
        }
    }
}

/// Whether two configs ask for the same thing, field by field.
/// (dsh: `callConfigEquals`, `call-config.ts`.) Fields v4 doesn't name are
/// left out, as dsh leaves them out.
pub fn call_config_equals(a: &CallConfig, b: &CallConfig) -> bool {
    // JavaScript compares numbers by value, so `1` and `1.0` are equal.
    let temperature = |config: &CallConfig| config.temperature.as_ref().and_then(Number::as_f64);
    a.provider == b.provider
        && a.model == b.model
        && a.reasoning_effort == b.reasoning_effort
        && temperature(a) == temperature(b)
        && a.max_tokens == b.max_tokens
        && a.stop == b.stop
}

impl RequestHeader {
    /// The canonical header for a request with `config` and `tools`, as the
    /// loop logs it. Fails only if a tool's parameters aren't JSON.
    pub fn new(config: CallConfig, tools: &[ToolSchema]) -> Result<Self, serde_json::Error> {
        let tools = tools.iter().map(tool_json).collect::<Result<Vec<_>, _>>()?;
        Ok(canonical_header(&Self {
            config,
            adapter_defaults: None,
            tools: Some(tools),
            extra: Extra::new(),
        }))
    }
}

/// A tool in v4's JSON, `{ name, description, parameters }`, in the order
/// dsh's tool registry writes it (`schemaOf`, `tools/src/index.ts`).
fn tool_json(schema: &ToolSchema) -> Result<Value, serde_json::Error> {
    let parameters: Value = serde_json::from_str(&schema.parameters)?;
    let mut tool = serde_json::Map::new();
    tool.insert("name".into(), Value::String(schema.name.clone()));
    tool.insert(
        "description".into(),
        Value::String(schema.description.clone()),
    );
    tool.insert("parameters".into(), parameters);
    Ok(Value::Object(tool))
}

/// `header` in canonical form: no adapter defaults unless one is marked, no
/// tool list if it is empty, and nothing v4 doesn't name. Logging, folding
/// and comparing all use this form. (dsh: `canonicalHeader`,
/// `request-header.ts`.)
pub fn canonical_header(header: &RequestHeader) -> RequestHeader {
    RequestHeader {
        config: header.config.clone(),
        adapter_defaults: header
            .adapter_defaults
            .filter(|defaults| defaults.reasoning_effort || defaults.max_tokens),
        tools: header.tools.clone().filter(|tools| !tools.is_empty()),
        extra: Extra::new(),
    }
}

/// Whether two canonical headers are the same: the config, the adapter
/// defaults, and the tools in order, each compared by its JSON text.
/// (dsh: `headerEquals`, `request-header.ts`.)
pub fn header_equals(a: &RequestHeader, b: &RequestHeader) -> bool {
    let defaults = |header: &RequestHeader| header.adapter_defaults.unwrap_or_default();
    let tools = |header: &RequestHeader| header.tools.clone().unwrap_or_default();
    let (a_tools, b_tools) = (tools(a), tools(b));
    call_config_equals(&a.config, &b.config)
        && defaults(a) == defaults(b)
        && a_tools.len() == b_tools.len()
        && a_tools
            .iter()
            .zip(&b_tools)
            .all(|(a, b)| json_text(a) == json_text(b))
}

/// `value` as JSON text, keys in their order. Comparing two of these is
/// JavaScript's `JSON.stringify(a) === JSON.stringify(b)`; comparing the
/// values themselves would ignore key order.
fn json_text(value: &Value) -> String {
    value.to_string()
}

/// The checks v4 makes on a `request/header` as it is appended or loaded
/// (`validateSessionEventData` in `surface.ts`, and `assertCurrentLlmShape`
/// and `assertAdapterDefaults` in `index.ts`).
pub(crate) fn check_header(data: &RequestHeaderData) -> Result<(), String> {
    let header = &data.header;
    if header.extra.contains_key("system") {
        return Err("request/header must omit header.system; use system/message".into());
    }
    if header.tools.as_ref().is_some_and(Vec::is_empty) {
        return Err("request/header must omit empty tools".into());
    }
    if let Some(defaults) = header.adapter_defaults {
        if !defaults.reasoning_effort && !defaults.max_tokens {
            return Err("request/header must omit empty adapterDefaults".into());
        }
        if (defaults.reasoning_effort && header.config.reasoning_effort.is_none())
            || (defaults.max_tokens && header.config.max_tokens.is_none())
        {
            return Err("request/header has invalid adapterDefaults".into());
        }
    }
    if header.config.provider.is_empty() || header.config.model.is_empty() {
        return Err("request/header lacks provider/model".into());
    }
    if header.config.reasoning_effort.as_deref() == Some("") {
        return Err("request/header has an invalid reasoningEffort".into());
    }
    Ok(())
}

/// Ported from dsh v4 `packages/core/session/tests/request-header.spec.ts`.
#[cfg(test)]
mod tests {
    use karaka_model::ToolSchema;
    use serde_json::{Value, json};

    use super::{
        AdapterDefaults, CallConfig, RequestHeader, call_config_equals, canonical_header,
        header_equals,
    };
    use crate::message::Extra;

    fn config() -> CallConfig {
        CallConfig::new("mock", "m")
    }

    fn tool(name: &str, description: &str) -> Value {
        json!({ "name": name, "description": description, "parameters": { "type": "object" } })
    }

    fn header(config: CallConfig, tools: Option<Vec<Value>>) -> RequestHeader {
        RequestHeader {
            config,
            adapter_defaults: None,
            tools,
            extra: Extra::new(),
        }
    }

    // "normalizes empty optional fields to absence and preserves populated
    // fields"
    #[test]
    fn canonical_form_drops_empty_fields_and_keeps_full_ones() {
        let mut empty = header(config(), Some(Vec::new()));
        empty.adapter_defaults = Some(AdapterDefaults::default());
        empty.extra.insert("note".into(), json!(true));
        assert_eq!(canonical_header(&empty), header(config(), None));

        let mut full = header(
            CallConfig {
                max_tokens: Some(256_000),
                ..config()
            },
            Some(vec![tool("a", "d")]),
        );
        full.adapter_defaults = Some(AdapterDefaults {
            reasoning_effort: false,
            max_tokens: true,
        });
        assert_eq!(canonical_header(&full), full);
    }

    // "compares every canonical field and preserves tool order"
    #[test]
    fn equality_compares_every_canonical_field_and_tool_order() {
        let base = header(config(), Some(vec![tool("a", "d")]));
        assert!(header_equals(&base, &base.clone()));

        let other_model = header(CallConfig::new("mock", "other"), base.tools.clone());
        assert!(!header_equals(&base, &other_model));

        let effort = header(
            CallConfig {
                reasoning_effort: Some("high".into()),
                ..config()
            },
            base.tools.clone(),
        );
        assert!(!header_equals(&base, &effort));

        let limited = header(
            CallConfig {
                max_tokens: Some(256_000),
                ..config()
            },
            base.tools.clone(),
        );
        let mut defaulted = limited.clone();
        defaulted.adapter_defaults = Some(AdapterDefaults {
            reasoning_effort: false,
            max_tokens: true,
        });
        assert!(!header_equals(&limited, &defaulted));

        assert!(!header_equals(&base, &header(config(), Some(Vec::new()))));
        assert!(!header_equals(
            &base,
            &header(config(), Some(vec![tool("a", "changed")]))
        ));
        assert!(!header_equals(
            &header(config(), Some(vec![tool("a", "d"), tool("b", "d")])),
            &header(config(), Some(vec![tool("b", "d"), tool("a", "d")])),
        ));
    }

    // "treats absent and empty tool arrays as equivalent canonical absence"
    #[test]
    fn absent_and_empty_tools_are_equal() {
        let absent = header(config(), None);
        let empty = header(config(), Some(Vec::new()));
        assert!(header_equals(&absent, &empty));
        assert!(header_equals(&empty, &absent));
    }

    /// v4 compares tools by their JSON text, so the same keys in another
    /// order are a different tool.
    #[test]
    fn tools_compare_by_their_json_text() {
        let reordered: Value = serde_json::from_str(
            r#"{"name":"a","parameters":{"type":"object"},"description":"d"}"#,
        )
        .unwrap();
        assert!(!header_equals(
            &header(config(), Some(vec![tool("a", "d")])),
            &header(config(), Some(vec![reordered])),
        ));
    }

    #[test]
    fn a_temperature_compares_by_value() {
        let one = |text: &str| CallConfig {
            temperature: Some(serde_json::from_str(text).unwrap()),
            ..config()
        };
        assert!(call_config_equals(&one("1"), &one("1.0")));
        assert!(!call_config_equals(&one("1"), &config()));
    }

    #[test]
    fn a_new_header_writes_tools_as_dsh_does() {
        let schema = ToolSchema {
            name: "clock".into(),
            description: "Reads the clock.".into(),
            parameters: r#"{"type":"object","properties":{"zone":{"type":"string"}}}"#.into(),
        };
        let header = RequestHeader::new(config(), &[schema]).unwrap();
        assert_eq!(
            serde_json::to_string(&header).unwrap(),
            concat!(
                r#"{"config":{"provider":"mock","model":"m"},"tools":[{"name":"clock","#,
                r#""description":"Reads the clock.","parameters":{"type":"object","#,
                r#""properties":{"zone":{"type":"string"}}}}]}"#
            )
        );
        // No tools, no list.
        let bare = RequestHeader::new(config(), &[]).unwrap();
        assert_eq!(bare.tools, None);
        assert!(
            RequestHeader::new(
                config(),
                &[ToolSchema {
                    name: "x".into(),
                    description: String::new(),
                    parameters: "not json".into(),
                }]
            )
            .is_err()
        );
    }
}
