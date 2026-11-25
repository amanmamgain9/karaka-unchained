//! What is particular to the JSONL backend: its files, its refusals, and
//! its generations. (dsh: `jsonl.spec.ts`, `current-event-admission.spec.ts`
//! and `load-path.spec.ts`, where they apply to raw v4.)

mod common;

use std::fs;
use std::os::unix::fs::MetadataExt;

use common::*;
use karaka_kernel::Runtime;
use karaka_session::{Entry, SessionHeader};
use karaka_session_jsonl::{JsonlConfig, JsonlPersistence, JsonlPlugin, session_dir};
use karaka_session_persistence::{
    CreateOptions, FormatStatus, PersistenceError, SessionAccess, SessionPersistence,
};
use serde_json::{Value, json};

/// The v4 fixture `karaka-session` reads: one turn with every event Karaka
/// models, as Play This Life stores it. It holds no real chat.
const FIXTURE: &str = include_str!("../../karaka-session/tests/fixtures/v4-log.json");

fn header_line(id: &str, cwd: Option<&str>) -> String {
    let mut line = json!({ "type": "session", "version": 4, "id": id, "createdAt": 1000 });
    if let Some(cwd) = cwd {
        line["cwd"] = cwd.into();
    }
    line["isSeeded"] = false.into();
    line["delegationDepth"] = 0.into();
    line.to_string()
}

fn lines(rows: &[Value]) -> String {
    rows.iter().map(|row| format!("{row}\n")).collect()
}

fn start() -> Value {
    json!({ "type": "turn/start", "seq": 0, "time": 1, "data": { "turn": 1 } })
}

// --- the plugin (load-path.spec.ts) ---

#[tokio::test]
async fn the_plugin_provides_the_port() {
    let root = TempRoot::new("plugin");
    let runtime = Runtime::start(vec![Box::new(JsonlPlugin::new(JsonlConfig::new(
        root.path(),
    )))])
    .await
    .unwrap();
    assert_eq!(runtime.plugins(), ["session-persistence-jsonl"]);
    let persistence = runtime.get::<dyn SessionPersistence>();
    assert!(persistence.list().await.unwrap().is_empty());
}

#[test]
fn a_root_that_is_not_a_directory_is_refused() {
    let root = TempRoot::new("file-root");
    let file = root.path().join("not-a-dir");
    fs::write(&file, "x").unwrap();
    let refusal = JsonlPersistence::new(JsonlConfig::new(file)).unwrap_err();
    assert!(matches!(refusal, PersistenceError::Io { .. }));
}

#[test]
fn a_relative_root_is_resolved_once() {
    let persistence = JsonlPersistence::new(JsonlConfig::new("relative-sessions")).unwrap();
    assert!(persistence.root().is_absolute());
    assert!(persistence.root().ends_with("relative-sessions"));
}

// --- layout ---

#[tokio::test]
async fn the_log_lives_in_the_sessions_own_directory() {
    let root = TempRoot::new("layout");
    let persistence = root.backend();
    write_log(
        &persistence,
        meta("layout", Some("/work/app")),
        one_turn_log(),
    )
    .await;
    let dir = root.path().join("--work-app--").join("layout");
    assert_eq!(
        session_dir(root.path(), Some("/work/app"), "layout").unwrap(),
        dir
    );
    let mut names: Vec<String> = fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names, ["session.lock", "session.v4.jsonl"]);
    // A header line, then one row per event.
    let rows = rows(&dir.join("session.v4.jsonl"));
    assert_eq!(rows.len(), 1 + one_turn_log().len());
    assert_eq!(
        rows[0],
        serde_json::from_str::<Value>(&header_line("layout", Some("/work/app"))).unwrap()
    );
    // A session with no cwd goes under _no-cwd.
    write_log(&persistence, meta("loose", None), one_turn_log()).await;
    assert!(
        root.path()
            .join("_no-cwd")
            .join("loose")
            .join("session.v4.jsonl")
            .is_file()
    );
}

#[tokio::test]
async fn ids_that_climb_out_of_the_root_are_neutralized() {
    let root = TempRoot::new("traversal");
    let persistence = root.backend();
    for name in ["../../etc/pwn", "/etc/passwd", "..", "a\u{0}b"] {
        write_log(&persistence, meta(name, None), one_turn_log()).await;
        assert_eq!(read_all(&persistence, name).await.unwrap(), one_turn_log());
    }
    let files = root.files();
    assert_eq!(files.len(), 8);
    assert!(files.iter().all(|file| file.starts_with(root.path())));
}

#[tokio::test]
async fn sessions_are_listed_across_project_directories() {
    let root = TempRoot::new("projects");
    let persistence = root.backend();
    assert!(persistence.list().await.unwrap().is_empty());
    write_log(&persistence, meta("one", Some("/a/b-c")), one_turn_log()).await;
    // The same project directory, but its own session directory.
    write_log(&persistence, meta("two", Some("/a-b/c")), one_turn_log()).await;
    write_log(
        &persistence,
        meta("three", Some("/elsewhere")),
        one_turn_log(),
    )
    .await;
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 2);
    let mut listed: Vec<String> = persistence
        .list()
        .await
        .unwrap()
        .into_iter()
        .map(|snapshot| {
            assert_eq!(snapshot.format_status, Some(FormatStatus::Current));
            snapshot.header.id.to_string()
        })
        .collect();
    listed.sort();
    assert_eq!(listed, ["one", "three", "two"]);
    // A missing root has no sessions.
    let missing = JsonlPersistence::new(JsonlConfig::new(root.path().join("absent"))).unwrap();
    assert!(missing.list().await.unwrap().is_empty());
}

#[tokio::test]
async fn the_obsolete_flat_layout_is_refused() {
    let root = TempRoot::new("flat");
    fs::create_dir_all(root.path().join("_no-cwd")).unwrap();
    fs::write(
        root.path().join("_no-cwd").join("flat.jsonl"),
        header_line("flat", None),
    )
    .unwrap();
    let persistence = root.backend();
    let refusal = persistence.list().await.unwrap_err();
    assert!(refusal.to_string().contains("unsupported flat-file layout"));
    let refusal = persistence
        .open(&id("flat"), SessionAccess::Read)
        .await
        .err()
        .unwrap();
    assert!(refusal.to_string().contains("unsupported flat-file layout"));
}

#[tokio::test]
async fn a_compressed_log_is_refused_by_this_raw_backend() {
    let root = TempRoot::new("encoding");
    let path = root
        .generation(None, "packed", 4)
        .with_extension("jsonl.zstd");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, b"\x28\xb5\x2f\xfd").unwrap();
    let refusal = root
        .backend()
        .create(meta("other", None), CreateOptions::default())
        .await
        .err()
        .unwrap();
    assert!(refusal.to_string().contains("uses .jsonl.zstd"));
    assert!(refusal.to_string().contains("compression \"none\""));
}

// --- stored-format refusals ---

#[tokio::test]
async fn a_future_header_is_refused_as_unsupported_naming_the_raw_log() {
    let root = TempRoot::new("future-shape");
    let path = root.store(
        Some("/work"),
        "future-shape",
        42,
        &format!(
            "{}\n{{\"future\":\"row\"}}\n",
            json!({ "type": "session", "version": 42, "id": "future-shape", "futureOnly": true })
        ),
    );
    let persistence = root.backend();
    for access in [SessionAccess::Read, SessionAccess::Write] {
        let refusal = persistence
            .open(&id("future-shape"), access)
            .await
            .err()
            .unwrap();
        assert!(matches!(
            refusal,
            PersistenceError::FormatUnsupported { .. }
        ));
        let message = refusal.to_string();
        assert!(message.contains("written by a newer harness"));
        assert!(message.contains("upgrade the harness"));
        assert!(message.contains(&format!("(raw log: {})", path.display())));
    }
    // Listing skips it instead of failing the whole root.
    assert!(persistence.list().await.unwrap().is_empty());
}

#[tokio::test]
async fn an_unparsable_future_header_is_corruption() {
    let root = TempRoot::new("future-malformed");
    root.store(Some("/work"), "future-malformed", 42, "{not-json}\n");
    let refusal = root
        .backend()
        .open(&id("future-malformed"), SessionAccess::Read)
        .await
        .err()
        .unwrap();
    assert!(matches!(refusal, PersistenceError::Corruption(_)));
}

#[tokio::test]
async fn a_future_refusal_names_a_non_string_id() {
    let root = TempRoot::new("numeric-id");
    root.store(
        Some("/work"),
        "numeric-id",
        42,
        &format!(
            "{}\n",
            json!({ "type": "session", "version": 42, "id": 123 })
        ),
    );
    let refusal = root
        .backend()
        .open(&id("numeric-id"), SessionAccess::Read)
        .await
        .err()
        .unwrap();
    assert!(
        refusal
            .to_string()
            .contains("session \"123\" uses log format v42")
    );
}

#[tokio::test]
async fn a_current_log_that_does_not_read_is_corruption_naming_its_path() {
    for (name, content, reason) in [
        ("scalar", "42\n", "first line is not a JSON object"),
        ("invalid-json", "not json\n", "not valid JSON"),
        (
            "missing-newline",
            "{\"type\":\"session\"}",
            "header-less session log",
        ),
    ] {
        let root = TempRoot::new(name);
        let path = root.store(Some("/work"), name, 4, content);
        let refusal = root
            .backend()
            .open(&id(name), SessionAccess::Read)
            .await
            .err()
            .unwrap();
        assert!(matches!(refusal, PersistenceError::Corruption(_)), "{name}");
        assert!(refusal.to_string().contains(reason), "{name}: {refusal}");
        assert!(
            refusal
                .to_string()
                .contains(&format!("(raw log: {})", path.display()))
        );
    }
}

#[tokio::test]
async fn a_retired_header_field_fails_stat_and_list() {
    let root = TempRoot::new("retired-policy");
    let mut line: Value =
        serde_json::from_str(&header_line("retired-policy", Some("/work"))).unwrap();
    line["sandboxMode"] = "strict".into();
    root.store(Some("/work"), "retired-policy", 4, &format!("{line}\n"));
    let persistence = root.backend();
    let refusal = persistence.stat(&id("retired-policy")).await.unwrap_err();
    assert!(
        refusal
            .to_string()
            .contains("retired policy baseline fields")
    );
    let refusal = persistence.list().await.unwrap_err();
    assert!(
        refusal
            .to_string()
            .contains("retired policy baseline fields")
    );
}

#[tokio::test]
async fn a_legacy_event_type_or_reason_is_refused() {
    let root = TempRoot::new("legacy");
    let header_delta = json!({
        "type": "request/header-delta", "seq": 1, "time": 2, "data": { "config": { "model": "legacy" } }
    });
    root.store(
        Some("/legacy"),
        "legacy-header-delta",
        4,
        &format!(
            "{}\n{}",
            header_line("legacy-header-delta", Some("/legacy")),
            lines(&[start(), header_delta])
        ),
    );
    let refusal = read_all(&root.backend(), "legacy-header-delta")
        .await
        .unwrap_err();
    assert!(
        refusal.to_string().contains(
            "contains event type \"request/header-delta\" (seq 1) unknown to this harness"
        )
    );

    let fallback = json!({
        "type": "request/header", "seq": 1, "time": 2,
        "data": { "header": { "config": { "provider": "mock", "model": "legacy" } }, "reason": "fallback" }
    });
    root.store(
        Some("/legacy"),
        "legacy-header-fallback",
        4,
        &format!(
            "{}\n{}",
            header_line("legacy-header-fallback", Some("/legacy")),
            lines(&[start(), fallback])
        ),
    );
    let refusal = read_all(&root.backend(), "legacy-header-fallback")
        .await
        .unwrap_err();
    assert!(
        refusal
            .to_string()
            .contains("unsupported legacy reason \"fallback\"")
    );
}

#[tokio::test]
async fn a_header_naming_another_session_is_refused_and_nothing_changes() {
    let root = TempRoot::new("identity");
    let persistence = root.backend();
    write_log(
        &persistence,
        meta("identity-a", Some("/same")),
        one_turn_log(),
    )
    .await;
    write_log(
        &persistence,
        meta("identity-b", Some("/same")),
        one_turn_log(),
    )
    .await;
    let a = root.log(Some("/same"), "identity-a");
    let content = fs::read_to_string(&a).unwrap();
    let mut rows: Vec<String> = content.lines().map(str::to_owned).collect();
    rows[0] = rows[0].replace("identity-a", "identity-b");
    fs::write(&a, format!("{}\n", rows.join("\n"))).unwrap();
    let before = fs::read(&a).unwrap();

    let refusal = read_all(&root.backend(), "identity-a").await.unwrap_err();
    assert!(
        refusal
            .to_string()
            .contains("requested id \"identity-a\" does not match header id \"identity-b\"")
    );
    assert_eq!(fs::read(&a).unwrap(), before);
}

// --- generations ---

#[tokio::test]
async fn an_older_generation_is_refused_and_never_written() {
    let root = TempRoot::new("older");
    let v3 = json!({
        "type": "session", "version": 3, "id": "older", "createdAt": 1000, "cwd": "/work",
        "isSeeded": false, "delegationDepth": 0
    });
    let content = format!("{v3}\n{}", lines(&[start()]));
    let path = root.store(Some("/work"), "older", 3, &content);
    let inode = fs::metadata(&path).unwrap().ino();
    let persistence = root.backend();

    for access in [SessionAccess::Read, SessionAccess::Write] {
        let refusal = persistence.open(&id("older"), access).await.err().unwrap();
        assert!(matches!(
            refusal,
            PersistenceError::FormatUnsupported { .. }
        ));
        let message = refusal.to_string();
        assert!(message.contains(
            "session \"older\" uses log format v3, older than the supported v4, and this build \
             ships no upgrade path for it"
        ));
        assert!(message.contains(&format!("(raw log: {})", path.display())));
    }
    // An older generation is an existing id.
    let refusal = persistence
        .create(meta("older", Some("/work")), CreateOptions::default())
        .await
        .err()
        .unwrap();
    assert!(matches!(refusal, PersistenceError::AlreadyExists(_)));
    // Observing refuses it too, and listing skips it.
    assert!(matches!(
        persistence.stat(&id("older")).await.unwrap_err(),
        PersistenceError::FormatUnsupported { .. }
    ));
    assert!(persistence.list().await.unwrap().is_empty());

    // The file is byte for byte what it was, the same inode, and alone: no
    // successor, no lock.
    assert_eq!(fs::read_to_string(&path).unwrap(), content);
    assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
    assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
}

#[tokio::test]
async fn the_highest_generation_is_selected() {
    let root = TempRoot::new("selection");
    let persistence = root.backend();
    write_log(
        &persistence,
        meta("selection", Some("/work")),
        one_turn_log(),
    )
    .await;
    // A retained older generation beside the current one doesn't matter.
    root.store(Some("/work"), "selection", 1, "{}\n");
    assert_eq!(
        read_all(&persistence, "selection").await.unwrap(),
        one_turn_log()
    );
    // A newer one wins, and is refused.
    root.store(
        Some("/work"),
        "selection",
        7,
        &format!(
            "{}\n",
            json!({ "type": "session", "version": 7, "id": "selection" })
        ),
    );
    let refusal = read_all(&persistence, "selection").await.unwrap_err();
    assert!(refusal.to_string().contains("uses log format v7"));
}

#[tokio::test]
async fn a_filename_and_header_that_disagree_are_refused() {
    let root = TempRoot::new("disagree");
    let line = json!({ "type": "session", "version": 3, "id": "disagree", "createdAt": 1, "isSeeded": false, "delegationDepth": 0 });
    root.store(None, "disagree", 4, &format!("{line}\n"));
    let refusal = root.backend().stat(&id("disagree")).await.unwrap_err();
    assert!(
        refusal
            .to_string()
            .contains("session generation filename identifies v4, but its header identifies v3")
    );
}

// --- durability ---

#[tokio::test]
async fn nothing_is_written_until_the_first_append() {
    let root = TempRoot::new("lazy-files");
    let persistence = root.backend();
    let header = meta("lazy", Some("/work"));
    let handle = persistence
        .create(header, CreateOptions::default())
        .await
        .unwrap();
    assert!(root.files().is_empty());
    let pending = persistence.stat(&id("lazy")).await.unwrap().unwrap();
    assert_eq!(pending.size_bytes, None);
    assert_eq!(pending.format_status, None);
    assert!(pending.revision.starts_with("memory:"));

    handle.append(one_turn_log()).await.unwrap();
    let path = root.log(Some("/work"), "lazy");
    assert!(path.is_file());
    let stored = persistence.stat(&id("lazy")).await.unwrap().unwrap();
    assert_eq!(stored.format_status, Some(FormatStatus::Current));
    assert_eq!(stored.size_bytes, Some(fs::metadata(&path).unwrap().len()));
    handle.close().await.unwrap();
}

#[tokio::test]
async fn an_explicitly_flushed_empty_session_is_its_header_line() {
    let root = TempRoot::new("flushed-empty");
    let persistence = root.backend();
    let handle = persistence
        .create(
            meta("durable-empty", Some("/work")),
            CreateOptions::default(),
        )
        .await
        .unwrap();
    handle.flush().await.unwrap();
    handle.close().await.unwrap();
    assert_eq!(
        fs::read_to_string(root.log(Some("/work"), "durable-empty")).unwrap(),
        format!("{}\n", header_line("durable-empty", Some("/work")))
    );
    assert!(
        read_all(&persistence, "durable-empty")
            .await
            .unwrap()
            .is_empty()
    );
}

/// jsonl.spec.ts: "a torn crash tail is served as the committed prefix
/// and repaired only by the write path".
#[tokio::test]
async fn complete_rows_after_the_last_turn_end_are_served_and_only_the_fragment_is_cut() {
    let root = TempRoot::new("torn");
    write_log(&root.backend(), meta("torn", Some("/work")), one_turn_log()).await;
    let path = root.log(Some("/work"), "torn");
    let committed = std::fs::read(&path).unwrap();
    // A crash in the middle of a second turn: two complete rows past the
    // turn end, and a torn fragment with no newline.
    let tail = format!(
        "{}\n{}\n{}",
        json!({ "type": "turn/start", "seq": 6, "time": 8, "data": { "turn": 2 } }),
        json!({ "type": "step/start", "seq": 7, "time": 9, "data": { "turn": 2, "step": 1 } }),
        "{\"type\":\"assistant/chunk\",\"seq\":8,\"ti"
    );
    append_bytes(&path, tail.as_bytes());

    // A reader serves the complete rows, not the fragment, and repairs
    // nothing.
    let reader = root.backend();
    assert_eq!(
        seqs(&read_all(&reader, "torn").await.unwrap()),
        [0, 1, 2, 3, 4, 5, 6, 7]
    );
    assert_eq!(
        std::fs::read(&path).unwrap(),
        [committed.clone(), tail.into_bytes()].concat()
    );

    // The write path cuts the fragment before its first append.
    let writer = reader
        .open(&id("torn"), SessionAccess::Write)
        .await
        .unwrap();
    writer
        .append(entries(json!([
            { "type": "step/end", "seq": 8, "time": 10, "data": { "turn": 2, "step": 1 } },
            { "type": "turn/end", "seq": 9, "time": 11, "data": { "turn": 2, "reason": { "kind": "interrupted" } } }
        ])))
        .await
        .unwrap();
    writer.close().await.unwrap();
    let repaired = std::fs::read_to_string(&path).unwrap();
    assert!(repaired.as_bytes().starts_with(&committed));
    assert!(!repaired.contains("assistant/chunk"));

    let verify = root.backend();
    assert_eq!(
        seqs(&read_all(&verify, "torn").await.unwrap()),
        [0, 1, 2, 3, 4, 5, 6, 7, 8, 9]
    );
}

#[tokio::test]
async fn a_stored_open_turn_is_served_as_stored() {
    // Closing an interrupted turn is the reader's job.
    let root = TempRoot::new("open-turn");
    let persistence = root.backend();
    write_log(
        &persistence,
        meta("open-turn", None),
        entries(json!([start()])),
    )
    .await;
    let events = read_all(&persistence, "open-turn").await.unwrap();
    assert_eq!(events.len(), 1);
}

#[tokio::test]
async fn a_read_handle_refuses_a_log_that_shrank() {
    let root = TempRoot::new("shrank");
    let persistence = root.backend();
    write_log(&persistence, meta("shrank", None), one_turn_log()).await;
    let reader = persistence
        .open(&id("shrank"), SessionAccess::Read)
        .await
        .unwrap();
    assert_eq!(reader.read(0, None).await.unwrap().len(), 6);
    let path = root.log(None, "shrank");
    let content = fs::read_to_string(&path).unwrap();
    let kept: Vec<&str> = content.lines().take(3).collect();
    fs::write(&path, format!("{}\n", kept.join("\n"))).unwrap();
    let refusal = reader.read(0, None).await.unwrap_err();
    assert!(
        refusal
            .to_string()
            .contains("stored log shrank below a previously observed prefix (2 < 6)")
    );
    reader.close().await.unwrap();
}

#[tokio::test]
async fn source_event_seqs_are_stored_as_ranges_and_read_back_exactly() {
    let root = TempRoot::new("ranges");
    let persistence = root.backend();
    let mut log = one_turn_log();
    let replacement: Entry = serde_json::from_value(json!({
        "type": "user/message", "seq": 6, "time": 7,
        "surfaceOp": { "op": "replace", "startSeq": 1, "endSeq": 3 },
        "sourceEventSeqs": [1, 2, 3],
        "data": { "id": "summary", "role": "user", "content": [{ "type": "text", "text": "short" }],
                  "source": { "kind": "user" } }
    }))
    .unwrap();
    log.push(replacement);
    write_log(&persistence, meta("ranges", None), log.clone()).await;
    let stored = rows(&root.log(None, "ranges"));
    assert_eq!(stored[7]["sourceEventSeqs"], json!([[1, 3]]));
    assert_eq!(read_all(&persistence, "ranges").await.unwrap(), log);
}

#[tokio::test]
async fn the_v4_fixture_round_trips_event_for_event() {
    let fixture: Vec<Value> = serde_json::from_str(FIXTURE).unwrap();
    let events: Vec<Entry> = serde_json::from_str(FIXTURE).unwrap();
    let root = TempRoot::new("fixture");
    let header = SessionHeader::new(id("fixture"), 1_700_000_000_000).with_cwd("/work");
    write_log(&root.backend(), header.clone(), events.clone()).await;

    // Another instance reads back the same events...
    let fresh = root.backend();
    let read = read_all(&fresh, "fixture").await.unwrap();
    assert_eq!(read, events);
    // ...each one the fixture's JSON, value for value.
    let written: Vec<Value> = read
        .iter()
        .map(|entry| serde_json::to_value(entry).unwrap())
        .collect();
    assert_eq!(written, fixture);
    // And the file is its header and one row per event.
    let stored = rows(&root.log(Some("/work"), "fixture"));
    assert_eq!(stored.len(), fixture.len() + 1);
    let opened = fresh
        .open(&id("fixture"), SessionAccess::Read)
        .await
        .unwrap();
    assert_eq!(opened.header(), &header);
    opened.close().await.unwrap();
}

// --- current-event-admission.spec.ts ---

#[tokio::test]
async fn unknown_required_metadata_is_refused_without_touching_the_file() {
    for (name, metadata) in [
        ("surface-op", json!({ "surfaceOp": "append" })),
        ("sources", json!({ "sourceEventSeqs": [] })),
    ] {
        let root = TempRoot::new(name);
        let mut event = json!({ "type": "future/required", "seq": 0, "time": 1, "data": {} });
        event
            .as_object_mut()
            .unwrap()
            .extend(metadata.as_object().unwrap().clone());
        let content = format!(
            "{}\n{}",
            header_line("current-admission", None),
            lines(&[event])
        );
        let path = root.store(None, "current-admission", 4, &content);
        let inode = fs::metadata(&path).unwrap().ino();
        let persistence = root.backend();
        for access in [SessionAccess::Read, SessionAccess::Write] {
            let refusal = persistence
                .open(&id("current-admission"), access)
                .await
                .err()
                .unwrap();
            assert!(
                refusal
                    .to_string()
                    .contains("unknown to this harness and not marked ignorable")
            );
            assert_eq!(fs::read_to_string(&path).unwrap(), content);
            assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
        }
    }
}

#[tokio::test]
async fn an_ignorable_retired_tag_is_kept_through_a_write() {
    for kind in ["tool/code-dispatch-start", "tool/code-dispatch"] {
        let root = TempRoot::new("ignorable");
        let event = json!({
            "type": kind, "seq": 1, "time": 2, "ignorable": true,
            "data": { "rootCallId": "root", "parentCallId": "root", "subCallId": "child",
                      "name": "read", "arguments": {} }
        });
        let content = format!(
            "{}\n{}",
            header_line("current-admission", None),
            lines(&[start(), event.clone()])
        );
        let path = root.store(None, "current-admission", 4, &content);
        let persistence = root.backend();
        let writer = persistence
            .open(&id("current-admission"), SessionAccess::Write)
            .await
            .unwrap();
        let read = writer.read(0, None).await.unwrap();
        assert_eq!(serde_json::to_value(&read[1]).unwrap(), event);
        writer
            .append(entries(json!([
                { "type": "turn/end", "seq": 2, "time": 3, "data": { "turn": 1, "reason": { "kind": "completed" } } }
            ])))
            .await
            .unwrap();
        writer.close().await.unwrap();
        assert!(fs::read_to_string(&path).unwrap().starts_with(&content));
    }
}

#[tokio::test]
async fn a_write_this_build_would_refuse_to_read_is_refused() {
    let root = TempRoot::new("write-admission");
    let persistence = root.backend();
    let handle = persistence
        .create(meta("write-admission", None), CreateOptions::default())
        .await
        .unwrap();
    let retired = entries(json!([
        { "type": "tool/code-dispatch", "seq": 0, "time": 1, "data": {} }
    ]));
    let refusal = handle.append(retired).await.unwrap_err();
    assert!(
        refusal
            .to_string()
            .contains("retired event type tool/code-dispatch")
    );
    // Nothing was written.
    assert!(root.files().is_empty());
    handle.close().await.unwrap();
}
