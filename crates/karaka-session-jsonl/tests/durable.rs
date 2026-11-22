//! The session store kept on this backend: live events routed to the
//! session's writer, flush as the barrier, resume with repair, and one
//! writer across instances. (dsh: `runLiveWritePathContract` as
//! `jsonl.spec.ts` runs it, and the agent loop's `resumeWith`.)

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use karaka_kernel::Runtime;
use karaka_session::{
    Entry, SessionEvent, SessionStore, SessionsPlugin, TurnEndReason, UserMessage, UserSource,
};
use karaka_session_jsonl::{JsonlConfig, JsonlPlugin};
use karaka_session_persistence::{
    BATCH_WINDOW, DurableError, DurableSessions, DurableSessionsPlugin, PersistenceError,
    SessionPersistence,
};

/// A store kept on a fresh backend over `root`, as one process has it.
fn durable(root: &TempRoot) -> (Arc<SessionStore>, DurableSessions) {
    let store = Arc::new(SessionStore::new());
    let persistence: Arc<dyn SessionPersistence> = Arc::new(root.backend());
    let durable = DurableSessions::new(Arc::clone(&store), persistence);
    (store, durable)
}

fn user(text: &str) -> SessionEvent {
    SessionEvent::user(UserMessage::text(
        format!("user-{text}"),
        UserSource::user(),
        text,
    ))
}

/// What another process reading the root sees.
async fn stored(root: &TempRoot, name: &str) -> Vec<Entry> {
    read_all(&root.backend(), name).await.unwrap()
}

#[tokio::test]
async fn appended_events_are_written_within_one_batching_window() {
    let root = TempRoot::new("routed");
    let (_store, durable) = durable(&root);
    let session = durable
        .open(id("routed"), Some("/work".into()))
        .await
        .unwrap();
    session.append(SessionEvent::turn_start(1)).unwrap();
    session.append(user("hello")).unwrap();
    // Nothing is stored before the window ends...
    assert!(root.files().is_empty());
    tokio::time::sleep(BATCH_WINDOW + Duration::from_millis(300)).await;
    // ...and both are after.
    assert_eq!(stored(&root, "routed").await, session.events());
    durable.close().await.unwrap();
}

#[tokio::test]
async fn the_store_flush_is_the_barrier() {
    let root = TempRoot::new("barrier");
    let (store, durable) = durable(&root);
    let session = durable.open(id("barrier"), None).await.unwrap();
    session.append(SessionEvent::turn_start(1)).unwrap();
    assert!(store.flush(&id("barrier")).await.unwrap());
    assert_eq!(stored(&root, "barrier").await, session.events());
    durable.close().await.unwrap();
}

#[tokio::test]
async fn a_session_not_opened_through_storage_stays_in_memory() {
    let root = TempRoot::new("in-memory");
    let (store, durable) = durable(&root);
    let session = store.create(id("loose")).unwrap();
    session.append(SessionEvent::turn_start(1)).unwrap();
    assert!(!store.flush(&id("loose")).await.unwrap());
    durable.flush().await.unwrap();
    assert!(root.files().is_empty());
    // And it can't be made durable after the fact.
    let refusal = durable.open(id("loose"), None).await.unwrap_err();
    assert!(matches!(refusal, DurableError::NotStored(_)));
}

#[tokio::test]
async fn a_restarted_process_carries_on_the_stored_session() {
    let root = TempRoot::new("restart");
    let first = {
        let (store, durable) = durable(&root);
        let session = durable.open(id("restart"), None).await.unwrap();
        session
            .append_all([SessionEvent::turn_start(1), user("hello")])
            .unwrap();
        session
            .append(SessionEvent::TurnEnd(karaka_session::TurnEnd {
                turn: 1,
                reason: TurnEndReason::Completed,
                extra: Default::default(),
            }))
            .unwrap();
        assert!(store.flush(&id("restart")).await.unwrap());
        durable.close().await.unwrap();
        session.events()
    };

    let (store, durable) = durable(&root);
    let session = durable.open(id("restart"), None).await.unwrap();
    // The same events, every seq and time kept, and nothing to repair.
    assert_eq!(session.events(), first);
    session.append(SessionEvent::turn_start(2)).unwrap();
    assert!(store.flush(&id("restart")).await.unwrap());
    let read = stored(&root, "restart").await;
    assert_eq!(seqs(&read), [0, 1, 2, 3]);
    durable.close().await.unwrap();
}

#[tokio::test]
async fn resuming_closes_a_turn_a_crash_left_open_and_stores_the_closers() {
    let root = TempRoot::new("repair");
    // A crash after the turn started: the stored log ends inside it.
    write_log(
        &root.backend(),
        meta("repair", None),
        entries(serde_json::json!([
            { "type": "turn/start", "seq": 0, "time": 1, "data": { "turn": 1 } },
            { "type": "step/start", "seq": 1, "time": 2, "data": { "turn": 1, "step": 1 } }
        ])),
    )
    .await;

    let (_store, durable) = durable(&root);
    let session = durable.open(id("repair"), None).await.unwrap();
    let events = session.events();
    assert!(events.len() > 2);
    assert!(matches!(
        events.last().map(|entry| &entry.event),
        Some(SessionEvent::TurnEnd(_))
    ));
    // The closers are durable before the session carries on.
    assert_eq!(stored(&root, "repair").await, events);
    durable.close().await.unwrap();
}

#[tokio::test]
async fn another_instance_cannot_take_a_session_this_one_writes() {
    let root = TempRoot::new("owned");
    let (_store, first) = durable(&root);
    let session = first.open(id("owned"), None).await.unwrap();
    session.append(SessionEvent::turn_start(1)).unwrap();
    first.flush().await.unwrap();

    let (_other, second) = durable(&root);
    let refusal = second.open(id("owned"), None).await.unwrap_err();
    assert!(matches!(
        refusal,
        DurableError::Persistence(PersistenceError::AlreadyOwned(_))
    ));

    // Once the first gives the session up, the second resumes it.
    first.close().await.unwrap();
    let resumed = second.open(id("owned"), None).await.unwrap();
    // It finds the turn the first left open, and closes it.
    let events = resumed.events();
    assert_eq!(events[..1], session.events()[..]);
    assert!(matches!(events[1].event, SessionEvent::TurnEnd(_)));
    second.close().await.unwrap();
}

#[tokio::test]
async fn close_drains_what_is_buffered() {
    let root = TempRoot::new("close-drains");
    let (_store, durable) = durable(&root);
    let session = durable.open(id("close-drains"), None).await.unwrap();
    session.append(SessionEvent::turn_start(1)).unwrap();
    durable.close().await.unwrap();
    assert_eq!(stored(&root, "close-drains").await, session.events());
    // Later events aren't stored.
    session.append(user("after")).unwrap();
    assert_eq!(stored(&root, "close-drains").await.len(), 1);
}

#[tokio::test]
async fn opening_twice_gives_the_same_session() {
    let root = TempRoot::new("twice");
    let (_store, durable) = durable(&root);
    let first = durable.open(id("twice"), None).await.unwrap();
    first.append(SessionEvent::turn_start(1)).unwrap();
    let second = durable.open(id("twice"), None).await.unwrap();
    assert_eq!(second.len(), 1);
    assert!(durable.is_stored(&id("twice")));
    durable.close().await.unwrap();
}

#[tokio::test]
async fn the_plugins_keep_the_store_on_the_backend() {
    let root = TempRoot::new("plugins");
    let runtime = Runtime::start(vec![
        Box::new(SessionsPlugin),
        Box::new(JsonlPlugin::new(JsonlConfig {
            root: root.path().to_owned(),
        })),
        Box::new(DurableSessionsPlugin),
    ])
    .await
    .unwrap();
    let durable = runtime.get::<DurableSessions>();
    let session = durable.open(id("plugged"), None).await.unwrap();
    session.append(SessionEvent::turn_start(1)).unwrap();
    assert!(
        runtime
            .get::<SessionStore>()
            .flush(&id("plugged"))
            .await
            .unwrap()
    );
    assert_eq!(stored(&root, "plugged").await, session.events());
    durable.close().await.unwrap();
}
