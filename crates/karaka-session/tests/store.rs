//! The in-memory `SessionStore`: finding, creating and restoring sessions,
//! and many threads using it at once.

use std::collections::HashSet;
use std::sync::{Arc, Barrier, Mutex};
use std::thread;

use karaka_model::Message;
use karaka_session::{
    AdapterDefaults, CallConfig, Entry, Extra, FlushFuture, LogError, RequestContext,
    RequestHeader, RequestHeaderReason, Seq, SessionError, SessionEvent, SessionId,
    SessionListener, SessionStore, SurfaceIntent, SurfaceOp, Time, UserMessage, UserSource,
};

fn id(name: &str) -> SessionId {
    SessionId::new(name)
}

fn user(text: &str) -> SessionEvent {
    SessionEvent::user(UserMessage::text(
        format!("msg-{text}"),
        UserSource::user(),
        text,
    ))
}

/// The text of every user message in the session, in log order.
fn texts(store: &SessionStore, session: &str) -> Vec<String> {
    store
        .messages(&id(session))
        .unwrap()
        .iter()
        .map(Message::text)
        .collect()
}

#[test]
fn a_created_session_starts_empty_and_takes_appends() {
    let store = SessionStore::new();
    let session = store.create(id("a")).expect("the id is free");
    assert!(session.is_empty());

    assert_eq!(store.append(&id("a"), user("hi")), Ok(Seq(0)));
    assert_eq!(session.append(user("again")), Ok(Seq(1)));

    // The store and the handle reach the same log.
    assert_eq!(session.len(), 2);
    assert_eq!(
        store.messages(&id("a")),
        Ok(vec![Message::user("hi"), Message::user("again")])
    );
    let seqs: Vec<Seq> = store
        .events(&id("a"))
        .unwrap()
        .iter()
        .map(|entry| entry.seq)
        .collect();
    assert_eq!(seqs, [Seq(0), Seq(1)]);
}

#[test]
fn creating_an_id_twice_is_an_error() {
    let store = SessionStore::new();
    store.create(id("a")).expect("the id is free");
    store.append(&id("a"), user("hi")).unwrap();

    let error = store.create(id("a")).expect_err("the id is taken");
    assert_eq!(error, SessionError::AlreadyExists(id("a")));
    // The first session is untouched.
    assert_eq!(texts(&store, "a"), ["hi"]);
}

#[test]
fn an_unknown_id_is_an_error() {
    let store = SessionStore::new();
    let missing = SessionError::NotFound(id("nope"));

    assert!(store.get(&id("nope")).is_none());
    assert_eq!(store.append(&id("nope"), user("hi")), Err(missing.clone()));
    assert_eq!(
        store.append_all(&id("nope"), [user("hi")]),
        Err(missing.clone())
    );
    assert_eq!(store.messages(&id("nope")), Err(missing.clone()));
    assert_eq!(store.history(&id("nope")), Err(missing.clone()));
    assert_eq!(store.events(&id("nope")), Err(missing.clone()));
    assert_eq!(
        store.events_since(&id("nope"), Seq(0)),
        Err(missing.clone())
    );
    assert_eq!(missing.to_string(), "no session has the id \"nope\"");
}

#[test]
fn a_refused_event_is_reported_and_not_logged() {
    let store = SessionStore::new();
    store.create(id("a")).unwrap();
    let bad = SessionEvent::UserMessage {
        message: UserMessage::text("bad", UserSource::user(), "bad"),
        surface: SurfaceIntent {
            op: SurfaceOp::Replace {
                start: Seq(0),
                end: Seq(0),
            },
            sources: vec![Seq(0)],
        },
    };
    let error = store.append(&id("a"), bad).expect_err("nothing to replace");
    assert!(matches!(
        error,
        SessionError::Log(LogError::Invalid { seq: Seq(0), .. })
    ));
    assert!(store.events(&id("a")).unwrap().is_empty());
}

#[test]
fn open_or_create_creates_once_then_opens() {
    let store = SessionStore::new();
    let first = store.open_or_create(id("a"));
    first.append(user("hi")).unwrap();

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
    store
        .restore(
            id("e"),
            [Entry {
                seq: Seq(0),
                time: Time(1),
                ignorable: false,
                event: SessionEvent::turn_start(1),
            }],
        )
        .unwrap();
    assert_eq!(store.list(), [id("c"), id("a"), id("b"), id("d"), id("e")]);
}

#[test]
fn restore_keeps_the_stored_events_and_refuses_a_taken_id_or_a_gap() {
    let store = SessionStore::new();
    let stored = vec![
        Entry {
            seq: Seq(0),
            time: Time(10),
            ignorable: false,
            event: user("stored"),
        },
        Entry {
            seq: Seq(1),
            time: Time(20),
            ignorable: false,
            event: SessionEvent::turn_start(1),
        },
    ];
    let session = store.restore(id("a"), stored.clone()).unwrap();
    assert_eq!(session.events(), stored);
    // The next append continues the stored seqs.
    assert_eq!(session.append(user("live")), Ok(Seq(2)));

    assert_eq!(
        store.restore(id("a"), Vec::new()).map(|_| ()),
        Err(SessionError::AlreadyExists(id("a")))
    );
    let gap = vec![Entry {
        seq: Seq(3),
        ..stored[0].clone()
    }];
    assert_eq!(
        store.restore(id("b"), gap).map(|_| ()),
        Err(SessionError::Log(LogError::NotContiguous {
            expected: Seq(0),
            found: Seq(3)
        }))
    );
    // A refused restore adds no session.
    assert_eq!(store.list(), [id("a")]);
}

#[test]
fn events_since_gives_a_follower_only_what_it_has_not_seen() {
    let store = SessionStore::new();
    let session = store.create(id("a")).unwrap();
    session.append_all([user("one"), user("two")]).unwrap();

    // A follower that has seen seq 0 asks from seq 1.
    let unseen = store.events_since(&id("a"), Seq(1)).unwrap();
    assert_eq!(
        unseen.iter().map(|entry| entry.seq).collect::<Vec<_>>(),
        [Seq(1)]
    );
    session.append(user("three")).unwrap();
    let next = session.events_since(Seq(2));
    assert_eq!(next.len(), 1);
    assert_eq!(next[0].event, user("three"));
    assert!(session.events_since(Seq(3)).is_empty());
}

#[test]
fn the_store_and_its_handles_are_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<SessionStore>();
    assert_send_sync::<karaka_session::Session>();
}

#[test]
fn threads_racing_to_open_one_new_id_all_get_the_same_session() {
    const THREADS: usize = 16;
    let store = Arc::new(SessionStore::new());
    // Every thread waits here, so they all call `open_or_create` at once.
    let start = Arc::new(Barrier::new(THREADS));

    let workers: Vec<_> = (0..THREADS)
        .map(|thread| {
            let store = Arc::clone(&store);
            let start = Arc::clone(&start);
            thread::spawn(move || {
                start.wait();
                let session = store.open_or_create(id("shared"));
                session.append(user(&format!("from {thread}"))).unwrap();
            })
        })
        .collect();
    for worker in workers {
        worker.join().expect("no thread panicked");
    }

    // One session, holding every thread's append. Had two threads created
    // their own session, one of them would have been lost from the map.
    assert_eq!(store.list(), [id("shared")]);
    let mut seen = texts(&store, "shared");
    seen.sort();
    let mut expected: Vec<String> = (0..THREADS)
        .map(|thread| format!("from {thread}"))
        .collect();
    expected.sort();
    assert_eq!(seen, expected);
}

#[test]
fn a_batch_is_never_interleaved_with_another_writers_events() {
    const THREADS: usize = 8;
    const BATCHES: usize = 50;
    const BATCH: usize = 5;

    let store = Arc::new(SessionStore::new());
    let session = store.create(id("shared")).unwrap();
    let start = Arc::new(Barrier::new(THREADS));

    let workers: Vec<_> = (0..THREADS)
        .map(|thread| {
            let session = session.clone();
            let start = Arc::clone(&start);
            thread::spawn(move || {
                start.wait();
                for batch in 0..BATCHES {
                    let events = (0..BATCH).map(|n| user(&format!("{thread}/{batch}/{n}")));
                    session.append_all(events).unwrap();
                }
            })
        })
        .collect();
    for worker in workers {
        worker.join().expect("no thread panicked");
    }

    let seen = texts(&store, "shared");
    assert_eq!(seen.len(), THREADS * BATCHES * BATCH);
    // Every batch sits whole, in order, at a multiple of the batch size.
    let mut batches = HashSet::new();
    for batch in seen.chunks(BATCH) {
        let prefix = batch[0]
            .strip_suffix("/0")
            .expect("each chunk starts with a batch's first event");
        let expected: Vec<String> = (0..BATCH).map(|n| format!("{prefix}/{n}")).collect();
        assert_eq!(batch, expected);
        batches.insert(prefix.to_owned());
    }
    // And each batch is there once.
    assert_eq!(batches.len(), THREADS * BATCHES);
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
                    own.append(user(&format!("{thread}/{n}"))).unwrap();
                    store
                        .append(&id("shared"), user(&format!("{thread}/{n}")))
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
        let name = format!("own-{thread}");
        let events = store.events(&id(&name)).unwrap();
        // Only this thread wrote here, so its appends are in its own order.
        for (n, entry) in events.iter().enumerate() {
            assert_eq!(entry.seq, Seq(n as u64));
        }
        let expected: Vec<String> = (0..APPENDS).map(|n| format!("{thread}/{n}")).collect();
        assert_eq!(texts(&store, &name), expected);
    }

    // In the shared session the threads' appends interleave, but every seq
    // from 0 up is used exactly once, and every append is there.
    let shared = store.events(&id("shared")).unwrap();
    assert_eq!(shared.len(), THREADS * APPENDS);
    for (index, entry) in shared.iter().enumerate() {
        assert_eq!(entry.seq, Seq(index as u64));
    }
    let texts = texts(&store, "shared");
    for thread in 0..THREADS {
        // All of this thread's appends are there, in the order it made them.
        let prefix = format!("{thread}/");
        let mine: Vec<&String> = texts
            .iter()
            .filter(|text| text.starts_with(&prefix))
            .collect();
        let expected: Vec<String> = (0..APPENDS).map(|n| format!("{thread}/{n}")).collect();
        assert_eq!(mine, expected.iter().collect::<Vec<_>>());
    }
}

fn context(model: &str, window: Option<u64>) -> RequestContext {
    RequestContext {
        provider: "mock".into(),
        model: model.into(),
        context_window: window,
        system_prompt_update: None,
        extra: Extra::new(),
    }
}

/// dsh v4 `request-header.spec.ts`: "takes the latest full snapshot and
/// skips unrelated events", and the `Session.requestContext` cases.
#[test]
fn the_latest_header_and_context_are_in_force() {
    let store = SessionStore::new();
    let session = store.create(id("requests")).unwrap();
    assert_eq!(session.request_header(), None);
    assert_eq!(session.request_context(), None);

    let first = RequestHeader::new(CallConfig::new("mock", "m"), &[]).unwrap();
    let mut second = RequestHeader::new(CallConfig::new("mock", "other"), &[]).unwrap();
    session.append(SessionEvent::turn_start(1)).unwrap();
    session
        .append(SessionEvent::request_header(
            first,
            RequestHeaderReason::Initial,
        ))
        .unwrap();
    session
        .append(SessionEvent::RequestContext(context("m", Some(128_000))))
        .unwrap();
    session.append(user("hi")).unwrap();
    // A header with an empty adapter-defaults marker list is refused, as v4
    // refuses it, so the latest one stays in force.
    second.adapter_defaults = Some(AdapterDefaults::default());
    assert!(matches!(
        session.append(SessionEvent::request_header(
            second.clone(),
            RequestHeaderReason::Change
        )),
        Err(LogError::Invalid { .. })
    ));
    second.adapter_defaults = None;
    session
        .append(SessionEvent::request_header(
            second.clone(),
            RequestHeaderReason::Change,
        ))
        .unwrap();
    session
        .append(SessionEvent::RequestContext(context("other", None)))
        .unwrap();

    assert_eq!(session.request_header(), Some(second));
    assert_eq!(session.request_context(), Some(context("other", None)));
}

/// A request built after a replacement starts a new series; the count of
/// replacements is how the loop tells.
#[test]
fn each_surface_replacement_counts() {
    let store = SessionStore::new();
    let session = store.create(id("generations")).unwrap();
    session.append(user("hi")).unwrap();
    assert_eq!(session.surface_generation(), 0);
    let SessionEvent::UserMessage { message, .. } = user("again") else {
        unreachable!()
    };
    session
        .append(SessionEvent::UserMessage {
            message,
            surface: SurfaceIntent {
                op: SurfaceOp::Replace {
                    start: Seq(0),
                    end: Seq(0),
                },
                sources: vec![Seq(0)],
            },
        })
        .unwrap();
    assert_eq!(session.surface_generation(), 1);
}

/// Records what it hears, and keeps (or fails) the sessions it is told to.
#[derive(Default)]
struct Recorder {
    heard: Mutex<Vec<(String, Seq)>>,
    keeps: Vec<&'static str>,
    fails: bool,
}

impl SessionListener for Recorder {
    fn event(&self, id: &SessionId, entry: &Entry) {
        self.heard
            .lock()
            .unwrap()
            .push((id.as_str().to_owned(), entry.seq));
    }

    fn flush(&self, id: &SessionId) -> Option<FlushFuture> {
        if !self.keeps.contains(&id.as_str()) {
            return None;
        }
        let fails = self.fails;
        Some(Box::pin(async move {
            if fails {
                Err("the disk is full".into())
            } else {
                Ok(())
            }
        }))
    }
}

#[test]
fn a_listener_hears_appends_in_order_but_not_a_restored_seed() {
    let store = SessionStore::new();
    let recorder = Arc::new(Recorder::default());
    store.listen(recorder.clone());

    store
        .restore(
            id("resumed"),
            [Entry {
                seq: Seq(0),
                time: Time(1),
                ignorable: false,
                event: SessionEvent::turn_start(1),
            }],
        )
        .unwrap();
    assert!(recorder.heard.lock().unwrap().is_empty());

    // The turn the seed left open is closed, and the closers are heard.
    let closers = store.get(&id("resumed")).unwrap().repair().unwrap();
    assert!(!closers.is_empty());

    let session = store.create(id("fresh")).unwrap();
    session.append(user("one")).unwrap();
    session.append_all([user("two"), user("three")]).unwrap();

    let heard = recorder.heard.lock().unwrap().clone();
    let resumed: Vec<Seq> = heard
        .iter()
        .filter(|(name, _)| name == "resumed")
        .map(|(_, seq)| *seq)
        .collect();
    assert_eq!(resumed.first(), Some(&Seq(1)));
    let fresh: Vec<Seq> = heard
        .iter()
        .filter(|(name, _)| name == "fresh")
        .map(|(_, seq)| *seq)
        .collect();
    assert_eq!(
        fresh,
        (0..session.len() as u64).map(Seq).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn flush_says_whether_a_listener_keeps_the_session_and_reports_its_failure() {
    let store = SessionStore::new();
    assert!(!store.flush(&id("a")).await.unwrap());

    store.listen(Arc::new(Recorder {
        keeps: vec!["a"],
        ..Recorder::default()
    }));
    assert!(store.flush(&id("a")).await.unwrap());
    assert!(!store.flush(&id("b")).await.unwrap());

    store.listen(Arc::new(Recorder {
        keeps: vec!["a"],
        fails: true,
        ..Recorder::default()
    }));
    let failure = store.flush(&id("a")).await.unwrap_err();
    assert_eq!(failure.to_string(), "the disk is full");
}
