//! What a session is, apart from its events: the header storage keeps
//! beside the log. (dsh v4: `SessionHeader` in `core/session/src/types.ts`.)
//!
//! The in-memory store doesn't need a header, so [`SessionStore`] keeps
//! none. A durable backend writes it as the first line of the session's
//! file, and lists sessions by reading only that line.
//!
//! [`SessionStore`]: crate::SessionStore

use crate::SessionId;

/// The session format this build reads and writes. (dsh:
/// `SESSION_FORMAT_VERSION`.)
///
/// Older formats need a migration, which isn't ported yet, and newer ones
/// were written by a newer harness. Storage refuses both.
pub const SESSION_FORMAT_VERSION: u64 = 4;

/// Who made a session, when it isn't a person. (dsh v4: `origin`.)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// Delegated from another session. (dsh v4: `'subagent'`.)
    Subagent,
}

/// The session's metadata, fixed when it is created. (dsh v4:
/// `SessionHeader`.)
///
/// dsh's header also has `version`, always the current format; here that is
/// [`SESSION_FORMAT_VERSION`], and the storage codec writes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionHeader {
    pub id: SessionId,
    /// When the session was created, in milliseconds since the Unix epoch.
    pub created_at: u64,
    /// The absolute project directory the session works in.
    pub cwd: Option<String>,
    /// The session this one was forked or delegated from.
    pub parent_session: Option<SessionId>,
    /// Whether the log starts with events inherited from another session.
    /// The last `session/end-seed` marker marks where they end.
    pub is_seeded: bool,
    pub origin: Option<Origin>,
    /// How many delegations deep this session is; 0 for a session a person
    /// started. (dsh v4 writes 0 when it isn't set.)
    pub delegation_depth: u64,
    /// The agent preset the session was composed from.
    pub agent_preset: Option<String>,
}

impl SessionHeader {
    /// A header for a fresh, unseeded session, made by a person, at
    /// `created_at`.
    pub fn new(id: SessionId, created_at: u64) -> Self {
        Self {
            id,
            created_at,
            cwd: None,
            parent_session: None,
            is_seeded: false,
            origin: None,
            delegation_depth: 0,
            agent_preset: None,
        }
    }

    /// The same header, working in `cwd`.
    pub fn with_cwd(mut self, cwd: impl Into<String>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }
}

/// Every event type dsh v4 defines, which is the vocabulary this build
/// understands. (dsh: `KNOWN_SESSION_EVENT_TYPES`,
/// `core/session/src/known-event-types.ts`.)
///
/// Karaka models only some of these and keeps the rest as passthrough, but
/// it knows what each one is. A stored log with a type outside this list,
/// not marked `ignorable`, was likely written by a newer harness, and
/// storage refuses to interpret it: skipping a required event could rebuild
/// a wrong session.
pub const KNOWN_EVENT_TYPES: &[&str] = &[
    "agent-preset/selected",
    "agent/inbox/spliced",
    "approval/asked",
    "approval/decided",
    "approval/policy",
    "assistant/attempt",
    "assistant/message",
    "command/done",
    "command/run",
    "compaction/end",
    "compaction/prune",
    "compaction/start",
    "compaction/summary",
    "deliverables/presented",
    "developer/message",
    "feedback/message-delete",
    "feedback/message-put",
    "feedback/record",
    "goal/change",
    "hook/invoked",
    "hook/result",
    "image/offload",
    "job/registered",
    "job/settled",
    "llm/retry",
    "llm/retry-started",
    "model/selection",
    "permission/preset",
    "plan/mode",
    "request/context",
    "request/header",
    "sandbox/mode",
    "schedule/change",
    "session-log-deepseek/delivery-accepted",
    "session/end-seed",
    "session/title",
    "session/title-llm-request",
    "step/end",
    "step/start",
    "subagent/catalog",
    "subagent/descriptor",
    "subagent/model-selection-policy",
    "system/message",
    "team/member",
    "team/message/delivered",
    "team/message/queued",
    "team/task",
    "todo/write",
    "tool-workflow/agent-end",
    "tool-workflow/agent-start",
    "tool-workflow/run-end",
    "tool-workflow/run-start",
    "tool/call",
    "tool/ptc-dispatch",
    "tool/ptc-dispatch-start",
    "tool/result",
    "turn/end",
    "turn/start",
    "user/message",
    "web/deepseek-search-llm-request",
    "working-directory/change",
    "workspace/changes",
];

/// Whether this build knows the event type `event_type`.
pub fn is_known_event_type(event_type: &str) -> bool {
    KNOWN_EVENT_TYPES.binary_search(&event_type).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_vocabulary_is_sorted_so_it_can_be_searched() {
        assert!(KNOWN_EVENT_TYPES.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(is_known_event_type("llm/retry"));
        assert!(!is_known_event_type("mystery/event"));
    }
}
