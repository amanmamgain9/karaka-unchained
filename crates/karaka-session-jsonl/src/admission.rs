//! The structural checks format v4 runs on every physical row, before a
//! torn tail can be recovered. (dsh: `assertV4RowAdmission` in
//! `session-format-v3-to-v4/src/codec.ts`, and the checks it calls in
//! `developer.ts`, `message-sources.ts`, `retired-syntax.ts`,
//! `system-message.ts`, `tool-role.ts` and `fork-result.ts`.)
//!
//! They refuse shapes v4 retired or never allows: the PTC dispatch tags, a
//! `request/header` with `header.system`, released tool-result wrappers in
//! content, plugin-attributed messages, malformed system and developer
//! messages, and tool results that aren't first-class tool messages. A
//! refused row refuses the whole file, even after an earlier damaged row:
//! recovery never truncates it as an ordinary damaged tail.
//!
//! They run on raw JSON, as dsh's do, so additional fields pass through.
//! The writer runs them too, so this build never stores a row it would
//! refuse to read.

use serde_json::{Map, Value};

use karaka_session_persistence::MAX_SAFE_INTEGER;

/// Why a row was refused.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// A shape this build won't interpret. (dsh:
    /// `SessionFormatUnsupportedMigrationError`.)
    Unsupported(String),
    /// A malformed row. (dsh: `SessionFormatError`.)
    Malformed(String),
}

impl Refusal {
    pub(crate) fn message(&self) -> &str {
        match self {
            Self::Unsupported(message) | Self::Malformed(message) => message,
        }
    }
}

type Check = Result<(), Refusal>;

fn malformed(message: impl Into<String>) -> Refusal {
    Refusal::Malformed(message.into())
}

fn object(value: Option<&Value>) -> Option<&Map<String, Value>> {
    value.and_then(Value::as_object)
}

fn text_of<'a>(value: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn nonempty(value: Option<&Value>) -> bool {
    value
        .and_then(Value::as_str)
        .is_some_and(|text| !text.is_empty())
}

/// A non-negative safe integer, as dsh's `sessionFormatCount` reads one.
pub(crate) fn count(value: Option<&Value>) -> Option<u64> {
    let number = value?.as_number()?;
    if let Some(count) = number.as_u64() {
        return (count <= MAX_SAFE_INTEGER).then_some(count);
    }
    // JavaScript reads `1.0` and `1e3` as integers too, but not `-0`.
    let float = number.as_f64()?;
    let whole = float.fract() == 0.0 && float >= 0.0 && !float.is_sign_negative();
    (whole && float <= MAX_SAFE_INTEGER as f64).then_some(float as u64)
}

fn count_of(value: Option<&Value>, label: &str) -> Result<u64, Refusal> {
    count(value).ok_or_else(|| malformed(format!("{label} must be a non-negative safe integer")))
}

/// Runs every v4 admission check on one parsed row. (dsh:
/// `assertV4RowAdmission`.)
pub(crate) fn admit_row(row: &Value) -> Check {
    if row.is_object() {
        developer_data(row)?;
    }
    source_row(row)?;
    retired_syntax(row)?;
    system_message_fields(row)?;
    if row.get("type").and_then(Value::as_str) == Some("tool/result") {
        tool_result_message(row)?;
        fork_result(row)?;
    }
    Ok(())
}

// --- developer.ts ---

fn tool_change(block: Option<&Value>, developer: bool) -> Check {
    let Some(block) = object(block) else {
        return Ok(());
    };
    let kind = text_of(block, "type").unwrap_or_default();
    if kind != "tool-addition" && kind != "tool-removal" {
        return Ok(());
    }
    if !developer {
        return Err(malformed(format!(
            "format v4 {kind} requires developer role"
        )));
    }
    if !nonempty(block.get("toolName")) {
        return Err(malformed(format!(
            "format v4 {kind} requires a nonempty toolName"
        )));
    }
    if kind == "tool-addition" && block.contains_key("tool") {
        return Err(malformed(
            "format v4 tool-addition must omit inline tool definitions",
        ));
    }
    Ok(())
}

fn tool_changes(content: Option<&Value>, developer: bool) -> Check {
    if let Some(blocks) = content.and_then(Value::as_array) {
        for block in blocks {
            tool_change(Some(block), developer)?;
        }
    }
    Ok(())
}

fn developer_message(message: &Map<String, Value>) -> Check {
    tool_changes(message.get("content"), true)?;
    let source = object(message.get("source"));
    let kind = source.and_then(|source| text_of(source, "kind"));
    if !nonempty(message.get("id"))
        || !message.get("content").is_some_and(Value::is_array)
        || kind.is_none_or(|kind| kind.is_empty() || kind == "plugin")
    {
        return Err(malformed(
            "format v4 developer message requires id, role, content, and a producer-owned source",
        ));
    }
    Ok(())
}

/// A message slot that isn't a developer message.
fn ordinary_message(value: Option<&Value>) -> Check {
    let Some(message) = object(value) else {
        return Ok(());
    };
    if text_of(message, "role") == Some("developer") {
        return Err(malformed(
            "format v4 developer/message and developer role must occur together",
        ));
    }
    tool_changes(message.get("content"), false)
}

/// (dsh: `assertV4DeveloperData`.)
fn developer_data(row: &Value) -> Check {
    let kind = row.get("type").and_then(Value::as_str).unwrap_or_default();
    let Some(data) = object(row.get("data")) else {
        if kind == "developer/message" {
            return Err(malformed(
                "format v4 developer/message data must be an object",
            ));
        }
        return Ok(());
    };
    match kind {
        "developer/message" => {
            let message = object(data.get("message"))
                .filter(|message| text_of(message, "role") == Some("developer"))
                .ok_or_else(|| {
                    malformed(
                        "format v4 developer/message requires turn, step, and a developer message",
                    )
                })?;
            for field in ["turn", "step"] {
                if count_of(data.get(field), &format!("developer/message {field}"))? == 0 {
                    return Err(malformed(format!(
                        "developer/message {field} must be positive"
                    )));
                }
            }
            developer_message(message)?;
            let additions = message
                .get("content")
                .and_then(Value::as_array)
                .is_some_and(|blocks| {
                    blocks
                        .iter()
                        .any(|block| block.get("type") == Some(&Value::from("tool-addition")))
                });
            if additions {
                count_of(data.get("headerSeq"), "developer/message headerSeq")?;
            } else if data.contains_key("headerSeq") {
                return Err(malformed(
                    "format v4 developer/message must omit headerSeq without tool additions",
                ));
            }
        }
        "user/message" => ordinary_message(row.get("data"))?,
        "system/message" | "assistant/message" | "tool/result" => {
            ordinary_message(data.get("message"))?;
        }
        "agent/inbox/spliced" | "session/title-llm-request" => {
            let key = if kind == "agent/inbox/spliced" {
                "inserted"
            } else {
                "messages"
            };
            if let Some(messages) = data.get(key).and_then(Value::as_array) {
                for message in messages {
                    ordinary_message(Some(message))?;
                }
            }
        }
        _ => {}
    }
    if kind == "compaction/summary" {
        tool_changes(data.get("summary"), false)?;
        tool_changes(data.get("rawOutput"), false)?;
    }
    if kind == "assistant/message" || kind == "assistant/attempt" {
        for chunk in stream_chunks(data) {
            match text_of(chunk, "type") {
                Some("block-end") => tool_change(chunk.get("block"), false)?,
                Some("block-start") => {
                    let block_type = text_of(chunk, "blockType");
                    if block_type == Some("tool-addition") || block_type == Some("tool-removal") {
                        return Err(malformed(
                            "format v4 tool-change blocks require developer role",
                        ));
                    }
                }
                _ => {}
            }
        }
    }
    if kind == "request/header"
        && let Some(tools) = object(data.get("header"))
            .and_then(|header| header.get("tools"))
            .and_then(Value::as_array)
    {
        for tool in tools.iter().filter_map(Value::as_object) {
            if let Some(defer) = tool.get("deferLoading")
                && defer != &Value::Bool(true)
            {
                return Err(malformed(
                    "format v4 tool deferLoading must be true when present",
                ));
            }
        }
    }
    Ok(())
}

/// The raw chunks of an embedded `stream`, skipping its packed records.
fn stream_chunks(data: &Map<String, Value>) -> impl Iterator<Item = &Map<String, Value>> {
    data.get("stream")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_object)
        .filter(|entry| text_of(entry, "type") == Some("chunk"))
        .filter_map(|entry| object(entry.get("chunk")))
}

// --- message-sources.ts ---

/// Refuses a message attributed to a retired plugin wrapper. (dsh:
/// `assertV4SourceRowAdmission`.)
fn source_row(row: &Value) -> Check {
    let Some(data) = object(row.get("data")) else {
        return Ok(());
    };
    let kind = row.get("type").and_then(Value::as_str).unwrap_or_default();
    let messages: Vec<&Value> = match kind {
        "user/message" => vec![row.get("data").unwrap_or(&Value::Null)],
        "system/message" | "assistant/message" | "tool/result" => {
            data.get("message").into_iter().collect()
        }
        "agent/inbox/spliced" => list(data.get("inserted")),
        "session/title-llm-request" => list(data.get("messages")),
        _ => Vec::new(),
    };
    for message in messages.into_iter().filter_map(Value::as_object) {
        if object(message.get("source")).and_then(|source| text_of(source, "kind"))
            == Some("plugin")
        {
            return Err(malformed(
                "format v4 message requires a producer-owned source kind",
            ));
        }
    }
    Ok(())
}

fn list(value: Option<&Value>) -> Vec<&Value> {
    value
        .and_then(Value::as_array)
        .map(|values| values.iter().collect())
        .unwrap_or_default()
}

// --- retired-syntax.ts ---

/// (dsh: `assertV4RetiredSyntax`.)
fn retired_syntax(row: &Value) -> Check {
    let Some(record) = row.as_object() else {
        return Ok(());
    };
    let kind = text_of(record, "type").unwrap_or_default();
    if (kind == "tool/code-dispatch-start" || kind == "tool/code-dispatch")
        && record.get("ignorable") != Some(&Value::Bool(true))
    {
        return Err(Refusal::Unsupported(format!(
            "format v4 rejects retired event type {kind}"
        )));
    }
    let data = object(record.get("data"));
    if kind == "request/header" {
        let header = data
            .and_then(|data| object(data.get("header")))
            .ok_or_else(|| {
                malformed("format v4 request/header requires data and header objects")
            })?;
        if header.contains_key("system") {
            return Err(malformed(
                "format v4 request/header rejects retired header.system",
            ));
        }
        return Ok(());
    }
    let Some(data) = data else {
        return Ok(());
    };
    let seq = record
        .get("seq")
        .map_or_else(|| "undefined".to_owned(), Value::to_string);
    let subject = format!("format v4 {kind} at seq {seq} content");
    let wrapper = || {
        malformed(format!(
            "{subject} must not contain a released tool-result wrapper"
        ))
    };
    let block = |block: Option<&Value>| -> Check {
        match object(block) {
            Some(block) if text_of(block, "type") == Some("tool-result") => Err(wrapper()),
            _ => Ok(()),
        }
    };
    let content = |value: Option<&Value>| -> Check {
        for item in list(value) {
            block(Some(item))?;
        }
        Ok(())
    };
    let message_content = |message: Option<&Value>| -> Check {
        content(object(message).and_then(|m| m.get("content")))
    };
    match kind {
        "user/message" => content(data.get("content"))?,
        "developer/message" | "assistant/message" | "team/message/queued" => {
            message_content(data.get("message"))?;
        }
        "agent/inbox/spliced" | "session/title-llm-request" => {
            let key = if kind == "agent/inbox/spliced" {
                "inserted"
            } else {
                "messages"
            };
            for message in list(data.get(key)) {
                message_content(Some(message))?;
            }
        }
        "compaction/summary" => {
            content(data.get("summary"))?;
            content(data.get("rawOutput"))?;
        }
        "tool/ptc-dispatch" => content(data.get("content"))?,
        _ => {}
    }
    if kind == "assistant/message" || kind == "assistant/attempt" {
        for chunk in stream_chunks(data) {
            match text_of(chunk, "type") {
                Some("block-end") => block(chunk.get("block"))?,
                Some("block-start") if text_of(chunk, "blockType") == Some("tool-result") => {
                    return Err(wrapper());
                }
                _ => {}
            }
        }
    }
    Ok(())
}

// --- system-message.ts ---

fn record<'a>(value: Option<&'a Value>, subject: &str) -> Result<&'a Map<String, Value>, Refusal> {
    object(value).ok_or_else(|| malformed(format!("format v4 {subject} requires an object")))
}

fn string(value: Option<&Value>, subject: &str, with_content: bool) -> Check {
    match value.and_then(Value::as_str) {
        Some(text) if !(with_content && text.is_empty()) => Ok(()),
        _ => Err(malformed(format!(
            "format v4 {subject} requires a string{}",
            if with_content { " with content" } else { "" }
        ))),
    }
}

fn positive(value: Option<&Value>, subject: &str) -> Check {
    if count_of(value, subject)? == 0 {
        return Err(malformed(format!("format v4 {subject} must be positive")));
    }
    Ok(())
}

fn image(value: Option<&Value>) -> Check {
    let attachment = record(value, "system image attachment")?;
    string(
        attachment.get("attachmentId"),
        "system image attachmentId",
        true,
    )?;
    let media = text_of(attachment, "mediaType").unwrap_or_default();
    if !["image/png", "image/jpeg", "image/webp", "image/gif"].contains(&media) {
        return Err(malformed(
            "format v4 system image requires a supported mediaType",
        ));
    }
    count_of(attachment.get("bytes"), "system image bytes")?;
    positive(attachment.get("width"), "system image width")?;
    positive(attachment.get("height"), "system image height")?;
    if let Some(name) = attachment.get("name") {
        string(Some(name), "system image name", false)?;
    }
    if let Some(original) = attachment.get("originalDimensions") {
        let original = record(Some(original), "system image originalDimensions")?;
        positive(original.get("width"), "system image original width")?;
        positive(original.get("height"), "system image original height")?;
    }
    Ok(())
}

/// (dsh: `assertV4SystemMessageFields`.)
fn system_message_fields(row: &Value) -> Check {
    if row.get("type").and_then(Value::as_str) != Some("system/message") {
        return Ok(());
    }
    let data = record(row.get("data"), "system/message data")?;
    positive(data.get("turn"), "system/message turn")?;
    positive(data.get("step"), "system/message step")?;
    let message = record(data.get("message"), "system message")?;
    string(message.get("id"), "system message id", true)?;
    if text_of(message, "role") != Some("system") {
        return Err(malformed("format v4 system message requires system role"));
    }
    let content = message
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| malformed("format v4 system message requires content array"))?;
    for value in content {
        let block = record(Some(value), "system content block")?;
        string(block.get("type"), "system content type", true)?;
        match text_of(block, "type") {
            Some("text" | "reasoning") => string(block.get("text"), "system content text", false)?,
            Some("tool-call") => {
                string(block.get("id"), "system tool-call id", true)?;
                string(block.get("name"), "system tool-call name", true)?;
                string(block.get("arguments"), "system tool-call arguments", false)?;
            }
            Some("image") => image(block.get("attachment"))?,
            Some("tool-result") => {
                return Err(malformed(
                    "format v4 system content rejects retired tool-result wrappers",
                ));
            }
            // Content blocks are open to plugins; only known fields are read.
            _ => {}
        }
    }
    Ok(())
}

// --- tool-role.ts ---

/// A `tool/result` must hold a first-class tool message. (dsh:
/// `assertV4ToolResultMessage`.)
fn tool_result_message(row: &Value) -> Check {
    let seq = row
        .get("seq")
        .map_or_else(|| "undefined".to_owned(), Value::to_string);
    let subject = format!("format v4 tool/result at seq {seq}");
    let fail = |what: &str| malformed(format!("{subject} {what}"));
    let data = object(row.get("data")).ok_or_else(|| fail("data must be an object"))?;
    let message = object(data.get("message")).ok_or_else(|| fail("message must be an object"))?;
    let source = object(message.get("source"));
    let source_call = source.and_then(|source| source.get("callId"));
    if !nonempty(message.get("id")) {
        return Err(fail("requires a first-class message with a string id"));
    }
    if text_of(message, "role") != Some("tool") {
        return Err(fail("requires a tool-role message"));
    }
    let call = message.get("toolCallId");
    if !nonempty(call) || source_call != call {
        return Err(fail("requires toolCallId matching its tool source"));
    }
    if source.and_then(|source| text_of(source, "kind")) != Some("tool") {
        return Err(fail("requires a tool source"));
    }
    let content = message
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| fail("requires array content"))?;
    if content
        .iter()
        .any(|block| block.get("type") == Some(&Value::from("tool-result")))
    {
        return Err(fail(
            "content must not contain a released tool-result wrapper",
        ));
    }
    let is_error = message.get("isError");
    if is_error.is_some_and(|flag| !flag.is_boolean()) {
        return Err(fail("isError must be boolean when present"));
    }
    if data.contains_key("error") && is_error != Some(&Value::Bool(true)) {
        return Err(fail("carries error metadata for a non-error tool result"));
    }
    Ok(())
}

// --- fork-result.ts ---

/// A fork's not-started result keeps its exact identity and text. (dsh:
/// `assertV4ForkResult`.)
fn fork_result(row: &Value) -> Check {
    let Some(data) = object(row.get("data")) else {
        return Ok(());
    };
    let error = object(data.get("error"));
    let message = object(data.get("message"));
    let (Some(error), Some(message)) = (error, message) else {
        return Ok(());
    };
    let Some(id) = text_of(message, "id") else {
        return Ok(());
    };
    if text_of(error, "code") != Some("TOOL_NOT_STARTED") || !id.starts_with("forked-tool-result-")
    {
        return Ok(());
    }
    let invalid = || malformed("invalid V4 not-started fork result");
    let source = object(message.get("source"));
    let call = source.and_then(|source| text_of(source, "callId"));
    let Some(call) = call else {
        return Err(invalid());
    };
    let prefix = format!("forked-tool-result-{call}-");
    let Some(suffix) = id.strip_prefix(&prefix) else {
        return Err(invalid());
    };
    let canonical = suffix == "0" || (!suffix.starts_with('0') && !suffix.is_empty());
    let sequence = suffix.parse::<u64>().ok().filter(|sequence| {
        canonical && *sequence <= MAX_SAFE_INTEGER && suffix.bytes().all(|b| b.is_ascii_digit())
    });
    let Some(sequence) = sequence else {
        return Err(invalid());
    };
    let operation = row.get("surfaceOp");
    let replacement = object(operation).and_then(|op| text_of(op, "op")) == Some("replace");
    let seq = row.get("seq").and_then(Value::as_u64);
    let sources = row.get("sourceEventSeqs");
    let text = message
        .get("content")
        .and_then(Value::as_array)
        .filter(|content| content.len() == 1)
        .and_then(|content| content[0].as_object());
    let ordered = if replacement {
        seq.is_some_and(|seq| sequence < seq)
    } else {
        seq == Some(sequence)
    };
    let placed = if replacement {
        sources
            .and_then(Value::as_array)
            .is_some_and(|sources| sources.len() == 1 && sources[0].as_u64() == Some(sequence))
    } else {
        sources.is_none() && operation == Some(&Value::from("append"))
    };
    let valid = ordered
        && placed
        && text_of(error, "name") == Some("ToolNotStartedError")
        && text_of(message, "role") == Some("tool")
        && message.get("isError") == Some(&Value::Bool(true))
        && text_of(message, "toolCallId") == Some(call)
        && source.and_then(|source| text_of(source, "kind")) == Some("tool")
        && text.is_some_and(|text| {
            text_of(text, "type") == Some("text") && text.get("text").is_some_and(Value::is_string)
        });
    if valid { Ok(()) } else { Err(invalid()) }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn refusal(row: Value) -> String {
        admit_row(&row).unwrap_err().message().to_owned()
    }

    #[test]
    fn retired_ptc_tags_are_unsupported_unless_ignorable() {
        let row = json!({ "type": "tool/code-dispatch", "seq": 1, "time": 2, "data": {} });
        assert_eq!(
            admit_row(&row),
            Err(Refusal::Unsupported(
                "format v4 rejects retired event type tool/code-dispatch".into()
            ))
        );
        let ignorable = json!({
            "type": "tool/code-dispatch-start", "seq": 1, "time": 2, "data": {}, "ignorable": true
        });
        assert_eq!(admit_row(&ignorable), Ok(()));
    }

    #[test]
    fn a_request_header_may_not_carry_the_retired_system() {
        let row = json!({
            "type": "request/header", "seq": 1, "time": 2,
            "data": { "header": { "system": "be brief", "config": {} } }
        });
        assert_eq!(
            refusal(row),
            "format v4 request/header rejects retired header.system"
        );
    }

    #[test]
    fn released_tool_result_wrappers_are_refused_in_content() {
        let row = json!({
            "type": "user/message", "seq": 3, "time": 2,
            "data": { "id": "u", "role": "user", "source": { "kind": "user" },
                      "content": [{ "type": "tool-result", "toolCallId": "c" }] }
        });
        assert_eq!(
            refusal(row),
            "format v4 user/message at seq 3 content must not contain a released tool-result wrapper"
        );
    }

    #[test]
    fn plugin_sources_and_malformed_system_messages_are_refused() {
        let plugin = json!({
            "type": "user/message", "seq": 0, "time": 1,
            "data": { "id": "u", "role": "user", "source": { "kind": "plugin" }, "content": [] }
        });
        assert_eq!(
            refusal(plugin),
            "format v4 message requires a producer-owned source kind"
        );
        let system = json!({
            "type": "system/message", "seq": 0, "time": 1,
            "data": { "turn": 0, "step": 1, "message": {} }
        });
        assert_eq!(
            refusal(system),
            "format v4 system/message turn must be positive"
        );
    }

    #[test]
    fn a_tool_result_must_be_a_first_class_tool_message() {
        let row = json!({ "type": "tool/result", "seq": 1, "time": 2, "data": { "message": [] } });
        assert_eq!(
            refusal(row),
            "format v4 tool/result at seq 1 message must be an object"
        );
        let ok = json!({
            "type": "tool/result", "seq": 1, "time": 2, "surfaceOp": "append",
            "data": { "turn": 1, "step": 1, "message": {
                "id": "r", "role": "tool", "toolCallId": "c",
                "source": { "kind": "tool", "callId": "c" }, "content": []
            } }
        });
        assert_eq!(admit_row(&ok), Ok(()));
    }

    #[test]
    fn counts_read_as_javascript_reads_them() {
        assert_eq!(count(Some(&json!(3))), Some(3));
        assert_eq!(count(Some(&json!(1.0))), Some(1));
        assert_eq!(count(Some(&json!(1.5))), None);
        assert_eq!(count(Some(&json!(-1))), None);
        assert_eq!(
            count(Some(&serde_json::from_str::<Value>("-0").unwrap())),
            None
        );
        assert_eq!(count(Some(&json!(MAX_SAFE_INTEGER + 1))), None);
    }
}
