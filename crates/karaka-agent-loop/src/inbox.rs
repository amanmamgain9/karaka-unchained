//! The agent's durable inbox: the `inbox` projection, folded from the
//! session's `agent/inbox/spliced` events, and the splices that change it.
//! (dsh: `inboxProjectionDefinition` and `ReactLoopInbox` in
//! `agent-loop/src/inbox.ts`.)
//!
//! Every change is a splice of one list, logged before it takes effect,
//! so what is pending is a fold of the log and survives a restore. The
//! fold refuses a splice that doesn't fit what came before it, as dsh's
//! does, and the agent won't open on such a history.
//!
//! dsh's projection throws from its fold. A unit here can't fail, so the
//! state records the first bad splice and folds nothing after it; reading
//! the inbox then gives dsh's error.

use std::collections::HashSet;
use std::sync::Arc;

use karaka_agent::{InboxError, SpliceCause};
use karaka_session::{
    Entry, Extra, InboxSplice, InboxTarget, Seq, Session, SessionEvent, SessionHeader,
    SpliceOutcome, UserMessage,
};
use karaka_session_projection::{ClientView, Projection, SessionProjections};
use serde::{Deserialize, Serialize};

/// The pending input. (dsh: `InboxState`.)
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct InboxState {
    /// Prompts waiting for a turn each, oldest first.
    #[serde(rename = "next-turn")]
    pub next_turn: Vec<UserMessage>,
    /// Input waiting for the next step boundary.
    #[serde(rename = "next-step")]
    pub next_step: Vec<UserMessage>,
    /// The first splice that didn't fold; nothing after it is folded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invalid: Option<InvalidSplice>,
}

/// A splice the fold refused. (dsh: the error its `apply` throws.)
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvalidSplice {
    pub seq: Seq,
    pub cause: String,
}

impl InboxState {
    fn list(&self, target: InboxTarget) -> &[UserMessage] {
        match target {
            InboxTarget::NextTurn => &self.next_turn,
            InboxTarget::NextStep => &self.next_step,
        }
    }

    /// The state with `target` replaced by `list`, or the first id pending
    /// twice across both lists, next-turn first.
    fn with(&self, target: InboxTarget, list: Vec<UserMessage>) -> Result<Self, String> {
        let (next_turn, next_step) = match target {
            InboxTarget::NextTurn => (list, self.next_step.clone()),
            InboxTarget::NextStep => (self.next_turn.clone(), list),
        };
        let mut ids = HashSet::new();
        for message in next_turn.iter().chain(&next_step) {
            if !ids.insert(message.id.as_str()) {
                return Err(message.id.clone());
            }
        }
        Ok(Self {
            next_turn,
            next_step,
            invalid: None,
        })
    }

    fn pending(&self) -> bool {
        !self.next_turn.is_empty() || !self.next_step.is_empty()
    }
}

/// `list` with `removed` messages from `start` replaced by `inserted`.
fn spliced(
    list: &[UserMessage],
    start: usize,
    removed: usize,
    inserted: &[UserMessage],
) -> Vec<UserMessage> {
    let mut next = list[..start].to_vec();
    next.extend_from_slice(inserted);
    next.extend_from_slice(&list[start + removed..]);
    next
}

/// The fold of one stored splice. (dsh: `apply`.)
fn fold(state: &InboxState, splice: &InboxSplice) -> Result<InboxState, String> {
    let list = state.list(splice.target);
    let start = usize::try_from(splice.start).unwrap_or(usize::MAX);
    let removed = usize::try_from(splice.removed_count.unwrap_or(0)).unwrap_or(usize::MAX);
    if start > list.len() || start.saturating_add(removed) > list.len() {
        return Err("invalid inbox splice".into());
    }
    state
        .with(
            splice.target,
            spliced(list, start, removed, &splice.inserted),
        )
        .map_err(|id| format!("message \"{id}\" is already pending"))
}

/// The `inbox` projection: what the agent has pending. Clients see the two
/// lists. (dsh: `inboxProjectionDefinition`.)
pub struct InboxProjection;

impl Projection for InboxProjection {
    const KEY: &'static str = "inbox";
    type State = InboxState;

    fn state_version(&self) -> u64 {
        1
    }

    fn init(&self, _: &SessionHeader, _: u64) -> InboxState {
        InboxState::default()
    }

    fn apply(&self, state: &InboxState, entry: &Entry) -> Option<InboxState> {
        let SessionEvent::InboxSpliced(splice) = &entry.event else {
            return None;
        };
        if state.invalid.is_some() {
            return None;
        }
        Some(fold(state, splice).unwrap_or_else(|cause| InboxState {
            invalid: Some(InvalidSplice {
                seq: entry.seq,
                cause,
            }),
            ..state.clone()
        }))
    }
}

/// The lists, as a client sees them. (dsh: `InboxWireState`.)
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct InboxView {
    #[serde(rename = "next-turn")]
    next_turn: Vec<UserMessage>,
    #[serde(rename = "next-step")]
    next_step: Vec<UserMessage>,
}

impl ClientView for InboxProjection {
    type View = InboxView;

    fn view(&self, state: &InboxState) -> InboxView {
        InboxView {
            next_turn: state.next_turn.clone(),
            next_step: state.next_step.clone(),
        }
    }
}

/// One session's inbox, read from the projection and changed by logging
/// splices. (dsh: `ReactLoopInbox`.)
///
/// The agent holds its lock around every change, so a splice is worked out
/// from the state it lands on.
pub(crate) struct Inbox<'a> {
    pub(crate) projections: &'a SessionProjections,
    pub(crate) session: &'a Session,
}

impl Inbox<'_> {
    /// The state as the log has it. (dsh: `current`.)
    pub(crate) fn current(&self) -> Result<Arc<InboxState>, InboxError> {
        let state = self
            .projections
            .state_of::<InboxProjection>(self.session)
            .ok_or_else(|| {
                InboxError::Unavailable(format!(
                    "agent \"{}\" cannot read inbox state: its projection registration is not \
                     active",
                    self.session.id()
                ))
            })?;
        match &state.invalid {
            Some(invalid) => Err(InboxError::Invalid {
                seq: invalid.seq,
                cause: SpliceCause(invalid.cause.clone()),
            }),
            None => Ok(state),
        }
    }

    /// Whether either list holds anything. (dsh: `hasPending`.)
    pub(crate) fn has_pending(&self) -> Result<bool, InboxError> {
        Ok(self.current()?.pending())
    }

    /// Whether next-step holds anything.
    pub(crate) fn has_next_step(&self) -> Result<bool, InboxError> {
        Ok(!self.current()?.next_step.is_empty())
    }

    /// Adds `message` at the end of `target`. (dsh: `append`, and `send`'s
    /// splice at `Infinity`.)
    pub(crate) fn append(
        &self,
        target: InboxTarget,
        message: UserMessage,
    ) -> Result<(), InboxError> {
        self.mutate(target, usize::MAX, 0, vec![message], false)
            .map(drop)
    }

    /// Adds `message` at the start of `target`. (dsh: `prepend`.)
    #[allow(dead_code)]
    pub(crate) fn prepend(
        &self,
        target: InboxTarget,
        message: UserMessage,
    ) -> Result<(), InboxError> {
        self.mutate(target, 0, 0, vec![message], false).map(drop)
    }

    /// Takes one step's input: all of next-step, then, when the step opens
    /// a turn, the oldest of next-turn. (dsh: `claim`.)
    pub(crate) fn claim(&self, target: InboxTarget) -> Result<Vec<UserMessage>, InboxError> {
        let mut claimed = self.mutate(InboxTarget::NextStep, 0, usize::MAX, Vec::new(), false)?;
        if target == InboxTarget::NextTurn {
            claimed.extend(self.mutate(InboxTarget::NextTurn, 0, 1, Vec::new(), false)?);
        }
        Ok(claimed)
    }

    /// Drops everything pending, next-step first, as cancellations, and
    /// gives what it dropped, in that order. (dsh: `clear`.)
    pub(crate) fn clear(&self) -> Result<Vec<UserMessage>, InboxError> {
        let mut dropped = self.splice(InboxTarget::NextStep, 0, usize::MAX, Vec::new())?;
        dropped.extend(self.splice(InboxTarget::NextTurn, 0, usize::MAX, Vec::new())?);
        Ok(dropped)
    }

    /// A splice whose removed messages are dropped, not taken. (dsh:
    /// `splice`.)
    pub(crate) fn splice(
        &self,
        target: InboxTarget,
        start: usize,
        delete: usize,
        inserted: Vec<UserMessage>,
    ) -> Result<Vec<UserMessage>, InboxError> {
        self.mutate(target, start, delete, inserted, true)
    }

    /// Logs one splice, its coordinates clamped to the list, as a
    /// JavaScript splice clamps them, and gives what it removed. Nothing is
    /// logged when it changes nothing. (dsh: `mutate`.)
    fn mutate(
        &self,
        target: InboxTarget,
        start: usize,
        delete: usize,
        inserted: Vec<UserMessage>,
        discard_removed: bool,
    ) -> Result<Vec<UserMessage>, InboxError> {
        let state = self.current()?;
        let list = state.list(target);
        let start = start.min(list.len());
        let delete = delete.min(list.len() - start);
        if delete == 0 && inserted.is_empty() {
            return Ok(Vec::new());
        }
        state
            .with(target, spliced(list, start, delete, &inserted))
            .map_err(InboxError::AlreadyPending)?;
        let removed = list[start..start + delete].to_vec();
        self.session
            .append(SessionEvent::InboxSpliced(InboxSplice {
                target,
                start: start as u64,
                removed_count: (delete > 0).then_some(delete as u64),
                inserted,
                outcome: (discard_removed && delete > 0).then_some(SpliceOutcome::Canceled),
                extra: Extra::new(),
            }))?;
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use karaka_session::UserSource;

    use super::*;

    fn message(id: &str) -> UserMessage {
        UserMessage::text(id, UserSource::user(), id)
    }

    fn splice(
        target: InboxTarget,
        start: u64,
        removed: Option<u64>,
        inserted: &[&str],
    ) -> InboxSplice {
        InboxSplice {
            target,
            start,
            removed_count: removed,
            inserted: inserted.iter().map(|id| message(id)).collect(),
            outcome: None,
            extra: Extra::new(),
        }
    }

    fn ids(list: &[UserMessage]) -> Vec<&str> {
        list.iter().map(|message| message.id.as_str()).collect()
    }

    #[test]
    fn a_splice_folds_as_javascripts() {
        let state = InboxState::default();
        let state = fold(&state, &splice(InboxTarget::NextTurn, 0, None, &["a", "b"])).unwrap();
        let state = fold(&state, &splice(InboxTarget::NextTurn, 1, Some(1), &["c"])).unwrap();
        let state = fold(&state, &splice(InboxTarget::NextStep, 0, None, &["d"])).unwrap();
        assert_eq!(ids(&state.next_turn), ["a", "c"]);
        assert_eq!(ids(&state.next_step), ["d"]);
        assert!(state.pending());
    }

    #[test]
    fn a_splice_past_its_list_or_repeating_an_id_doesnt_fold() {
        let state = InboxState::default();
        for bad in [
            splice(InboxTarget::NextTurn, 1, None, &[]),
            splice(InboxTarget::NextTurn, 0, Some(1), &[]),
        ] {
            assert_eq!(fold(&state, &bad).unwrap_err(), "invalid inbox splice");
        }
        let state = fold(&state, &splice(InboxTarget::NextTurn, 0, None, &["a"])).unwrap();
        assert_eq!(
            fold(&state, &splice(InboxTarget::NextStep, 0, None, &["a"])).unwrap_err(),
            "message \"a\" is already pending"
        );
    }

    #[test]
    fn the_fold_stops_at_the_first_bad_splice() {
        let entry = |seq, splice| Entry {
            seq: Seq(seq),
            time: karaka_session::Time(1),
            ignorable: false,
            event: SessionEvent::InboxSpliced(splice),
        };
        let unit = InboxProjection;
        let state = InboxState::default();
        let broken = unit
            .apply(
                &state,
                &entry(4, splice(InboxTarget::NextTurn, 2, None, &["a"])),
            )
            .unwrap();
        assert_eq!(
            broken.invalid,
            Some(InvalidSplice {
                seq: Seq(4),
                cause: "invalid inbox splice".into()
            })
        );
        assert_eq!(
            unit.apply(
                &broken,
                &entry(5, splice(InboxTarget::NextTurn, 0, None, &["b"]))
            ),
            None
        );
    }
}
