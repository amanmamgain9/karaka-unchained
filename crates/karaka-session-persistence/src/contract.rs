//! Checks every backend makes the same way: the stored id, the event
//! vocabulary, a create's header, and a batch's contiguity.
//! (dsh: `session-persistence/src/storage-contract.ts`.)

use karaka_session::{Entry, SessionHeader, SessionId, SessionLog, is_known_event_type};
use serde_json::Value;

use crate::{PersistenceError, SessionLocation};

/// The largest integer JSON readers in JavaScript keep exactly, the limit
/// dsh puts on every stored count. (JavaScript: `Number.MAX_SAFE_INTEGER`.)
pub const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;

/// Refuses a stored header that isn't the session asked for. (dsh:
/// `assertStoredId`.)
pub fn assert_stored_id(id: &SessionId, header: &SessionHeader) -> Result<(), PersistenceError> {
    if &header.id == id {
        return Ok(());
    }
    Err(PersistenceError::Other(format!(
        "stored session identity mismatch: requested \"{id}\", header contains \"{}\"",
        header.id
    )))
}

/// Refuses a header `create` can't store. (dsh: `materializeCreateHeader`.)
///
/// dsh also refuses metadata that isn't plain JSON; a Rust header always
/// is.
pub fn assert_create_header(header: &SessionHeader) -> Result<(), PersistenceError> {
    if header.created_at > MAX_SAFE_INTEGER {
        return Err(PersistenceError::Other(
            "session metadata createdAt must be a non-negative safe integer".into(),
        ));
    }
    Ok(())
}

/// Refuses a batch that doesn't continue the stored log at `cursor`, its
/// next seq. (dsh: `assertContiguous`.)
pub fn assert_contiguous(
    id: &SessionId,
    events: &[Entry],
    cursor: u64,
) -> Result<(), PersistenceError> {
    for (index, entry) in events.iter().enumerate() {
        let expected = cursor + index as u64;
        if entry.seq.0 != expected {
            return Err(PersistenceError::Other(format!(
                "append seq mismatch for \"{id}\": expected {expected} at index {index}, got {}",
                entry.seq
            )));
        }
    }
    Ok(())
}

/// Turns the stored rows of `header`'s session into events, refusing what
/// this build mustn't interpret. (dsh: `validateStoredEvents`.)
///
/// First, on the raw rows, as dsh does: an event type this build doesn't
/// know, unless marked `ignorable`, and the retired `fallback` reason of a
/// `request/header`, are format refusals naming the raw log. Then each row
/// is read as an [`Entry`], and the whole log is restored into a
/// [`SessionLog`], which checks surface operations; a failure there is
/// corruption. (In dsh that last check is the format catalog restoring the
/// artifact through `Session.fromRestore`.)
pub fn validate_stored_events(
    header: &SessionHeader,
    rows: Vec<Value>,
    location: Option<&SessionLocation>,
) -> Result<Vec<Entry>, PersistenceError> {
    let id = &header.id;
    for row in &rows {
        let event_type = row.get("type").and_then(Value::as_str).unwrap_or_default();
        let seq = row.get("seq").cloned().unwrap_or(Value::Null);
        let ignorable = row.get("ignorable") == Some(&Value::Bool(true));
        if !is_known_event_type(event_type) && !ignorable {
            return Err(PersistenceError::unsupported(
                format!(
                    "session \"{id}\" contains event type \"{event_type}\" (seq {seq}) unknown to \
                     this harness and not marked ignorable; refusing to interpret the log — it \
                     was likely written by a newer harness"
                ),
                location.cloned(),
            ));
        }
        if event_type == "request/header"
            && row.pointer("/data/reason") == Some(&Value::from("fallback"))
        {
            return Err(PersistenceError::unsupported(
                format!(
                    "session \"{id}\" contains a request/header event (seq {seq}) with the \
                     unsupported legacy reason \"fallback\"; refusing to interpret the log — it \
                     was written by a retired pre-release harness"
                ),
                location.cloned(),
            ));
        }
    }
    let failed = |error: String| {
        PersistenceError::Corruption(format!(
            "stored session \"{id}\" failed validation: {error}"
        ))
    };
    let entries = rows
        .into_iter()
        .map(serde_json::from_value::<Entry>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| failed(error.to_string()))?;
    SessionLog::restore(entries.iter().cloned()).map_err(|error| failed(error.to_string()))?;
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use karaka_session::{Seq, SessionEvent, Time};
    use serde_json::json;

    use super::*;

    fn header() -> SessionHeader {
        SessionHeader::new(SessionId::new("s"), 1)
    }

    #[test]
    fn an_unknown_type_is_refused_unless_ignorable() {
        let unknown = json!({ "type": "mystery/event", "seq": 0, "time": 1, "data": {} });
        let refusal = validate_stored_events(&header(), vec![unknown], None).unwrap_err();
        assert!(matches!(
            refusal,
            PersistenceError::FormatUnsupported { .. }
        ));
        assert!(
            refusal
                .to_string()
                .contains("\"mystery/event\" (seq 0) unknown")
        );

        let ignorable = json!({
            "type": "mystery/event", "seq": 0, "time": 1, "data": {}, "ignorable": true
        });
        assert_eq!(
            validate_stored_events(&header(), vec![ignorable], None)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn the_legacy_fallback_reason_is_refused() {
        let row = json!({
            "type": "request/header", "seq": 0, "time": 1,
            "data": { "header": { "config": { "provider": "mock", "model": "m" } }, "reason": "fallback" }
        });
        let refusal = validate_stored_events(&header(), vec![row], None).unwrap_err();
        assert!(
            refusal
                .to_string()
                .contains("unsupported legacy reason \"fallback\"")
        );
    }

    #[test]
    fn a_row_that_does_not_read_or_fit_is_corruption() {
        let bad = json!({ "type": "turn/start", "seq": 0, "time": 1, "data": { "turn": "one" } });
        let error = validate_stored_events(&header(), vec![bad], None).unwrap_err();
        assert!(matches!(error, PersistenceError::Corruption(_)));
        assert!(
            error
                .to_string()
                .starts_with("stored session \"s\" failed validation")
        );
    }

    #[test]
    fn a_batch_must_continue_the_log() {
        let entry = |seq| Entry {
            seq: Seq(seq),
            time: Time(1),
            ignorable: false,
            event: SessionEvent::turn_start(1),
        };
        let id = SessionId::new("s");
        assert!(assert_contiguous(&id, &[entry(6), entry(7)], 6).is_ok());
        let error = assert_contiguous(&id, &[entry(6), entry(8)], 6).unwrap_err();
        assert_eq!(
            error.to_string(),
            "append seq mismatch for \"s\": expected 7 at index 1, got 8"
        );
    }
}
