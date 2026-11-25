//! What the registry hands out: snapshots, checkpoint rows, and the change
//! feed. (dsh: `ProjectionSnapshot`, `ProjectionCheckpointRow`,
//! `ProjectionChangeListener` in `session-projection/src/index.ts`.)

use std::collections::BTreeMap;
use std::fmt;

use karaka_session::{Seq, SessionId};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// One consistent cut over every client-visible unit of one session: each
/// value reflects exactly the events up to `as_of_seq`. (dsh:
/// `ProjectionSnapshot`.)
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectionSnapshot {
    /// The seq of the last event the values reflect; `None` for an empty
    /// log, which dsh writes as -1.
    #[serde(with = "cursor")]
    pub as_of_seq: Option<Seq>,
    /// Each unit's whole current view, by key, in registration order.
    pub values: Map<String, Value>,
}

/// One unit's state at a seq: a persisted projection-cache row, without its
/// session and key. Never authoritative, only a shortcut for the fold:
/// [`restore`](crate::SessionProjections::restore) drops a row at another
/// version, or one past the log it is given. (dsh:
/// `ProjectionCheckpointRow`.)
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CheckpointRow {
    /// The unit's state version when it was folded.
    pub ver: u64,
    /// The seq of the last event folded into `val`; `None` for an empty
    /// log, written -1.
    #[serde(with = "cursor")]
    pub seq: Option<Seq>,
    /// The unit's state, as its serde form writes it.
    pub val: Value,
}

/// One session's rows, by key. (dsh: `ProjectionCheckpoint`.)
pub type ProjectionCheckpoint = BTreeMap<String, CheckpointRow>;

/// What [`restore`](crate::SessionProjections::restore) gives: the cut at
/// the end of the events it was given, and every unit's row at that cut,
/// ready to be written back.
#[derive(Clone, Debug, PartialEq)]
pub struct Restored {
    pub snapshot: ProjectionSnapshot,
    pub checkpoint: ProjectionCheckpoint,
}

/// One client-visible unit's view changed for one session. (dsh: the
/// arguments of a `ProjectionChangeListener`.)
#[derive(Clone, Copy, Debug)]
pub struct ProjectionChange<'a> {
    pub session: &'a SessionId,
    pub key: &'a str,
    /// The new whole view.
    pub value: &'a Value,
    /// The seq of the event that changed it.
    pub seq: Seq,
}

/// Why the registry refused a registration or a restore.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectionError(String);

impl ProjectionError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for ProjectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ProjectionError {}

/// A seq that may be before the first event, written as dsh's
/// `SessionSeqCursor`: the seq, or -1.
mod cursor {
    use karaka_session::Seq;
    use serde::de::Error as _;
    use serde::{Deserialize, Deserializer, Serializer};

    pub(super) fn serialize<S: Serializer>(
        seq: &Option<Seq>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match seq {
            Some(seq) => serializer.serialize_u64(seq.0),
            None => serializer.serialize_i64(-1),
        }
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Seq>, D::Error> {
        match i64::deserialize(deserializer)? {
            -1 => Ok(None),
            seq if seq >= 0 => Ok(Some(Seq(seq as u64))),
            seq => Err(D::Error::custom(format!(
                "a seq cursor is -1 or a seq, not {seq}"
            ))),
        }
    }
}
