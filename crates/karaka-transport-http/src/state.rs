//! A chat's application state: the request ids already admitted, and the
//! model it last used or was told to use. (Karaka:
//! `transport-http/src/application-state.ts`.)
//!
//! Karaka keeps it as a session projection, `karakaApplication`. Here it
//! is folded from the events when it is read, with the same rules: no
//! checkpoint is kept for it.

use karaka_agent::ModelSelection;
use karaka_session::Entry;
use serde_json::Value;

/// (Karaka: `ApplicationState`.)
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ApplicationState {
    /// Every admitted request id, messages still queued included.
    pub request_ids: Vec<String>,
    /// The route the last request went to, without an effort the adapter
    /// chose.
    pub last_used: Option<ModelSelection>,
    /// A selection no request has used yet.
    pub pending: Option<ModelSelection>,
}

fn selection(value: &Value) -> Option<ModelSelection> {
    Some(ModelSelection {
        provider: value["provider"].as_str()?.to_owned(),
        model: value["model"].as_str()?.to_owned(),
        reasoning_effort: value["reasoningEffort"].as_str().map(str::to_owned),
    })
}

/// One event's change. (Karaka: `foldApplicationState`.)
pub fn fold(mut state: ApplicationState, entry: &Entry) -> ApplicationState {
    let event = serde_json::to_value(entry).unwrap_or(Value::Null);
    let data = &event["data"];
    match event["type"].as_str() {
        Some("model/selection") => {
            if let Some(selected) = selection(data) {
                state.pending = Some(selected);
            }
            state
        }
        Some("request/header") => {
            let config = &data["header"]["config"];
            let Some(mut used) = selection(config) else {
                return state;
            };
            if state.pending.as_ref() == Some(&used) {
                state.pending = None;
            }
            if data["header"]["adapterDefaults"]["reasoningEffort"] == true {
                used.reasoning_effort = None;
            }
            state.last_used = Some(used);
            state
        }
        Some(kind @ ("agent/inbox/spliced" | "user/message")) => {
            let messages: Vec<&Value> = if kind == "user/message" {
                vec![data]
            } else {
                data["inserted"]
                    .as_array()
                    .map(|inserted| inserted.iter().collect())
                    .unwrap_or_default()
            };
            for message in messages {
                let source = &message["source"];
                if source["kind"] != "user" {
                    continue;
                }
                if let Some(id) = source["rpcId"].as_str()
                    && !state.request_ids.iter().any(|known| known == id)
                {
                    state.request_ids.push(id.to_owned());
                }
            }
            state
        }
        _ => state,
    }
}

/// The state of a log. (Karaka: `applicationState`.)
pub fn application_state(events: &[Entry]) -> ApplicationState {
    events
        .iter()
        .fold(ApplicationState::default(), |state, entry| {
            fold(state, entry)
        })
}
