//! The session-log record of which preset a session runs. (dsh:
//! `agent-preset-registry/src/session.ts`.)
//!
//! The header names the preset a session started with. A session may change
//! preset while it is blank, and that change outlives the blank window, so
//! it is logged as an `agent-preset/selected` event. The `agentPreset`
//! projection folds both, and is what reconstruction reads, never the
//! header alone.

use karaka_session::{Entry, Passthrough, SessionEvent, SessionHeader};
use karaka_session_projection::{ClientView, Projection};
use serde_json::json;

/// The selection event's type. (dsh: `agent-preset/selected`, `{
/// agentPreset }`.)
pub const SELECTED_EVENT: &str = "agent-preset/selected";

/// The event recording that the session now runs `preset`.
pub fn selected(preset: &str) -> SessionEvent {
    SessionEvent::Passthrough(Passthrough {
        event_type: SELECTED_EVENT.into(),
        data: json!({ "agentPreset": preset }),
        surface_op: None,
        source_event_seqs: None,
    })
}

/// The `agentPreset` projection: the header's preset, then the latest
/// selection, or `None` when neither names one. Clients see the state
/// itself. (dsh: `agentPresetProjectionDefinition`.)
pub struct AgentPresetProjection;

impl Projection for AgentPresetProjection {
    const KEY: &'static str = "agentPreset";
    type State = Option<String>;

    fn state_version(&self) -> u64 {
        1
    }

    fn init(&self, header: &SessionHeader, _: u64) -> Option<String> {
        header.agent_preset.clone()
    }

    fn apply(&self, _: &Option<String>, entry: &Entry) -> Option<Option<String>> {
        match &entry.event {
            SessionEvent::Passthrough(event) if event.event_type == SELECTED_EVENT => event
                .data
                .get("agentPreset")
                .and_then(|preset| preset.as_str())
                .map(|preset| Some(preset.to_owned())),
            _ => None,
        }
    }
}

impl ClientView for AgentPresetProjection {
    type View = Option<String>;

    fn view(&self, state: &Option<String>) -> Option<String> {
        state.clone()
    }
}
