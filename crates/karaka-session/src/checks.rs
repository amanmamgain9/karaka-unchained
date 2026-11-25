//! The checks an event passes when it joins a log, beyond its seq and its
//! surface operation (`surface.rs`). (dsh v4: `validateSessionEventData`,
//! `assertDeveloperHeader` and the event-local part of `surfaceOpOf`, in
//! `core/session/src/surface.ts`.)
//!
//! They read the event's JSON, as dsh's do, so blocks Karaka keeps as
//! [`ContentBlock::Other`](crate::ContentBlock::Other), such as tool
//! changes, are checked too.

use serde_json::Value;

use crate::event::{Entry, Seq, SessionEvent};
use crate::header::is_known_event_type;
use crate::json::{not_surface_eligible, surface_field};
use crate::request;

/// Checks `entry` as the event after `log`.
pub(crate) fn check_entry(log: &[Entry], entry: &Entry) -> Result<(), String> {
    let event = &entry.event;
    if let SessionEvent::Passthrough(passthrough) = event {
        // Only an unknown event its writer marked ignorable keeps opaque
        // surface fields; it never joins the surface.
        let opaque = !is_known_event_type(&passthrough.event_type) && entry.ignorable;
        if !opaque
            && let Some(field) =
                surface_field(&passthrough.surface_op, &passthrough.source_event_seqs)
        {
            return Err(not_surface_eligible(&passthrough.event_type, field));
        }
    }
    if let SessionEvent::RequestHeader(data) = event {
        request::check_header(data)?;
    }
    if let SessionEvent::ToolResult { data, .. } = event
        && data.error.is_some()
        && data.message.is_error != Some(true)
    {
        return Err("error requires message.isError === true".into());
    }
    check_tool_changes(event)?;
    if let SessionEvent::DeveloperMessage { data, .. } = event {
        check_developer_header(log, entry.seq, data)?;
    }
    Ok(())
}

/// The message of a surface event, as JSON, and whether it is a developer
/// message.
fn message_json(event: &SessionEvent) -> Option<(Value, bool)> {
    let message = match event {
        SessionEvent::SystemMessage { data, .. } => serde_json::to_value(&data.message),
        SessionEvent::DeveloperMessage { data, .. } => {
            return serde_json::to_value(&data.message)
                .ok()
                .map(|message| (message, true));
        }
        SessionEvent::UserMessage { message, .. } => serde_json::to_value(message),
        SessionEvent::AssistantMessage { data, .. } => serde_json::to_value(&data.message),
        SessionEvent::ToolResult { data, .. } => serde_json::to_value(&data.message),
        _ => return None,
    };
    message.ok().map(|message| (message, false))
}

fn is_tool_change(block: &Value) -> bool {
    matches!(
        block.get("type").and_then(Value::as_str),
        Some("tool-addition" | "tool-removal")
    )
}

/// Tool changes belong to developer messages, each names its tool, an
/// addition carries no inline definition, and `headerSeq` is there exactly
/// when something is added. (dsh v4: `validateSessionEventData`.)
fn check_tool_changes(event: &SessionEvent) -> Result<(), String> {
    let Some((message, developer)) = message_json(event) else {
        return Ok(());
    };
    let Some(content) = message.get("content").and_then(Value::as_array) else {
        return Ok(());
    };
    if !developer {
        if content.iter().any(is_tool_change) {
            return Err("tool-change blocks require developer role".into());
        }
        return Ok(());
    }
    let mut additions = false;
    for block in content.iter().filter(|block| is_tool_change(block)) {
        let kind = block["type"].as_str().unwrap_or_default();
        if block
            .get("toolName")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        {
            return Err(format!("{kind} requires a nonempty toolName"));
        }
        if kind == "tool-addition" {
            additions = true;
            if block.get("tool").is_some() {
                return Err("tool-addition must omit inline tool definitions".into());
            }
        }
    }
    let SessionEvent::DeveloperMessage { data, .. } = event else {
        return Ok(());
    };
    if additions != data.header_seq.is_some() {
        return Err("requires headerSeq exactly when tool additions are present".into());
    }
    Ok(())
}

/// Each tool a developer message adds is defined, once and whole, in the
/// earlier `request/header` its `headerSeq` names. (dsh v4:
/// `assertDeveloperHeader`.)
fn check_developer_header(
    log: &[Entry],
    seq: Seq,
    data: &crate::data::DeveloperMessageData,
) -> Result<(), String> {
    let Some(header_seq) = data.header_seq else {
        return Ok(());
    };
    let header = usize::try_from(header_seq.0)
        .ok()
        .filter(|_| header_seq < seq)
        .and_then(|index| log.get(index))
        .and_then(|entry| match &entry.event {
            SessionEvent::RequestHeader(header) => Some(header),
            _ => None,
        })
        .ok_or("developer/message headerSeq must reference an earlier request/header")?;
    let tools = header.header.tools.as_deref().unwrap_or_default();
    let content = serde_json::to_value(&data.message.content).unwrap_or(Value::Null);
    for block in content.as_array().into_iter().flatten() {
        if block.get("type").and_then(Value::as_str) != Some("tool-addition") {
            continue;
        }
        let name = block["toolName"].as_str().unwrap_or_default();
        let definitions: Vec<&Value> = tools
            .iter()
            .filter(|tool| tool.get("name").and_then(Value::as_str) == Some(name))
            .collect();
        let [definition] = definitions.as_slice() else {
            return Err(format!(
                "developer/message tool-addition \"{name}\" must name exactly one tool in headerSeq \
                 {header_seq}"
            ));
        };
        if !definition.get("description").is_some_and(Value::is_string)
            || !definition.get("parameters").is_some_and(Value::is_object)
        {
            return Err(format!(
                "developer/message tool-addition \"{name}\" requires a complete tool definition in \
                 headerSeq {header_seq}"
            ));
        }
        if definition
            .get("deferLoading")
            .is_some_and(|defer| defer != &Value::Bool(true))
        {
            return Err(
                "developer/message referenced tool deferLoading must be true when present".into(),
            );
        }
    }
    Ok(())
}
