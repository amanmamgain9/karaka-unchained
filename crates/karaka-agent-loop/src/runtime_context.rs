//! The runtime-context snapshot: the user-role message that shows the model
//! the registry's runtime context, sent only when it changed. (dsh:
//! `RuntimeContextProjection` in `agent-loop/src/runtime-context.ts`.)
//!
//! The projection keeps the last snapshot in effect: the newest
//! `runtime-context` user message still on the surface. It is worked out
//! from the log when the agent is made, so a resumed session doesn't send
//! its snapshot again, then follows the log: a new snapshot takes its
//! place, and a replacement that cites it, such as compaction, drops it.
//!
//! dsh follows the log with a `session/event` listener; here the
//! projection reads the events it hasn't seen each time it is asked, which
//! gives the same state, since only the log changes it.

use karaka_session::{
    ContentBlock, Entry, Extra, RuntimeContextSource, Seq, SessionEvent, SurfaceOp, UserMessage,
    UserRole, UserSource,
};
use karaka_system_prompt::ContextSection;

/// What a snapshot says once the context has gone. (dsh: `CLEARED`.)
pub(crate) const CLEARED: &str =
    "Current runtime context: none. Earlier runtime-context snapshots no longer apply.";

/// The last snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Retained {
    /// No snapshot was ever logged. (dsh: `undefined`.)
    Never,
    /// One was, but none is in effect. (dsh: `null`.)
    Gone,
    /// The one in effect: its seq, and its text if it is one text block.
    Kept { seq: Seq, text: Option<String> },
}

/// The runtime-context message of an event, if it is one.
fn owned(entry: &Entry) -> Option<&UserMessage> {
    match &entry.event {
        SessionEvent::UserMessage { message, .. }
            if matches!(message.source, UserSource::RuntimeContext(_)) =>
        {
            Some(message)
        }
        _ => None,
    }
}

/// The message's text when it is exactly one text block. (dsh: `textOf`.)
fn text_of(message: &UserMessage) -> Option<String> {
    match message.content.as_slice() {
        [ContentBlock::Text(text)] => Some(text.text.clone()),
        _ => None,
    }
}

/// The seqs a surface replacement cites. (dsh: `isReplacementSurfaceEvent`
/// with its `sourceEventSeqs`.)
fn replaced(event: &SessionEvent) -> Option<&[Seq]> {
    let surface = match event {
        SessionEvent::SystemMessage { surface, .. }
        | SessionEvent::DeveloperMessage { surface, .. }
        | SessionEvent::UserMessage { surface, .. }
        | SessionEvent::ToolResult { surface, .. } => surface,
        _ => return None,
    };
    matches!(surface.op, SurfaceOp::Replace { .. }).then_some(surface.sources.as_slice())
}

/// Follows a session's runtime-context snapshot. (dsh:
/// `RuntimeContextProjection`.)
#[derive(Debug)]
pub(crate) struct RuntimeContextProjection {
    retained: Retained,
    /// How many of the log's events it has read.
    seen: usize,
}

impl RuntimeContextProjection {
    /// The snapshot in effect in `events`: the newest one still on the
    /// `surface`. (dsh: the constructor's restore.)
    pub(crate) fn restore(events: &[Entry], surface: &[Seq]) -> Self {
        let mut retained = Retained::Never;
        for entry in events.iter().rev() {
            let Some(message) = owned(entry) else {
                continue;
            };
            if retained == Retained::Never {
                retained = Retained::Gone;
            }
            if surface.contains(&entry.seq) {
                retained = Retained::Kept {
                    seq: entry.seq,
                    text: text_of(message),
                };
                break;
            }
        }
        Self {
            retained,
            seen: events.len(),
        }
    }

    /// Reads the events logged since it last looked. (dsh: its
    /// `session/event` listener.)
    fn follow(&mut self, events: &[Entry]) {
        for entry in events.iter().skip(self.seen) {
            if let Some(message) = owned(entry) {
                self.retained = Retained::Kept {
                    seq: entry.seq,
                    text: text_of(message),
                };
            } else if let Retained::Kept { seq, .. } = &self.retained
                && replaced(&entry.event).is_some_and(|sources| sources.contains(seq))
            {
                self.retained = Retained::Gone;
            }
        }
        self.seen = events.len();
    }

    /// The snapshot to send now, for `current`, the rendered context of
    /// `sections`, or `None` when the one in effect says the same. Context
    /// that has gone sends [`CLEARED`], unless no snapshot was ever sent.
    /// The message gets `id`. (dsh: `project`.)
    pub(crate) fn project(
        &mut self,
        events: &[Entry],
        current: &str,
        sections: &[ContextSection],
        id: String,
    ) -> Option<UserMessage> {
        self.follow(events);
        if self.retained == Retained::Never && current.is_empty() {
            return None;
        }
        let snapshot = if current.is_empty() { CLEARED } else { current };
        if let Retained::Kept {
            text: Some(text), ..
        } = &self.retained
            && text == snapshot
        {
            return None;
        }
        // The cleared marker has no contributions left to name.
        let source = if sections.is_empty() {
            RuntimeContextSource::default()
        } else {
            RuntimeContextSource {
                form: Some("snapshot".into()),
                sections: Some(
                    sections
                        .iter()
                        .map(|section| karaka_session::ContextSection {
                            name: section.name.clone(),
                            text: section.text.clone(),
                            extra: Extra::new(),
                        })
                        .collect(),
                ),
                ..RuntimeContextSource::default()
            }
        };
        Some(UserMessage {
            id,
            role: UserRole::User,
            content: vec![ContentBlock::text(snapshot)],
            source: UserSource::RuntimeContext(source),
            extra: Extra::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use karaka_session::{SessionLog, SurfaceIntent, Time};
    use serde_json::json;

    use super::*;

    fn snapshot(text: &str) -> SessionEvent {
        SessionEvent::user(UserMessage {
            id: format!("context-{text}"),
            role: UserRole::User,
            content: vec![ContentBlock::text(text)],
            source: UserSource::RuntimeContext(RuntimeContextSource::default()),
            extra: Extra::new(),
        })
    }

    fn section(name: &str, text: &str) -> ContextSection {
        ContextSection {
            name: name.into(),
            text: text.into(),
        }
    }

    // runtime-context.spec.ts: "restores the latest visible owned snapshot
    // and ignores other sessions"
    #[test]
    fn the_newest_snapshot_on_the_surface_is_restored() {
        let mut log = SessionLog::new();
        let retained = log.append(snapshot("retained")).unwrap();
        let shadowed = log.append(snapshot("shadowed")).unwrap();
        log.append(SessionEvent::UserMessage {
            message: UserMessage::text(
                "summary",
                UserSource::Other(json!({ "kind": "test-compaction" })),
                "summary",
            ),
            surface: SurfaceIntent {
                op: SurfaceOp::Replace {
                    start: shadowed,
                    end: shadowed,
                },
                sources: vec![shadowed],
            },
        })
        .unwrap();
        assert!(log.surface_nodes().contains(&retained));

        let events = log.events().to_vec();
        let mut projection = RuntimeContextProjection::restore(&events, log.surface_nodes());
        assert_eq!(
            projection.project(&events, "retained", &[], "a".into()),
            None
        );
        let next = projection
            .project(
                &events,
                "next",
                &[section("sandbox:policy", "policy")],
                "b".into(),
            )
            .expect("a changed snapshot is sent");
        assert_eq!(
            serde_json::to_value(&next.source).unwrap(),
            json!({ "kind": "runtime-context", "form": "snapshot",
                    "sections": [{ "name": "sandbox:policy", "text": "policy" }] })
        );
    }

    #[test]
    fn nothing_is_sent_until_there_is_context() {
        let mut projection = RuntimeContextProjection::restore(&[], &[]);
        assert_eq!(projection.project(&[], "", &[], "a".into()), None);
        let first = projection.project(&[], "now", &[], "b".into()).unwrap();
        assert_eq!(first.content, [ContentBlock::text("now")]);
        // Once one was sent, losing the context clears it.
        let entry = Entry {
            seq: Seq(0),
            time: Time(1),
            ignorable: false,
            event: SessionEvent::user(first),
        };
        let cleared = projection.project(&[entry], "", &[], "c".into()).unwrap();
        assert_eq!(cleared.content, [ContentBlock::text(CLEARED)]);
        assert_eq!(
            cleared.source,
            UserSource::RuntimeContext(RuntimeContextSource::default())
        );
    }
}
