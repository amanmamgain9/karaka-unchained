//! The compressed encoding, dsh's default: one checksummed frame for the
//! header and one per batch, torn-frame recovery, and the refusals of
//! damaged frames. (dsh: `zstd.spec.ts`.)

mod common;

use std::fs;

use common::*;
use karaka_session::SessionHeader;
use karaka_session_jsonl::frames::{compress_frame, scan_frames};
use karaka_session_jsonl::{Compression, JsonlConfig, JsonlPersistence};
use karaka_session_persistence::{
    CreateOptions, PersistenceError, SessionAccess, SessionPersistence,
};
use serde_json::{Value, json};

/// A synthetic two-turn session written with Node's built-in
/// `zlib.zstdCompress` and the checksum flag, exactly the call dsh's
/// `compressZstdFrame` makes: a header frame, then one frame per batch.
/// `fixtures/generate-node-zstd-session.mjs` wrote it. It holds no real
/// chat.
const NODE_WRITTEN: &[u8] = include_bytes!("fixtures/node-zstd-session.jsonl.zstd");

fn zstd(root: &TempRoot) -> JsonlPersistence {
    root.backend_in(Compression::Zstd)
}

fn log(root: &TempRoot, cwd: Option<&str>, name: &str) -> std::path::PathBuf {
    root.log_in(cwd, name, Compression::Zstd)
}

fn header_line(header: &SessionHeader) -> Value {
    let mut line = json!({ "type": "session", "version": 4, "id": header.id.as_str(), "createdAt": header.created_at });
    if let Some(cwd) = &header.cwd {
        line["cwd"] = cwd.as_str().into();
    }
    line["isSeeded"] = false.into();
    line["delegationDepth"] = 0.into();
    line
}

/// The rows of a compressed log, header first.
fn rows_of(bytes: &[u8]) -> Vec<Value> {
    decode_complete_frames(bytes)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn as_rows(entries: &[karaka_session::Entry]) -> Vec<Value> {
    entries
        .iter()
        .map(|entry| serde_json::to_value(entry).unwrap())
        .collect()
}

/// A turn left open by a crash, the last of its records half flushed into a
/// torn frame.
fn open_turn_frame() -> (Vec<Value>, Vec<u8>) {
    let rows = vec![
        json!({ "type": "turn/start", "seq": 6, "time": 7, "data": { "turn": 2 } }),
        json!({ "type": "step/start", "seq": 7, "time": 8, "data": { "turn": 2, "step": 1 } }),
        json!({ "type": "assistant/attempt", "seq": 8, "time": 9, "data": {
            "turn": 2, "step": 1,
            "stream": [{ "type": "text-chunks", "time0": 9, "index": 0, "dt": [], "texts": [noise(300_000)] }]
        } }),
    ];
    let plaintext: String = rows.iter().map(|row| format!("{row}\n")).collect();
    let torn = torn_frame(&plaintext, |decoded| {
        decoded.matches('\n').count() >= 2 && !decoded.ends_with('\n')
    });
    (rows, torn)
}

fn closers() -> Vec<karaka_session::Entry> {
    entries(json!([
        { "type": "step/end", "seq": 8, "time": 10, "data": { "turn": 2, "step": 1 } },
        { "type": "turn/end", "seq": 9, "time": 11, "data": { "turn": 2, "reason": { "kind": "interrupted" } } }
    ]))
}

// --- the default ---

#[tokio::test]
async fn the_default_is_zstd_and_the_file_name_says_so() {
    let root = TempRoot::new("default-zstd");
    let persistence = JsonlPersistence::new(JsonlConfig::new(root.path())).unwrap();
    let header = meta("default-zstd", Some("/work"));
    write_log(&persistence, header.clone(), one_turn_log()).await;

    let path = log(&root, Some("/work"), "default-zstd");
    assert!(path.to_string_lossy().ends_with("session.v4.jsonl.zstd"));
    assert!(!root.log(Some("/work"), "default-zstd").exists());
    let bytes = fs::read(&path).unwrap();
    assert_eq!(bytes[..4], MAGIC);
    // One frame for the header, one for the first batch.
    assert_eq!(frame_count(&bytes), 2);
    let mut expected = vec![header_line(&header)];
    expected.extend(as_rows(&one_turn_log()));
    assert_eq!(rows_of(&bytes), expected);
    assert_eq!(
        read_all(&persistence, "default-zstd").await.unwrap(),
        one_turn_log()
    );
}

#[tokio::test]
async fn a_flushed_empty_session_is_one_header_frame() {
    let root = TempRoot::new("empty-zstd");
    let persistence = zstd(&root);
    let header = meta("empty-zstd", Some("/work"));
    let handle = persistence
        .create(header.clone(), CreateOptions::default())
        .await
        .unwrap();
    handle.flush().await.unwrap();
    handle.close().await.unwrap();
    let bytes = fs::read(log(&root, Some("/work"), "empty-zstd")).unwrap();
    assert_eq!(frame_count(&bytes), 1);
    assert_eq!(rows_of(&bytes), [header_line(&header)]);
    assert!(
        read_all(&persistence, "empty-zstd")
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn each_batch_is_one_more_frame_and_earlier_bytes_never_change() {
    let root = TempRoot::new("append-frame");
    let persistence = zstd(&root);
    let handle = persistence
        .create(meta("append-frame", None), CreateOptions::default())
        .await
        .unwrap();
    handle.append(one_turn_log()).await.unwrap();
    let path = log(&root, None, "append-frame");
    let before = fs::read(&path).unwrap();
    handle.append(second_turn(6)).await.unwrap();
    handle.close().await.unwrap();

    let after = fs::read(&path).unwrap();
    assert!(after.starts_with(&before));
    assert_eq!(frame_count(&after), 3);
    let mut all = one_turn_log();
    all.extend(second_turn(6));
    assert_eq!(read_all(&persistence, "append-frame").await.unwrap(), all);
}

#[tokio::test]
async fn a_zstd_log_round_trips_through_another_instance() {
    let root = TempRoot::new("round-trip-zstd");
    let mut all = one_turn_log();
    all.extend(second_turn(6));
    write_log(
        &zstd(&root),
        meta("round-trip", Some("/work")),
        one_turn_log(),
    )
    .await;
    let writer = zstd(&root)
        .open(&id("round-trip"), SessionAccess::Write)
        .await
        .unwrap();
    writer.append(second_turn(6)).await.unwrap();
    writer.close().await.unwrap();
    assert_eq!(read_all(&zstd(&root), "round-trip").await.unwrap(), all);
    let snapshot = zstd(&root).stat(&id("round-trip")).await.unwrap().unwrap();
    assert_eq!(snapshot.header, meta("round-trip", Some("/work")));
}

#[tokio::test]
async fn a_log_node_wrote_as_dsh_does_loads_and_carries_on() {
    let root = TempRoot::new("node-written");
    let path = log(&root, Some("/work"), "dsh-written");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, NODE_WRITTEN).unwrap();
    let persistence = zstd(&root);

    let mut expected = one_turn_log();
    expected.extend(second_turn(6));
    assert_eq!(
        read_all(&persistence, "dsh-written").await.unwrap(),
        expected
    );
    let listed = persistence.list().await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].header, meta("dsh-written", Some("/work")));

    // Karaka appends its own frame after Node's, leaving Node's bytes.
    let writer = persistence
        .open(&id("dsh-written"), SessionAccess::Write)
        .await
        .unwrap();
    writer.append(second_turn(8)).await.unwrap();
    writer.close().await.unwrap();
    let bytes = fs::read(&path).unwrap();
    assert!(bytes.starts_with(NODE_WRITTEN));
    assert_eq!(frame_count(&bytes), 4);
    expected.extend(second_turn(8));
    assert_eq!(
        read_all(&persistence, "dsh-written").await.unwrap(),
        expected
    );
}

// --- torn frames ---

#[tokio::test]
async fn a_frame_torn_mid_batch_keeps_its_flushed_records_and_is_repaired() {
    let root = TempRoot::new("recover-torn");
    write_log(
        &zstd(&root),
        meta("recover-torn", Some("/proj")),
        one_turn_log(),
    )
    .await;
    let path = log(&root, Some("/proj"), "recover-torn");
    let committed = fs::read(&path).unwrap();
    let (rows, torn) = open_turn_frame();
    append_bytes(&path, &torn);

    // The records already flushed into the torn frame are real: a read
    // serves them, without the half-written third, and changes nothing.
    let read = read_all(&zstd(&root), "recover-torn").await.unwrap();
    assert_eq!(seqs(&read), [0, 1, 2, 3, 4, 5, 6, 7]);
    assert_eq!(as_rows(&read[6..]), rows[..2]);
    assert_eq!(fs::read(&path).unwrap(), [committed.clone(), torn].concat());

    // The first append cuts the torn bytes, writes the recovered records
    // again as a frame of their own, then the batch.
    let writer = zstd(&root)
        .open(&id("recover-torn"), SessionAccess::Write)
        .await
        .unwrap();
    writer.append(closers()).await.unwrap();
    writer.close().await.unwrap();
    let repaired = fs::read(&path).unwrap();
    assert!(repaired.starts_with(&committed));
    assert_eq!(frame_count(&repaired), 4);
    let mut expected = as_rows(&one_turn_log());
    expected.extend(rows[..2].iter().cloned());
    expected.extend(as_rows(&closers()));
    assert_eq!(rows_of(&repaired)[1..], expected);
    assert_eq!(
        seqs(&read_all(&zstd(&root), "recover-torn").await.unwrap()),
        [0, 1, 2, 3, 4, 5, 6, 7, 8, 9]
    );
}

#[tokio::test]
async fn a_frame_torn_at_its_checksum_is_recovered_whole() {
    let root = TempRoot::new("partial-checksum");
    write_log(&zstd(&root), meta("partial-checksum", None), one_turn_log()).await;
    let path = log(&root, None, "partial-checksum");
    let committed = fs::read(&path).unwrap();
    let frame = frame_of(&as_rows(&second_turn(6)));
    append_bytes(&path, &frame[..frame.len() - 1]);

    let mut expected = one_turn_log();
    expected.extend(second_turn(6));
    assert_eq!(
        read_all(&zstd(&root), "partial-checksum").await.unwrap(),
        expected
    );
    let writer = zstd(&root)
        .open(&id("partial-checksum"), SessionAccess::Write)
        .await
        .unwrap();
    writer.append(second_turn(8)).await.unwrap();
    writer.close().await.unwrap();
    let repaired = fs::read(&path).unwrap();
    assert!(repaired.starts_with(&committed));
    expected.extend(second_turn(8));
    assert_eq!(rows_of(&repaired)[1..], as_rows(&expected));
}

#[tokio::test]
async fn a_frame_cut_inside_its_header_gives_nothing_and_the_earlier_frames_survive() {
    let root = TempRoot::new("partial-magic");
    write_log(&zstd(&root), meta("partial-magic", None), one_turn_log()).await;
    let path = log(&root, None, "partial-magic");
    let committed = fs::read(&path).unwrap();
    let frame = frame_of(&as_rows(&second_turn(6)));
    for cut in [2, 5] {
        fs::write(&path, [&committed[..], &frame[..cut]].concat()).unwrap();
        assert_eq!(
            read_all(&zstd(&root), "partial-magic").await.unwrap(),
            one_turn_log()
        );
        // Reads never repair.
        assert_eq!(fs::read(&path).unwrap().len(), committed.len() + cut);
    }
    // The next append cuts it off, with nothing to rewrite.
    let writer = zstd(&root)
        .open(&id("partial-magic"), SessionAccess::Write)
        .await
        .unwrap();
    writer.append(second_turn(6)).await.unwrap();
    writer.close().await.unwrap();
    let repaired = fs::read(&path).unwrap();
    assert!(repaired.starts_with(&committed));
    assert_eq!(frame_count(&repaired), 3);
}

#[tokio::test]
async fn a_torn_frame_with_no_complete_record_is_dropped_whole() {
    // A torn frame that never gave a complete record: nothing recovered.
    let root = TempRoot::new("torn-nothing");
    write_log(&zstd(&root), meta("torn-nothing", None), one_turn_log()).await;
    let path = log(&root, None, "torn-nothing");
    let line = format!(
        "{}\n",
        json!({ "type": "assistant/chunk", "seq": 6, "time": 9, "data": { "text": noise(300_000) } })
    );
    append_bytes(&path, &torn_frame(&line, |decoded| !decoded.contains('\n')));
    assert_eq!(
        read_all(&zstd(&root), "torn-nothing").await.unwrap(),
        one_turn_log()
    );
    let writer = zstd(&root)
        .open(&id("torn-nothing"), SessionAccess::Write)
        .await
        .unwrap();
    writer.append(second_turn(6)).await.unwrap();
    writer.close().await.unwrap();
    assert_eq!(
        seqs(&read_all(&zstd(&root), "torn-nothing").await.unwrap()),
        [0, 1, 2, 3, 4, 5, 6, 7]
    );
}

// --- refusals ---

async fn refusal(root: &TempRoot, name: &str) -> String {
    zstd(root)
        .open(&id(name), SessionAccess::Read)
        .await
        .err()
        .expect("the log is refused")
        .to_string()
}

#[tokio::test]
async fn a_complete_frame_ending_inside_a_record_is_refused() {
    let root = TempRoot::new("complete-bad-jsonl");
    write_log(
        &zstd(&root),
        meta("complete-bad-jsonl", None),
        one_turn_log(),
    )
    .await;
    append_bytes(
        &log(&root, None, "complete-bad-jsonl"),
        &compress_frame(b"{\"type\":\"turn/start\"").unwrap(),
    );
    let message = refusal(&root, "complete-bad-jsonl").await;
    assert!(
        message.contains("complete frame contains a torn JSONL record"),
        "{message}"
    );
}

#[tokio::test]
async fn bad_magic_and_reserved_bits_are_refused_with_dshs_words() {
    // Each damage, and how far into it dsh's text points.
    for (name, tail, expected, offset) in [
        ("bad-magic", vec![0, 0, 0, 0], "invalid frame magic", 0),
        (
            "reserved-bit",
            [&MAGIC[..], &[0x08]].concat(),
            "reserved frame-header bit",
            4,
        ),
        (
            "reserved-block",
            [&MAGIC[..], &[0x20, 0x00, 0x07, 0x00, 0x00]].concat(),
            "reserved block type",
            6,
        ),
    ] {
        let root = TempRoot::new(name);
        write_log(&zstd(&root), meta(name, None), one_turn_log()).await;
        let path = log(&root, None, name);
        let at = fs::metadata(&path).unwrap().len() + offset;
        append_bytes(&path, &tail);
        let message = refusal(&root, name).await;
        assert!(
            message.contains(&format!(
                "corrupt Zstandard session log: {expected} at byte {at}"
            )),
            "{message}"
        );
    }
}

#[tokio::test]
async fn a_complete_frame_whose_checksum_does_not_match_is_refused() {
    let root = TempRoot::new("bad-checksum");
    write_log(&zstd(&root), meta("bad-checksum", None), one_turn_log()).await;
    let path = log(&root, None, "bad-checksum");
    let mut bytes = fs::read(&path).unwrap();
    let frames = scan_frames(&bytes, None).unwrap().frames;
    let last = bytes.len() - 1;
    bytes[last] ^= 0xFF;
    fs::write(&path, &bytes).unwrap();
    let message = refusal(&root, "bad-checksum").await;
    assert!(
        message.contains(&format!(
            "corrupt Zstandard session log: frame at byte {} failed validation",
            frames[1].start
        )),
        "{message}"
    );
}

#[tokio::test]
async fn header_frames_that_are_missing_empty_or_damaged_are_refused_and_not_listed() {
    let root = TempRoot::new("headers");
    let store = |name: &str, bytes: &[u8]| {
        let path = log(&root, None, name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, bytes).unwrap();
    };
    let header = header_line(&meta("bad-checksum", None));
    let mut damaged = frame_of(&[header]);
    let last = damaged.len() - 1;
    damaged[last] ^= 0xFF;
    let two_lines = frame_of(&[
        header_line(&meta("two-lines", None)),
        json!({ "type": "turn/start" }),
    ]);
    store("empty", b"");
    store("partial-only", &MAGIC);
    store("empty-header", &compress_frame(b"").unwrap());
    store("two-lines", &two_lines);
    store("bad-checksum", &damaged);
    let persistence = zstd(&root);
    assert!(persistence.list().await.unwrap().is_empty());

    assert!(
        refusal(&root, "empty")
            .await
            .contains("empty or header-less Zstandard session log")
    );
    assert!(
        refusal(&root, "partial-only")
            .await
            .contains("empty or header-less Zstandard session log")
    );
    for name in ["empty-header", "two-lines"] {
        let message = refusal(&root, name).await;
        assert!(
            message.contains("first frame is not exactly one header line"),
            "{name}: {message}"
        );
    }
    let stat = persistence.stat(&id("bad-checksum")).await.unwrap_err();
    assert!(matches!(stat, PersistenceError::Corruption(_)));
    assert!(
        stat.to_string().contains("header frame failed validation"),
        "{stat}"
    );
    assert!(
        refusal(&root, "bad-checksum")
            .await
            .contains("frame at byte 0 failed validation")
    );
}

// --- encodings ---

#[tokio::test]
async fn a_root_in_the_other_encoding_is_refused_both_ways() {
    let raw_root = TempRoot::new("raw-mismatch");
    write_log(&raw_root.backend(), meta("raw-log", None), one_turn_log()).await;
    let refusal = zstd(&raw_root).list().await.unwrap_err();
    assert!(
        refusal
            .to_string()
            .contains("uses .jsonl, but this backend is configured for compression \"zstd\""),
        "{refusal}"
    );
    let refusal = zstd(&raw_root)
        .open(&id("raw-log"), SessionAccess::Read)
        .await
        .err()
        .unwrap();
    assert!(refusal.to_string().contains("uses .jsonl,"), "{refusal}");

    let zstd_root = TempRoot::new("zstd-mismatch");
    write_log(&zstd(&zstd_root), meta("zstd-log", None), one_turn_log()).await;
    let refusal = zstd_root.backend().list().await.unwrap_err();
    assert!(
        refusal
            .to_string()
            .contains("uses .jsonl.zstd, but this backend is configured for compression \"none\""),
        "{refusal}"
    );
}
