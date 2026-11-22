//! The current generation on disk: the header line, one row per event, and
//! the scanner that reads them back, dropping a torn tail. (dsh:
//! `session-persistence-jsonl/src/format.ts`, with the released v2 row codec
//! v4 reuses, `session-format-v1-to-v2/src/codec.ts`.)
//!
//! # Rows
//!
//! A row is the event's v4 envelope, as [`Entry`] writes it, with one
//! change: `sourceEventSeqs` is stored losslessly as ranges. A run of three
//! or more consecutive seqs becomes a `[start, end]` pair; any other list
//! stays as it is. Reading expands it back to the exact list.
//!
//! # Reading
//!
//! The scanner decodes only the envelope of each row: required keys, the
//! seq continuing the log, the ranges. Typed decoding of the events, and
//! the vocabulary check, happen after, on the whole log
//! ([`validate_stored_events`](karaka_session_persistence::validate_stored_events)).
//!
//! A final line without a newline is a torn tail, and is dropped. So is
//! everything from the first damaged row on (bad JSON, a malformed
//! envelope, a gap in the seqs), as long as no later row is a `turn/end`:
//! a later committed turn end means the damage is inside committed data,
//! and the file is refused. Rows that fail the v4 admission checks refuse
//! the file wherever they are.

use std::path::Path;

use karaka_session::{Entry, SESSION_FORMAT_VERSION, SessionHeader, SessionId};
use karaka_session_persistence::{PersistenceError, format_version_refusal};
use serde_json::{Map, Value};

use crate::admission::{self, Refusal, count};

/// Why a file couldn't be scanned.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ScanError {
    /// A format this build won't interpret.
    Unsupported(String),
    /// Bytes it can't interpret.
    Corrupt(String),
}

impl From<Refusal> for ScanError {
    fn from(refusal: Refusal) -> Self {
        match refusal {
            Refusal::Unsupported(message) => Self::Unsupported(message),
            Refusal::Malformed(message) => Self::Corrupt(message),
        }
    }
}

/// The header keys v4 allows, required first.
const HEADER_REQUIRED: [&str; 6] = [
    "type",
    "version",
    "id",
    "createdAt",
    "isSeeded",
    "delegationDepth",
];
const HEADER_OPTIONAL: [&str; 4] = ["cwd", "parentSession", "origin", "agentPreset"];

/// The envelope keys a row may have, required first.
const ROW_REQUIRED: [&str; 4] = ["type", "seq", "time", "data"];
const ROW_OPTIONAL: [&str; 3] = ["ignorable", "sourceEventSeqs", "surfaceOp"];

/// The header line for `header`, with v4's key order. (dsh: `toHeaderLine`,
/// with v2's `encodeHeader`.)
pub(crate) fn header_line(
    header: &SessionHeader,
    inherited_event_count: u64,
) -> Result<String, PersistenceError> {
    if !header.is_seeded && inherited_event_count != 0 {
        return Err(PersistenceError::Other(
            "unseeded session header inherited event count must be 0".into(),
        ));
    }
    if header
        .cwd
        .as_deref()
        .is_some_and(|cwd| !Path::new(cwd).is_absolute())
    {
        return Err(PersistenceError::Other(
            "format v4 header cwd must be absolute".into(),
        ));
    }
    let mut line = Map::new();
    line.insert("type".into(), "session".into());
    line.insert("version".into(), SESSION_FORMAT_VERSION.into());
    line.insert("id".into(), header.id.as_str().into());
    line.insert("createdAt".into(), header.created_at.into());
    if let Some(cwd) = &header.cwd {
        line.insert("cwd".into(), cwd.as_str().into());
    }
    if let Some(parent) = &header.parent_session {
        line.insert("parentSession".into(), parent.as_str().into());
    }
    line.insert("isSeeded".into(), header.is_seeded.into());
    if header.origin.is_some() {
        line.insert("origin".into(), "subagent".into());
    }
    line.insert("delegationDepth".into(), header.delegation_depth.into());
    if let Some(preset) = &header.agent_preset {
        line.insert("agentPreset".into(), preset.as_str().into());
    }
    Ok(Value::Object(line).to_string())
}

/// Refuses the policy fields no released header has. (dsh:
/// `assertNoRetiredHeaderFields`.)
pub(crate) fn assert_no_retired_header_fields(value: &Value) -> Result<(), String> {
    match value.as_object() {
        Some(header)
            if header.contains_key("sandboxMode") || header.contains_key("approvalPolicy") =>
        {
            Err("session header uses retired policy baseline fields".into())
        }
        _ => Ok(()),
    }
}

/// The `version` of a parsed header, when it's a number other than this
/// build's, refused before any other check: a newer format needn't pass
/// this build's checks, and its user should read "upgrade the harness", not
/// "corrupt". (dsh: `refuseForeignFormatVersion`.)
pub(crate) fn refuse_foreign_version(header: &Value) -> Result<(), ScanError> {
    let Some(version) = header.get("version").and_then(Value::as_number) else {
        return Ok(());
    };
    if version.as_u64() == Some(SESSION_FORMAT_VERSION) {
        return Ok(());
    }
    let id = match header.get("id") {
        Some(Value::String(id)) => id.clone(),
        Some(other) => other.to_string(),
        None => "undefined".into(),
    };
    // A number that isn't a whole version reads as newer: it is no
    // version this build ships.
    let version = version.as_u64().unwrap_or(u64::MAX);
    Err(ScanError::Unsupported(format_version_refusal(&id, version)))
}

/// Reads a current header line that passed [`refuse_foreign_version`].
/// (dsh: the `isHeaderLine` guard and `fromHeaderLine`.)
pub(crate) fn header_of(value: &Value) -> Option<SessionHeader> {
    let line = value.as_object()?;
    if !HEADER_REQUIRED.iter().all(|key| line.contains_key(*key))
        || !line.keys().all(|key| {
            HEADER_REQUIRED.contains(&key.as_str()) || HEADER_OPTIONAL.contains(&key.as_str())
        })
        || line.get("type")? != "session"
        || line.get("version")?.as_u64() != Some(SESSION_FORMAT_VERSION)
    {
        return None;
    }
    let optional_string = |key: &str| match line.get(key) {
        None => Some(None),
        Some(Value::String(text)) => Some(Some(text.clone())),
        Some(_) => None,
    };
    let cwd = optional_string("cwd")?;
    if cwd
        .as_deref()
        .is_some_and(|cwd| !Path::new(cwd).is_absolute())
    {
        return None;
    }
    let origin = match line.get("origin") {
        None => None,
        Some(origin) if origin == "subagent" => Some(karaka_session::Origin::Subagent),
        Some(_) => return None,
    };
    Some(SessionHeader {
        id: SessionId::new(line.get("id")?.as_str()?),
        created_at: count(line.get("createdAt"))?,
        cwd,
        parent_session: optional_string("parentSession")?.map(SessionId::new),
        is_seeded: line.get("isSeeded")?.as_bool()?,
        origin,
        delegation_depth: count(line.get("delegationDepth"))?,
        agent_preset: optional_string("agentPreset")?,
    })
}

/// Reads the first record of a log, newline included. (dsh:
/// `parseHeaderRecord`.)
pub(crate) fn parse_header_record(record: &[u8]) -> Result<SessionHeader, ScanError> {
    let corrupt = |message: &str| ScanError::Corrupt(message.into());
    if record.last() != Some(&b'\n')
        || record.iter().position(|byte| *byte == b'\n') != Some(record.len() - 1)
    {
        return Err(corrupt("empty or header-less session log"));
    }
    let parsed: Value = serde_json::from_slice(&record[..record.len() - 1])
        .map_err(|_| corrupt("corrupt session log: header line is not valid JSON"))?;
    if !parsed.is_object() {
        return Err(corrupt(
            "corrupt session log: first line is not a JSON object",
        ));
    }
    refuse_foreign_version(&parsed)?;
    assert_no_retired_header_fields(&parsed).map_err(ScanError::Corrupt)?;
    header_of(&parsed)
        .ok_or_else(|| corrupt("corrupt session log: first line is not a session header"))
}

/// One event as its row. (dsh: `eventLine`, through v4's `encodeEvent`.)
///
/// The row passes the v4 admission checks first, so nothing is stored that
/// a reader would refuse.
pub(crate) fn event_line(entry: &Entry) -> Result<String, PersistenceError> {
    let mut row = serde_json::to_value(entry).map_err(|error| {
        PersistenceError::Other(format!(
            "session event at seq {} can't be written: {error}",
            entry.seq
        ))
    })?;
    admission::admit_row(&row)
        .map_err(|refusal| PersistenceError::Other(refusal.message().to_owned()))?;
    if let Some(sources) = row.get_mut("sourceEventSeqs") {
        let seqs: Vec<u64> = sources
            .as_array()
            .and_then(|list| list.iter().map(Value::as_u64).collect())
            .ok_or_else(|| {
                PersistenceError::Other(format!(
                    "session event at seq {} has sourceEventSeqs that aren't a list of seqs",
                    entry.seq
                ))
            })?;
        *sources = encode_ranges(&seqs);
    }
    Ok(row.to_string())
}

/// `[start, end]` for each run of three or more consecutive seqs, the
/// rest as they are; a list that isn't increasing is kept whole. (dsh:
/// `encodeSeqRanges`.)
fn encode_ranges(values: &[u64]) -> Value {
    if values.windows(2).any(|pair| pair[1] <= pair[0]) {
        return values.iter().copied().map(Value::from).collect();
    }
    let mut out = Vec::new();
    let mut index = 0;
    while index < values.len() {
        let start = values[index];
        let mut end = start;
        while index + 1 < values.len() && values[index + 1] == end + 1 {
            index += 1;
            end += 1;
        }
        if end - start >= 2 {
            out.push(Value::from(vec![start, end]));
        } else {
            out.push(Value::from(start));
            if end - start == 1 {
                out.push(Value::from(end));
            }
        }
        index += 1;
    }
    Value::Array(out)
}

/// Expands stored ranges back to the exact list, refusing ranges that
/// don't name unique earlier seqs. (dsh: `decodeSeqRanges`.)
fn decode_ranges(value: &Value, max_entries: u64) -> Result<Vec<u64>, String> {
    let list = value.as_array().ok_or("sourceEventSeqs must be an array")?;
    let member = |value: &Value, label: &str| {
        count(Some(value)).ok_or_else(|| format!("{label} must be a non-negative safe integer"))
    };
    let mut out: Vec<u64> = Vec::new();
    let mut has_range = false;
    for entry in list {
        let Some(pair) = entry.as_array() else {
            out.push(member(entry, "sourceEventSeqs member")?);
            continue;
        };
        if pair.len() != 2 {
            return Err("sourceEventSeqs range must be a [start, end] pair".into());
        }
        let start = member(&pair[0], "sourceEventSeqs range start")?;
        let end = member(&pair[1], "sourceEventSeqs range end")?;
        if start > end || end >= max_entries || end - start + 1 > max_entries - out.len() as u64 {
            return Err("sourceEventSeqs range exceeds its event seq".into());
        }
        out.extend(start..=end);
        has_range = true;
    }
    let mut seen = std::collections::HashSet::new();
    if out
        .iter()
        .any(|seq| *seq >= max_entries || !seen.insert(*seq))
    {
        return Err("sourceEventSeqs ranges must contain unique earlier seqs".into());
    }
    if has_range && out.windows(2).any(|pair| pair[1] <= pair[0]) {
        return Err("sourceEventSeqs ranges must be strictly increasing".into());
    }
    Ok(out)
}

/// Decodes one row's envelope, expanding its ranges. (dsh: v2's
/// `decodeEvent`.)
fn decode_row(value: Value, row: usize) -> Result<Value, String> {
    let Value::Object(mut record) = value else {
        return Err(format!("released v2 row {row} must be an object"));
    };
    if let Some(missing) = ROW_REQUIRED.iter().find(|key| !record.contains_key(**key)) {
        return Err(format!(
            "released v2 row {row} lacks required field {missing}"
        ));
    }
    if let Some(unexpected) = record
        .keys()
        .find(|key| !ROW_REQUIRED.contains(&key.as_str()) && !ROW_OPTIONAL.contains(&key.as_str()))
    {
        return Err(format!(
            "released v2 row {row} has unexpected field {unexpected}"
        ));
    }
    if !record["type"].is_string() {
        return Err(format!("released v2 row {row} type must be a string"));
    }
    if !record["time"].is_i64() && !record["time"].is_u64() {
        return Err(format!("released v2 row {row} time must be a safe integer"));
    }
    if record
        .get("ignorable")
        .is_some_and(|flag| flag != &Value::Bool(true))
    {
        return Err(format!(
            "released v2 row {row} ignorable must be true when present"
        ));
    }
    if let Some(sources) = record.get("sourceEventSeqs") {
        let seq = count(record.get("seq")).ok_or_else(|| {
            format!("released v2 row {row} seq must be a non-negative safe integer")
        })?;
        let expanded = decode_ranges(sources, seq)?;
        record.insert("sourceEventSeqs".into(), expanded.into());
    }
    Ok(Value::Object(record))
}

/// What a scan gives back.
#[derive(Debug)]
pub(crate) struct Scan {
    pub(crate) header: SessionHeader,
    /// Where the inherited events of a seeded log end.
    pub(crate) inherited_event_count: u64,
    /// The committed rows, envelopes decoded, not yet typed.
    pub(crate) rows: Vec<Value>,
    /// The length of the committed prefix: where the next append goes.
    pub(crate) committed_bytes: u64,
}

/// Reads rows after a header record, chunk by chunk, keeping only an
/// incomplete last line between chunks. (dsh: `SessionLogScanner`.)
pub(crate) struct Scanner {
    header: SessionHeader,
    strict: bool,
    rows: Vec<Value>,
    inherited: Option<u64>,
    fragment: Vec<u8>,
    input_bytes: u64,
    committed_bytes: u64,
    line: usize,
    issue: Option<String>,
    finished: bool,
}

impl Scanner {
    /// A scanner after `header_record`, which must be exactly one
    /// newline-terminated line. A `strict` scanner fails at the first
    /// damaged row instead of treating it as a torn tail.
    pub(crate) fn new(header_record: &[u8], strict: bool) -> Result<Self, ScanError> {
        let header = parse_header_record(header_record)?;
        let length = header_record.len() as u64;
        Ok(Self {
            header,
            strict,
            rows: Vec::new(),
            inherited: None,
            fragment: Vec::new(),
            input_bytes: length,
            committed_bytes: length,
            line: 0,
            issue: None,
            finished: false,
        })
    }

    /// Takes the next bytes of the file.
    pub(crate) fn write(&mut self, chunk: &[u8]) -> Result<(), ScanError> {
        if self.finished {
            return Err(ScanError::Corrupt(
                "cannot write to a finished session log scanner".into(),
            ));
        }
        let start = self.input_bytes;
        self.input_bytes += chunk.len() as u64;
        let mut line_start = 0;
        while let Some(offset) = chunk[line_start..].iter().position(|byte| *byte == b'\n') {
            let newline = line_start + offset;
            let line = if self.fragment.is_empty() {
                chunk[line_start..newline].to_vec()
            } else {
                let mut line = std::mem::take(&mut self.fragment);
                line.extend_from_slice(&chunk[line_start..newline]);
                line
            };
            self.consume(&line, start + newline as u64 + 1)?;
            line_start = newline + 1;
        }
        self.fragment.extend_from_slice(&chunk[line_start..]);
        Ok(())
    }

    /// Ends the scan; an incomplete last line is a torn tail.
    pub(crate) fn finish(mut self) -> Result<Scan, ScanError> {
        self.finished = true;
        // (dsh: the v2 decoder's `finish`, which v4 reuses.)
        if self.header.is_seeded && self.inherited.is_none() {
            return Err(ScanError::Corrupt(
                "released v2 seeded Session lacks an inherited end-seed marker".into(),
            ));
        }
        if !self.header.is_seeded && self.inherited.is_some() {
            return Err(ScanError::Corrupt(
                "released v2 unseeded Session contains an inherited end-seed marker".into(),
            ));
        }
        Ok(Scan {
            header: self.header,
            inherited_event_count: self.inherited.unwrap_or(0),
            rows: self.rows,
            committed_bytes: self.committed_bytes,
        })
    }

    fn consume(&mut self, line: &[u8], end_byte: u64) -> Result<(), ScanError> {
        self.line += 1;
        let Ok(decoded) = serde_json::from_slice::<Value>(line) else {
            let issue = format!(
                "corrupt session log: unparsable committed event at line {}",
                self.line
            );
            if self.strict {
                return Err(ScanError::Corrupt(issue));
            }
            self.issue.get_or_insert(issue);
            return Ok(());
        };
        // Admission comes before recovery: a refused row refuses the file,
        // even after an earlier damaged one.
        admission::admit_row(&decoded)?;
        let is_turn_end = decoded.get("type") == Some(&Value::from("turn/end"));
        if let Some(issue) = &self.issue {
            if is_turn_end {
                return Err(ScanError::Corrupt(issue.clone()));
            }
            return Ok(());
        }
        let row_index = self.rows.len();
        let decoded = decode_row(decoded, row_index).and_then(|row| {
            let expected = self.rows.len() as u64;
            match row.get("seq").and_then(Value::as_u64) {
                Some(seq) if seq == expected => Ok(row),
                found => Err(format!(
                    "released v2 row {row_index} has seq gap (expected {expected}, got {})",
                    found.map_or_else(|| row["seq"].to_string(), |seq| seq.to_string())
                )),
            }
        });
        let row = match decoded {
            Ok(row) => row,
            Err(detail) => {
                let issue = format!(
                    "corrupt session log: invalid committed event at line {}: {detail}",
                    self.line
                );
                if self.strict || is_turn_end {
                    return Err(ScanError::Corrupt(issue));
                }
                self.issue = Some(issue);
                return Ok(());
            }
        };
        if row["type"] == "session/end-seed"
            && row.pointer("/data/inherited") == Some(&Value::Bool(true))
        {
            self.inherited = row["seq"].as_u64();
        }
        self.rows.push(row);
        self.committed_bytes = end_byte;
        Ok(())
    }
}

/// Scans a whole file: the header line, then its rows. (dsh: `scanLog`.)
pub(crate) fn scan_log(bytes: &[u8]) -> Result<Scan, ScanError> {
    let Some(header_end) = bytes.iter().position(|byte| *byte == b'\n') else {
        return Err(ScanError::Corrupt(
            "empty or header-less session log".into(),
        ));
    };
    let mut scanner = Scanner::new(&bytes[..=header_end], false)?;
    scanner.write(&bytes[header_end + 1..])?;
    scanner.finish()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn header(name: &str) -> String {
        header_line(&SessionHeader::new(SessionId::new(name), 1000), 0).unwrap()
    }

    fn log(lines: &[String]) -> Vec<u8> {
        format!("{}\n", lines.join("\n")).into_bytes()
    }

    fn row(value: Value) -> String {
        value.to_string()
    }

    fn turn_start(seq: u64) -> String {
        row(json!({ "type": "turn/start", "seq": seq, "time": 1, "data": { "turn": 1 } }))
    }

    fn turn_end(seq: u64) -> String {
        row(json!({
            "type": "turn/end", "seq": seq, "time": 2,
            "data": { "turn": 1, "reason": { "kind": "completed" } }
        }))
    }

    fn seqs(scan: &Scan) -> Vec<u64> {
        scan.rows
            .iter()
            .map(|row| row["seq"].as_u64().unwrap())
            .collect()
    }

    // jsonl.spec.ts: "requires exactly one newline-terminated header record"
    #[test]
    fn the_header_record_is_exactly_one_line() {
        let line = header("scanner-header");
        assert!(Scanner::new(b"", false).is_err());
        assert!(Scanner::new(line.as_bytes(), false).is_err());
        let twice = format!("{line}\n{line}\n");
        let Err(ScanError::Corrupt(message)) = Scanner::new(twice.as_bytes(), false) else {
            panic!("two header lines are refused");
        };
        assert!(message.contains("header-less"));
    }

    #[test]
    fn a_strict_scanner_fails_at_the_first_invalid_row() {
        let record = format!("{}\n", header("scanner-strict"));
        let mut scanner = Scanner::new(record.as_bytes(), true).unwrap();
        let Err(ScanError::Corrupt(message)) = scanner.write(b"null\n") else {
            panic!("a null row is refused");
        };
        assert!(message.contains("invalid committed event"));
    }

    #[test]
    fn header_shapes_are_checked() {
        for bad in [
            "42\n",
            "null\n",
            "not json\n",
            "{\"type\":\"event\"}\n",
            "{\"type\":\"session\",\"version\":4,\"id\":123}\n",
        ] {
            assert!(
                matches!(scan_log(bad.as_bytes()), Err(ScanError::Corrupt(_))),
                "{bad}"
            );
        }
        let foreign = "{\"type\":\"session\",\"version\":5,\"id\":123}\n";
        let Err(ScanError::Unsupported(message)) = scan_log(foreign.as_bytes()) else {
            panic!("a newer version is refused as unsupported");
        };
        assert!(message.contains("session \"123\" uses log format v5"));
        for created_at in ["1.5", "-1", "9007199254740992", "-0"] {
            let line = format!(
                "{{\"type\":\"session\",\"version\":4,\"id\":\"c\",\"createdAt\":{created_at},\"isSeeded\":false,\"delegationDepth\":0}}\n"
            );
            assert!(scan_log(line.as_bytes()).is_err(), "createdAt {created_at}");
        }
        let missing_seeded = "{\"type\":\"session\",\"version\":4,\"id\":\"c\",\"createdAt\":1,\"delegationDepth\":0}\n";
        assert!(scan_log(missing_seeded.as_bytes()).is_err());
        let retired = "{\"type\":\"session\",\"version\":4,\"id\":\"c\",\"createdAt\":1,\"isSeeded\":false,\"delegationDepth\":0,\"sandboxMode\":\"strict\"}\n";
        let Err(ScanError::Corrupt(message)) = scan_log(retired.as_bytes()) else {
            panic!("a retired field is refused");
        };
        assert_eq!(
            message,
            "session header uses retired policy baseline fields"
        );
    }

    #[test]
    fn every_optional_header_field_round_trips() {
        let full = SessionHeader {
            id: SessionId::new("full-header"),
            created_at: 5,
            cwd: Some("/w".into()),
            parent_session: Some(SessionId::new("parent")),
            is_seeded: true,
            origin: Some(karaka_session::Origin::Subagent),
            delegation_depth: 2,
            agent_preset: Some("minimal".into()),
        };
        let marker = row(json!({
            "type": "session/end-seed", "seq": 0, "time": 4, "data": { "inherited": true }
        }));
        let scan = scan_log(&log(&[header_line(&full, 0).unwrap(), marker])).unwrap();
        assert_eq!(scan.header, full);
        assert_eq!(scan.inherited_event_count, 0);
        assert_eq!(
            header_line(&full, 0).unwrap(),
            "{\"type\":\"session\",\"version\":4,\"id\":\"full-header\",\"createdAt\":5,\"cwd\":\"/w\",\"parentSession\":\"parent\",\"isSeeded\":true,\"origin\":\"subagent\",\"delegationDepth\":2,\"agentPreset\":\"minimal\"}"
        );
    }

    #[test]
    fn the_seed_cut_must_agree_with_the_header() {
        let mut seeded = SessionHeader::new(SessionId::new("seeded"), 1);
        seeded.is_seeded = true;
        let Err(ScanError::Corrupt(message)) = scan_log(&log(&[header_line(&seeded, 0).unwrap()]))
        else {
            panic!("a seeded log needs its marker");
        };
        assert!(message.contains("seeded Session lacks an inherited end-seed marker"));
        let marker = row(json!({
            "type": "session/end-seed", "seq": 0, "time": 1, "data": { "inherited": true }
        }));
        let Err(ScanError::Corrupt(message)) = scan_log(&log(&[header("unseeded"), marker])) else {
            panic!("an unseeded log can't have one");
        };
        assert!(message.contains("unseeded Session contains an inherited end-seed marker"));
        let unseeded = SessionHeader::new(SessionId::new("u"), 1);
        assert!(header_line(&unseeded, 1).is_err());
    }

    #[test]
    fn source_ranges_round_trip_exactly() {
        assert_eq!(
            encode_ranges(&[0, 1, 2, 3, 7, 9, 10]),
            json!([[0, 3], 7, 9, 10])
        );
        assert_eq!(encode_ranges(&[4, 5]), json!([4, 5]));
        assert_eq!(encode_ranges(&[5, 4, 3]), json!([5, 4, 3]));
        for list in [
            vec![0, 1, 2, 3, 7, 9, 10],
            vec![4, 5],
            vec![5, 4, 3],
            vec![2],
        ] {
            assert_eq!(decode_ranges(&encode_ranges(&list), 20).unwrap(), list);
        }
        assert!(decode_ranges(&json!([[0, 5]]), 3).is_err());
        assert!(decode_ranges(&json!([1, 1]), 3).is_err());
        assert!(decode_ranges(&json!([[0, 1, 2]]), 3).is_err());
        assert!(decode_ranges(&json!({}), 3).is_err());
    }

    #[test]
    fn stored_source_ranges_expand() {
        let user = json!({
            "type": "user/message", "seq": 2, "time": 3, "surfaceOp": "append",
            "sourceEventSeqs": [[0, 1]],
            "data": { "id": "u", "role": "user", "content": [{ "type": "text", "text": "hi" }],
                      "source": { "kind": "user" } }
        });
        let first = json!({
            "type": "user/message", "seq": 1, "time": 2, "surfaceOp": "append",
            "data": { "id": "u0", "role": "user", "content": [], "source": { "kind": "user" } }
        });
        let scan = scan_log(&log(&[
            header("ranges"),
            turn_start(0),
            row(first),
            row(user),
        ]))
        .unwrap();
        assert_eq!(scan.rows[2]["sourceEventSeqs"], json!([0, 1]));
    }

    // "handles empty writes, boundary newlines, torn fragments, and scanner completion"
    #[test]
    fn a_torn_fragment_is_never_committed() {
        let record = format!("{}\n", header("lifecycle"));
        let event = turn_start(0);
        let mut scanner = Scanner::new(record.as_bytes(), false).unwrap();
        scanner.write(b"").unwrap();
        scanner.write(event.as_bytes()).unwrap();
        scanner.write(b"\nignored torn tail").unwrap();
        let scan = scanner.finish().unwrap();
        assert_eq!(seqs(&scan), [0]);
        assert_eq!(
            scan.committed_bytes,
            (record.len() + event.len() + 1) as u64
        );
    }

    // "incrementally scans records split across reusable decoder chunks"
    #[test]
    fn a_record_split_inside_a_character_still_reads() {
        let record = format!("{}\n", header("incremental"));
        let user = row(json!({
            "type": "user/message", "seq": 1, "time": 2, "surfaceOp": "append",
            "data": { "id": "u", "role": "user", "content": [{ "type": "text", "text": "你好" }],
                      "source": { "kind": "user" } }
        }));
        let body = format!("{}\n{user}\n", turn_start(0));
        let split = body.find('你').unwrap() + 1;
        let mut scanner = Scanner::new(record.as_bytes(), false).unwrap();
        scanner.write(&body.as_bytes()[..split]).unwrap();
        scanner.write(&body.as_bytes()[split..]).unwrap();
        let scan = scanner.finish().unwrap();
        let whole = scan_log(format!("{record}{body}").as_bytes()).unwrap();
        assert_eq!(scan.rows, whole.rows);
        assert_eq!(scan.rows[1]["data"]["content"][0]["text"], "你好");
    }

    #[test]
    fn a_damaged_suffix_is_dropped_until_a_committed_turn_end() {
        // A corrupt line after the last turn end bounds the kept prefix.
        let scan = scan_log(&log(&[header("c2"), turn_start(0), "{not json".into()])).unwrap();
        assert_eq!(seqs(&scan), [0]);
        // So does a gap with no committed turn end after it.
        let gap = row(
            json!({ "type": "step/start", "seq": 2, "time": 2, "data": { "turn": 1, "step": 1 } }),
        );
        let scan = scan_log(&log(&[header("g"), turn_start(0), gap.clone()])).unwrap();
        assert_eq!(seqs(&scan), [0]);
        // A gap after a turn end is an uncommitted tail.
        let late = row(
            json!({ "type": "step/start", "seq": 9, "time": 3, "data": { "turn": 2, "step": 1 } }),
        );
        let scan = scan_log(&log(&[header("t"), turn_start(0), turn_end(1), late])).unwrap();
        assert_eq!(seqs(&scan), [0, 1]);
        // A gap before a later turn end is damage inside committed data.
        let Err(ScanError::Corrupt(message)) =
            scan_log(&log(&[header("g2"), turn_start(0), gap, turn_end(3)]))
        else {
            panic!("committed damage is refused");
        };
        assert!(message.contains("seq gap"));
    }

    #[test]
    fn malformed_records_before_a_committed_turn_end_are_refused() {
        for (record, expected) in [
            ("{not json".to_owned(), "unparsable committed event"),
            ("null".to_owned(), "invalid committed event"),
            (
                row(json!({ "type": "assistant/message", "sourceEventSeqs": [0], "data": {} })),
                "invalid committed event",
            ),
        ] {
            let Err(ScanError::Corrupt(message)) =
                scan_log(&log(&[header("c"), record, turn_end(1)]))
            else {
                panic!("refused");
            };
            assert!(message.contains(expected), "{message}");
        }
    }

    #[test]
    fn a_header_only_log_keeps_nothing() {
        let line = format!("{}\n", header("h0"));
        let scan = scan_log(line.as_bytes()).unwrap();
        assert!(scan.rows.is_empty());
        assert_eq!(scan.committed_bytes, line.len() as u64);
    }

    // current-event-admission.spec.ts and "rejects malformed native tool
    // results before recovery"
    #[test]
    fn admission_refuses_a_row_even_in_the_tail() {
        let tool =
            row(json!({ "type": "tool/result", "seq": 1, "time": 2, "data": { "message": [] } }));
        for sealed in [false, true] {
            let mut lines = vec![header("malformed-tool"), turn_start(0), tool.clone()];
            if sealed {
                lines.push(turn_end(2));
            }
            let Err(ScanError::Corrupt(message)) = scan_log(&log(&lines)) else {
                panic!("refused");
            };
            assert!(message.contains("tool/result at seq 1 message must be an object"));
        }
        // After an earlier damaged row, too.
        let ptc = row(json!({ "type": "tool/code-dispatch", "seq": 1, "time": 2, "data": {} }));
        let Err(ScanError::Unsupported(message)) = scan_log(&log(&[
            header("ptc"),
            turn_start(0),
            "{not json".into(),
            ptc,
        ])) else {
            panic!("refused");
        };
        assert!(message.contains("retired event type tool/code-dispatch"));
    }

    #[test]
    fn an_ignorable_retired_tag_is_kept() {
        let ptc = json!({
            "type": "tool/code-dispatch", "seq": 1, "time": 2, "ignorable": true,
            "data": { "rootCallId": "root", "parentCallId": "root", "subCallId": "child",
                      "name": "read", "arguments": {} }
        });
        let scan = scan_log(&log(&[header("keep"), turn_start(0), row(ptc.clone())])).unwrap();
        assert_eq!(scan.rows[1], ptc);
    }

    #[test]
    fn unknown_required_metadata_is_scanned_for_the_vocabulary_check() {
        // The scanner keeps it; the vocabulary check after refuses it.
        for metadata in [
            json!({ "surfaceOp": "append" }),
            json!({ "sourceEventSeqs": [] }),
        ] {
            let mut event = json!({ "type": "future/required", "seq": 0, "time": 1, "data": {} });
            event
                .as_object_mut()
                .unwrap()
                .extend(metadata.as_object().unwrap().clone());
            let bytes = log(&[header("future"), row(event)]);
            let scan = scan_log(&bytes).unwrap();
            assert_eq!(scan.rows.len(), 1);
            assert_eq!(scan.committed_bytes, bytes.len() as u64);
        }
    }
}
