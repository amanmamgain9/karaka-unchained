//! The seam's contract, held against this backend. (dsh:
//! `runPersistenceContract` in `session-persistence/tests/contract.ts`, as
//! `jsonl.spec.ts` runs it for `compression: 'none'`.)
//!
//! Left out: read arguments that are negative or fractional, and event data
//! that isn't JSON, which Rust's types rule out; and `await using`, which is
//! a handle dropped without closing here.

mod common;

use std::fs::OpenOptions;
use std::io::Write;

use common::*;
use karaka_session::SessionId;
use karaka_session_persistence::{
    CreateOptions, PersistenceError, SessionAccess, SessionPersistence,
};
use serde_json::json;

#[tokio::test]
async fn round_trips_through_one_write_handle() {
    let root = TempRoot::new("round-trip");
    let persistence = root.backend();
    let header = meta("round-trip", Some("/work"));
    let log = one_turn_log();
    let handle = persistence
        .create(header.clone(), CreateOptions::default())
        .await
        .unwrap();
    assert_eq!(handle.access(), SessionAccess::Write);
    assert_eq!(handle.id(), &id("round-trip"));
    assert_eq!(handle.header(), &header);

    handle.append(log.clone()).await.unwrap();
    // An empty batch does nothing.
    handle.append(Vec::new()).await.unwrap();
    // A write handle reads its own appends.
    assert_eq!(handle.read(0, None).await.unwrap(), log);
    assert_eq!(handle.read(3, None).await.unwrap(), log[3..]);
    assert_eq!(handle.read(0, Some(2)).await.unwrap(), log[..2]);
    assert_eq!(handle.read(1, Some(3)).await.unwrap(), log[1..4]);
    // At or past the end: empty, never an error.
    assert!(handle.read(log.len(), None).await.unwrap().is_empty());
    assert!(handle.read(log.len() + 100, None).await.unwrap().is_empty());
    // A flush after a durable append is already satisfied.
    handle.flush().await.unwrap();
    handle.close().await.unwrap();
}

#[tokio::test]
async fn a_duplicate_create_is_refused_until_the_creator_closes_unwritten() {
    let root = TempRoot::new("dup-pending");
    let persistence = root.backend();
    let first = persistence
        .create(meta("dup-pending", None), CreateOptions::default())
        .await
        .unwrap();
    let refusal = persistence
        .create(meta("dup-pending", None), CreateOptions::default())
        .await
        .err()
        .unwrap();
    assert!(matches!(refusal, PersistenceError::AlreadyExists(_)));
    // Closing a creator that never wrote erases the session.
    first.close().await.unwrap();
    let second = persistence
        .create(meta("dup-pending", None), CreateOptions::default())
        .await
        .unwrap();
    second.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn of_two_concurrent_creates_one_wins() {
    let root = TempRoot::new("dup-race");
    let persistence = root.backend();
    let (a, b) = tokio::join!(
        persistence.create(meta("dup-race", None), CreateOptions::default()),
        persistence.create(meta("dup-race", None), CreateOptions::default()),
    );
    let (winner, loser) = match (a, b) {
        (Ok(winner), Err(loser)) | (Err(loser), Ok(winner)) => (winner, loser),
        _ => panic!("exactly one create wins"),
    };
    assert!(matches!(loser, PersistenceError::AlreadyExists(_)));
    winner.close().await.unwrap();
}

#[tokio::test]
async fn a_duplicate_create_is_refused_against_a_stored_session() {
    let root = TempRoot::new("dup-stored");
    write_log(
        &root.backend(),
        meta("dup-stored", Some("/work")),
        one_turn_log(),
    )
    .await;
    let fresh = root.backend();
    let refusal = fresh
        .create(meta("dup-stored", Some("/work")), CreateOptions::default())
        .await
        .err()
        .unwrap();
    assert!(matches!(refusal, PersistenceError::AlreadyExists(_)));
}

#[tokio::test]
async fn opening_an_absent_session_is_not_found() {
    let root = TempRoot::new("absent");
    let persistence = root.backend();
    for access in [SessionAccess::Read, SessionAccess::Write] {
        let refusal = persistence.open(&id("absent"), access).await.err().unwrap();
        assert!(matches!(refusal, PersistenceError::NotFound(_)));
        assert_eq!(refusal.to_string(), "session \"absent\" not found");
    }
}

#[tokio::test]
async fn one_write_handle_at_a_time_until_it_closes() {
    let root = TempRoot::new("owned");
    let persistence = root.backend();
    let creator = persistence
        .create(meta("owned", None), CreateOptions::default())
        .await
        .unwrap();
    // The creator owns the session even before it is written.
    let refusal = persistence
        .open(&id("owned"), SessionAccess::Write)
        .await
        .err()
        .unwrap();
    assert!(matches!(refusal, PersistenceError::AlreadyOwned(_)));
    creator.append(one_turn_log()).await.unwrap();
    let refusal = persistence
        .open(&id("owned"), SessionAccess::Write)
        .await
        .err()
        .unwrap();
    assert_eq!(
        refusal.to_string(),
        "session \"owned\" is already owned by an active write handle"
    );
    creator.close().await.unwrap();

    // After close, a new writer continues at the next seq.
    let writer = persistence
        .open(&id("owned"), SessionAccess::Write)
        .await
        .unwrap();
    writer.append(second_turn(6)).await.unwrap();
    assert_eq!(
        seqs(&writer.read(0, None).await.unwrap()),
        [0, 1, 2, 3, 4, 5, 6, 7]
    );
    writer.close().await.unwrap();
}

#[tokio::test]
async fn a_read_handle_refuses_append_and_flush() {
    let root = TempRoot::new("read-only");
    let persistence = root.backend();
    write_log(&persistence, meta("read-only", None), one_turn_log()).await;
    let reader = persistence
        .open(&id("read-only"), SessionAccess::Read)
        .await
        .unwrap();
    assert_eq!(reader.access(), SessionAccess::Read);
    let refusal = reader.append(second_turn(6)).await.unwrap_err();
    assert_eq!(
        refusal.to_string(),
        "session \"read-only\": append is not available on a read handle"
    );
    assert!(matches!(
        reader.flush().await.unwrap_err(),
        PersistenceError::ReadOnly { .. }
    ));
    // The refusals changed nothing.
    assert_eq!(reader.read(0, None).await.unwrap(), one_turn_log());
    reader.close().await.unwrap();
}

#[tokio::test]
async fn a_closed_handle_refuses_and_a_dropped_one_gives_up_ownership() {
    let root = TempRoot::new("closed");
    let persistence = root.backend();
    let handle = persistence
        .create(meta("closed", None), CreateOptions::default())
        .await
        .unwrap();
    handle.append(one_turn_log()).await.unwrap();
    handle.close().await.unwrap();
    handle.close().await.unwrap();
    let refusal = handle.read(0, None).await.unwrap_err();
    assert_eq!(
        refusal.to_string(),
        "session \"closed\": read on a closed handle"
    );
    assert!(matches!(
        handle.append(second_turn(6)).await.unwrap_err(),
        PersistenceError::HandleClosed { .. }
    ));
    assert!(matches!(
        handle.flush().await.unwrap_err(),
        PersistenceError::HandleClosed { .. }
    ));

    {
        let writer = persistence
            .open(&id("closed"), SessionAccess::Write)
            .await
            .unwrap();
        writer.append(second_turn(6)).await.unwrap();
        // Dropped without closing.
    }
    let reopened = persistence
        .open(&id("closed"), SessionAccess::Write)
        .await
        .unwrap();
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn service_flush_stores_every_open_writer_and_not_a_closed_one() {
    let root = TempRoot::new("flush-all");
    let persistence = root.backend();
    let kept = persistence
        .create(meta("flush-all", None), CreateOptions::default())
        .await
        .unwrap();
    let abandoned = persistence
        .create(meta("flush-all-closing", None), CreateOptions::default())
        .await
        .unwrap();
    abandoned.close().await.unwrap();
    persistence.flush().await.unwrap();

    let fresh = root.backend();
    // The barrier stored the empty session...
    assert!(fresh.stat(&id("flush-all")).await.unwrap().is_some());
    // ...and the one closed unwritten never existed.
    assert!(
        fresh
            .stat(&id("flush-all-closing"))
            .await
            .unwrap()
            .is_none()
    );
    kept.close().await.unwrap();
}

#[tokio::test]
async fn a_created_session_is_visible_here_and_not_to_another_instance() {
    let root = TempRoot::new("lazy");
    let persistence = root.backend();
    let header = meta("lazy", Some("/work"));
    let creator = persistence
        .create(header.clone(), CreateOptions::default())
        .await
        .unwrap();
    assert!(creator.read(0, None).await.unwrap().is_empty());
    let snapshot = persistence.stat(&id("lazy")).await.unwrap().unwrap();
    assert_eq!(snapshot.header, header);
    let listed = persistence.list().await.unwrap();
    assert!(
        listed
            .iter()
            .any(|snapshot| snapshot.header.id == id("lazy"))
    );
    let reader = persistence
        .open(&id("lazy"), SessionAccess::Read)
        .await
        .unwrap();
    assert!(reader.read(0, None).await.unwrap().is_empty());
    reader.close().await.unwrap();

    let fresh = root.backend();
    assert!(fresh.stat(&id("lazy")).await.unwrap().is_none());
    assert!(fresh.list().await.unwrap().is_empty());
    let refusal = fresh
        .open(&id("lazy"), SessionAccess::Read)
        .await
        .err()
        .unwrap();
    assert!(matches!(refusal, PersistenceError::NotFound(_)));
    creator.close().await.unwrap();
}

#[tokio::test]
async fn closing_an_unwritten_session_erases_it() {
    let root = TempRoot::new("never-was");
    let persistence = root.backend();
    let creator = persistence
        .create(meta("never-was", None), CreateOptions::default())
        .await
        .unwrap();
    creator.close().await.unwrap();
    assert!(persistence.stat(&id("never-was")).await.unwrap().is_none());
    assert!(persistence.list().await.unwrap().is_empty());
    let refusal = persistence
        .open(&id("never-was"), SessionAccess::Read)
        .await
        .err()
        .unwrap();
    assert!(matches!(refusal, PersistenceError::NotFound(_)));
}

#[tokio::test]
async fn flush_stores_an_empty_session_for_another_instance() {
    let root = TempRoot::new("durable-empty");
    let header = meta("durable-empty", Some("/work"));
    let creator = root
        .backend()
        .create(header.clone(), CreateOptions::default())
        .await
        .unwrap();
    creator.flush().await.unwrap();
    creator.close().await.unwrap();

    let fresh = root.backend();
    let listed = fresh.list().await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].header, header);
    assert!(read_all(&fresh, "durable-empty").await.unwrap().is_empty());
}

#[tokio::test]
async fn reads_after_an_append_see_it_on_any_handle() {
    let root = TempRoot::new("fresh");
    let persistence = root.backend();
    let writer = persistence
        .create(meta("fresh", None), CreateOptions::default())
        .await
        .unwrap();
    writer.append(one_turn_log()).await.unwrap();
    let before = persistence
        .open(&id("fresh"), SessionAccess::Read)
        .await
        .unwrap();
    assert_eq!(before.read(0, None).await.unwrap(), one_turn_log());

    writer.append(second_turn(6)).await.unwrap();
    assert_eq!(
        seqs(&before.read(0, None).await.unwrap()),
        [0, 1, 2, 3, 4, 5, 6, 7]
    );
    let after = persistence
        .open(&id("fresh"), SessionAccess::Read)
        .await
        .unwrap();
    assert_eq!(
        seqs(&after.read(0, None).await.unwrap()),
        [0, 1, 2, 3, 4, 5, 6, 7]
    );
    before.close().await.unwrap();
    after.close().await.unwrap();
    writer.close().await.unwrap();
}

#[tokio::test]
async fn another_instance_continues_at_the_next_seq() {
    let root = TempRoot::new("continue");
    write_log(
        &root.backend(),
        meta("continue", Some("/work")),
        one_turn_log(),
    )
    .await;
    let fresh = root.backend();
    let writer = fresh
        .open(&id("continue"), SessionAccess::Write)
        .await
        .unwrap();
    writer.append(second_turn(6)).await.unwrap();
    assert_eq!(
        seqs(&writer.read(0, None).await.unwrap()),
        [0, 1, 2, 3, 4, 5, 6, 7]
    );
    writer.close().await.unwrap();
}

#[tokio::test]
async fn a_batch_that_does_not_continue_the_log_is_refused() {
    let root = TempRoot::new("contiguity");
    let persistence = root.backend();
    let handle = persistence
        .create(meta("contiguity", None), CreateOptions::default())
        .await
        .unwrap();
    handle.append(one_turn_log()).await.unwrap();
    // Appending stored seqs again is refused, not duplicated.
    let refusal = handle.append(one_turn_log()).await.unwrap_err();
    assert!(refusal.to_string().contains("expected 6"));
    // A gap inside a batch refuses the whole batch.
    let mut gapped = second_turn(6);
    gapped[1].seq.0 = 8;
    let refusal = handle.append(gapped).await.unwrap_err();
    assert!(refusal.to_string().contains("expected 7"));
    assert_eq!(
        seqs(&handle.read(0, None).await.unwrap()),
        [0, 1, 2, 3, 4, 5]
    );
    handle.close().await.unwrap();
}

#[tokio::test]
async fn an_unknown_stored_event_type_refuses_reads_and_write_opens() {
    let root = TempRoot::new("foreign-vocabulary");
    let persistence = root.backend();
    let handle = persistence
        .create(meta("foreign-vocabulary", None), CreateOptions::default())
        .await
        .unwrap();
    // Appends are permissive: a newer producer's type is stored as it is...
    handle.append(one_turn_log()).await.unwrap();
    handle
        .append(entries(json!([
            { "type": "mystery/event", "seq": 6, "time": 7, "data": { "payload": true } }
        ])))
        .await
        .unwrap();
    handle.close().await.unwrap();

    // ...but this build refuses to interpret it.
    for _ in 0..2 {
        // The failed open gave up its claim: the retry is refused the same
        // way, never as owned.
        let refusal = persistence
            .open(&id("foreign-vocabulary"), SessionAccess::Write)
            .await
            .err()
            .unwrap();
        assert!(matches!(
            refusal,
            PersistenceError::FormatUnsupported { .. }
        ));
    }
    // This backend refuses a read at its open, before any read.
    let refusal = persistence
        .open(&id("foreign-vocabulary"), SessionAccess::Read)
        .await
        .err()
        .unwrap();
    assert!(matches!(
        refusal,
        PersistenceError::FormatUnsupported { .. }
    ));
    assert!(refusal.to_string().contains("mystery/event"));
}

#[tokio::test]
async fn a_torn_tail_is_never_served_and_the_write_path_cuts_it() {
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
    OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(tail.as_bytes())
        .unwrap();

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
async fn stat_and_list_agree_on_revisions_that_change_with_an_append() {
    let root = TempRoot::new("revisions");
    let persistence = root.backend();
    let header = meta("revisions", Some("/work"));
    let writer = persistence
        .create(header.clone(), CreateOptions::default())
        .await
        .unwrap();
    writer.append(one_turn_log()).await.unwrap();

    let first = persistence.stat(&id("revisions")).await.unwrap().unwrap();
    let again = persistence.stat(&id("revisions")).await.unwrap().unwrap();
    let listed = persistence.list().await.unwrap().remove(0);
    assert_eq!(again.revision, first.revision);
    assert_eq!(listed.revision, first.revision);

    writer.append(second_turn(6)).await.unwrap();
    let changed = persistence.stat(&id("revisions")).await.unwrap().unwrap();
    assert_ne!(changed.revision, first.revision);
    assert_eq!(
        persistence.list().await.unwrap()[0].revision,
        changed.revision
    );

    // Every snapshot carries the stored header.
    let reader = persistence
        .open(&id("revisions"), SessionAccess::Read)
        .await
        .unwrap();
    assert_eq!(&changed.header, reader.header());
    assert_eq!(changed.header, header);
    reader.close().await.unwrap();
    writer.close().await.unwrap();
    assert!(
        persistence
            .stat(&SessionId::new("absent-stat"))
            .await
            .unwrap()
            .is_none()
    );
}
