//! The in-memory `SessionStore`: finding and creating sessions, and many
//! threads using it at once.

use std::sync::Arc;
use std::thread;

use karaka_model::Message;
use karaka_session::{Seq, SessionError, SessionEvent, SessionId, SessionStore};

fn id(name: &str) -> SessionId {
    SessionId::new(name)
}

#[test]
fn a_created_session_starts_empty_and_takes_appends() {
    let store = SessionStore::new();
    let session = store.create(id("a")).expect("the id is free");
    assert!(session.is_empty());

    assert_eq!(store.append(&id("a"), SessionEvent::user("hi")), Ok(Seq(0)));
    assert_eq!(session.append(SessionEvent::user("again")), Seq(1));

    // The store and the handle reach the same log.
    assert_eq!(session.len(), 2);
    assert_eq!(
        store.messages(&id("a")),
        Ok(vec![Message::user("hi"), Message::user("again")])
    );
    assert_eq!(store.events(&id("a")).map(|events| events.len()), Ok(2));
}

#[test]
fn creating_an_id_twice_is_an_error() {
    let store = SessionStore::new();
    store.create(id("a")).expect("the id is free");
    store.append(&id("a"), SessionEvent::user("hi")).unwrap();

    let error = store.create(id("a")).expect_err("the id is taken");
    assert_eq!(error, SessionError::AlreadyExists(id("a")));
    // The first session is untouched.
    assert_eq!(store.events(&id("a")).unwrap().len(), 1);
}

#[test]
fn an_unknown_id_is_an_error() {
    let store = SessionStore::new();
    let missing = SessionError::NotFound(id("nope"));

    assert!(store.get(&id("nope")).is_none());
    assert_eq!(
        store.append(&id("nope"), SessionEvent::user("hi")),
        Err(missing.clone())
    );
    assert_eq!(store.messages(&id("nope")), Err(missing.clone()));
    assert_eq!(store.events(&id("nope")), Err(missing.clone()));
    assert_eq!(missing.to_string(), "no session has the id \"nope\"");
}

#[test]
fn open_or_create_creates_once_then_opens() {
    let store = SessionStore::new();
    let first = store.open_or_create(id("a"));
    first.append(SessionEvent::user("hi"));

    let second = store.open_or_create(id("a"));
    assert_eq!(second.id(), &id("a"));
    assert_eq!(second.len(), 1);
    assert_eq!(store.list(), [id("a")]);
}

#[test]
fn list_gives_ids_in_the_order_they_were_created() {
    let store = SessionStore::new();
    for name in ["c", "a", "b"] {
        store.create(id(name)).unwrap();
    }
    store.open_or_create(id("a"));
    store.open_or_create(id("d"));
    assert_eq!(store.list(), [id("c"), id("a"), id("b"), id("d")]);
}

#[test]
fn the_store_and_its_handles_are_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<SessionStore>();
    assert_send_sync::<karaka_session::Session>();
}

#[test]
fn many_threads_append_at_once_without_losing_or_reusing_a_seq() {
    const THREADS: usize = 8;
    const APPENDS: usize = 200;

    let store = Arc::new(SessionStore::new());
    store.create(id("shared")).unwrap();

    let workers: Vec<_> = (0..THREADS)
        .map(|thread| {
            let store = Arc::clone(&store);
            thread::spawn(move || {
                // Every thread opens its own session, all at the same time.
                let own = store.open_or_create(id(&format!("own-{thread}")));
                for n in 0..APPENDS {
                    own.append(SessionEvent::user(format!("{thread}/{n}")));
                    store
                        .append(&id("shared"), SessionEvent::user(format!("{thread}/{n}")))
                        .expect("the shared session exists");
                }
            })
        })
        .collect();
    for worker in workers {
        worker.join().expect("no thread panicked");
    }

    assert_eq!(store.list().len(), THREADS + 1);
    for thread in 0..THREADS {
        let events = store.events(&id(&format!("own-{thread}"))).unwrap();
        assert_eq!(events.len(), APPENDS);
        // Only this thread wrote here, so its appends are in its own order.
        for (n, entry) in events.iter().enumerate() {
            assert_eq!(entry.seq, Seq(n as u64));
            assert_eq!(entry.event, SessionEvent::user(format!("{thread}/{n}")));
        }
    }

    // In the shared session the threads' appends interleave, but every seq
    // from 0 up is used exactly once, and every append is there.
    let shared = store.events(&id("shared")).unwrap();
    assert_eq!(shared.len(), THREADS * APPENDS);
    for (index, entry) in shared.iter().enumerate() {
        assert_eq!(entry.seq, Seq(index as u64));
    }
    let texts: Vec<String> = store
        .messages(&id("shared"))
        .unwrap()
        .iter()
        .map(Message::text)
        .collect();
    for thread in 0..THREADS {
        // All of this thread's appends are there, in the order it made them.
        let mine: Vec<&String> = texts
            .iter()
            .filter(|text| text.starts_with(&format!("{thread}/")))
            .collect();
        let expected: Vec<String> = (0..APPENDS).map(|n| format!("{thread}/{n}")).collect();
        assert_eq!(mine, expected.iter().collect::<Vec<_>>());
    }
}
