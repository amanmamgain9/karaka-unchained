//! Resuming through `DurableSessions` when storage fails part way, against
//! a backend in memory. (dsh: `resumeWith` in `agent-loop/src/index.ts`,
//! which closes the handle and never publishes the session when anything
//! before publication fails.)

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use karaka_session::{Entry, Seq, SessionEvent, SessionHeader, SessionId, SessionStore, Time};
use karaka_session_persistence::{
    CreateOptions, DurableSessions, PersistenceError, PersistenceFuture, SessionAccess,
    SessionHandle, SessionPersistence, SessionSnapshot,
};

/// One stored session, whose next `failures` appends fail.
struct Memory {
    stored: Mutex<Vec<Entry>>,
    failures: AtomicUsize,
    closed: AtomicUsize,
}

struct Handle {
    id: SessionId,
    header: SessionHeader,
    memory: Arc<Memory>,
}

impl SessionHandle for Handle {
    fn id(&self) -> &SessionId {
        &self.id
    }

    fn header(&self) -> &SessionHeader {
        &self.header
    }

    fn inherited_event_count(&self) -> u64 {
        0
    }

    fn access(&self) -> SessionAccess {
        SessionAccess::Write
    }

    fn read(&self, _: usize, _: Option<usize>) -> PersistenceFuture<'_, Vec<Entry>> {
        Box::pin(async { Ok(self.memory.stored.lock().unwrap().clone()) })
    }

    fn append(&self, events: Vec<Entry>) -> PersistenceFuture<'_, ()> {
        Box::pin(async move {
            let failures = &self.memory.failures;
            if failures.load(Ordering::SeqCst) > 0 {
                failures.fetch_sub(1, Ordering::SeqCst);
                return Err(PersistenceError::Other("simulated write failure".into()));
            }
            self.memory.stored.lock().unwrap().extend(events);
            Ok(())
        })
    }

    fn flush(&self) -> PersistenceFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }

    fn close(&self) -> PersistenceFuture<'_, ()> {
        Box::pin(async {
            self.memory.closed.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
}

struct Backend(Arc<Memory>);

impl SessionPersistence for Backend {
    fn create(
        &self,
        _: SessionHeader,
        _: CreateOptions,
    ) -> PersistenceFuture<'_, Box<dyn SessionHandle>> {
        Box::pin(async { Err(PersistenceError::Other("not used".into())) })
    }

    fn open<'a>(
        &'a self,
        id: &'a SessionId,
        _: SessionAccess,
    ) -> PersistenceFuture<'a, Box<dyn SessionHandle>> {
        Box::pin(async move {
            Ok(Box::new(Handle {
                id: id.clone(),
                header: SessionHeader::new(id.clone(), 1),
                memory: Arc::clone(&self.0),
            }) as Box<dyn SessionHandle>)
        })
    }

    fn stat<'a>(&'a self, _: &'a SessionId) -> PersistenceFuture<'a, Option<SessionSnapshot>> {
        Box::pin(async { Ok(None) })
    }

    fn list(&self) -> PersistenceFuture<'_, Vec<SessionSnapshot>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn flush(&self) -> PersistenceFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test]
async fn a_resume_whose_closers_fail_to_store_publishes_nothing_and_can_be_retried() {
    // A crash left turn 1 open, so resuming must store its closers.
    let memory = Arc::new(Memory {
        stored: Mutex::new(vec![Entry {
            seq: Seq(0),
            time: Time(1),
            ignorable: false,
            event: SessionEvent::turn_start(1),
        }]),
        failures: AtomicUsize::new(1),
        closed: AtomicUsize::new(0),
    });
    let store = Arc::new(SessionStore::new());
    let durable = DurableSessions::new(Arc::clone(&store), Arc::new(Backend(Arc::clone(&memory))));
    let id = SessionId::new("resumed");

    let failure = durable.open(id.clone(), None).await.unwrap_err();
    assert!(failure.to_string().contains("simulated write failure"));
    // The handle is given up, and the session never published.
    assert_eq!(memory.closed.load(Ordering::SeqCst), 1);
    assert!(store.get(&id).is_none());
    assert!(!durable.is_stored(&id));
    assert_eq!(memory.stored.lock().unwrap().len(), 1);

    // A retry resumes it whole: the closers are stored before it returns.
    let session = durable.open(id.clone(), None).await.unwrap();
    let stored = memory.stored.lock().unwrap().clone();
    assert!(stored.len() > 1);
    assert!(matches!(
        stored.last().unwrap().event,
        SessionEvent::TurnEnd(_)
    ));
    assert_eq!(session.events(), stored);
}
