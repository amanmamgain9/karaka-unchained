//! Karaka's application authority over real sessions, with a stored-header
//! stand-in for persistence. (Karaka: `identity/tests/authority.spec.ts`,
//! `records.spec.ts`, `plugin.spec.ts` and `helpers.ts`.)
//!
//! Karaka's spec makes sessions outside the store with `Session.create`;
//! here they are made in a second store, so the authority's store doesn't
//! hold them. Error codes are checked through `IdentityError.code`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use karaka_identity::{
    ApplicationId, ApplicationOwner, IdentityError, IdentityErrorCode, KarakaIdentity,
    KarakaIdentityPlugin, Reservation, SessionOwners, TenantId, UserId, binding_of,
    matches_binding,
};
use karaka_kernel::Runtime;
use karaka_session::{
    Appended, Entry, FlushFuture, Origin, Session, SessionHeader, SessionId, SessionListener,
    SessionStore, SessionsPlugin,
};
use karaka_session_persistence::{
    CreateOptions, PersistenceError, PersistenceFuture, SessionAccess, SessionHandle,
    SessionPersistence, SessionSnapshot,
};

use IdentityErrorCode::{Forbidden, Unavailable};

fn owner() -> ApplicationOwner {
    ApplicationOwner {
        application_id: ApplicationId::new("app"),
        tenant_id: TenantId::new("tenant"),
        user_id: UserId::new("user"),
    }
}

fn foreign() -> ApplicationOwner {
    ApplicationOwner {
        user_id: UserId::new("foreign"),
        ..owner()
    }
}

fn id(text: &str) -> SessionId {
    SessionId::new(text)
}

type Stored = Arc<Mutex<HashMap<SessionId, SessionHeader>>>;

/// Stored headers, read by `stat`; nothing else is stored.
struct Headers {
    stored: Stored,
    lists: AtomicUsize,
}

fn unsupported<'a, T: Send + 'a>() -> PersistenceFuture<'a, T> {
    Box::pin(async { Err(PersistenceError::unsupported("not in the fixture", None)) })
}

impl SessionPersistence for Headers {
    fn create(
        &self,
        _: SessionHeader,
        _: CreateOptions,
    ) -> PersistenceFuture<'_, Box<dyn SessionHandle>> {
        unsupported()
    }

    fn open<'a>(
        &'a self,
        _: &'a SessionId,
        _: SessionAccess,
    ) -> PersistenceFuture<'a, Box<dyn SessionHandle>> {
        unsupported()
    }

    fn stat<'a>(&'a self, id: &'a SessionId) -> PersistenceFuture<'a, Option<SessionSnapshot>> {
        let header = self.stored.lock().unwrap().get(id).cloned();
        Box::pin(async move {
            Ok(header.map(|header| SessionSnapshot {
                header,
                format_status: None,
                revision: "fixture".into(),
                size_bytes: None,
            }))
        })
    }

    fn list(&self) -> PersistenceFuture<'_, Vec<SessionSnapshot>> {
        self.lists.fetch_add(1, Ordering::SeqCst);
        unsupported()
    }

    fn flush(&self) -> PersistenceFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }
}

/// How a flush behaves: whether the listener keeps the session, and
/// whether its flush stores the header.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Flush {
    Absent,
    Empty,
    Stores,
}

/// The `session/flush` listener: stores the live header, as Karaka's
/// fixture does.
struct Flusher {
    store: Mutex<Option<Arc<SessionStore>>>,
    stored: Stored,
    mode: Mutex<Flush>,
}

impl SessionListener for Flusher {
    fn event(&self, _: &Appended<'_>, _: &Entry) {}

    fn flush(&self, id: &SessionId) -> Option<FlushFuture> {
        match *self.mode.lock().unwrap() {
            Flush::Absent => None,
            Flush::Empty => Some(Box::pin(async { Ok(()) })),
            Flush::Stores => {
                let store = self.store.lock().unwrap().clone()?;
                if let Some(session) = store.get(id) {
                    self.stored
                        .lock()
                        .unwrap()
                        .insert(id.clone(), session.header().clone());
                }
                Some(Box::pin(async { Ok(()) }))
            }
        }
    }
}

struct Fixture {
    store: Arc<SessionStore>,
    /// Holds sessions the authority's store doesn't.
    elsewhere: SessionStore,
    stored: Stored,
    persistence: Arc<Headers>,
    flusher: Arc<Flusher>,
    authority: KarakaIdentity,
}

fn fixture() -> Fixture {
    let store = Arc::new(SessionStore::new());
    let stored: Stored = Arc::default();
    let persistence = Arc::new(Headers {
        stored: Arc::clone(&stored),
        lists: AtomicUsize::new(0),
    });
    let flusher = Arc::new(Flusher {
        store: Mutex::new(Some(Arc::clone(&store))),
        stored: Arc::clone(&stored),
        mode: Mutex::new(Flush::Stores),
    });
    store.listen(Arc::clone(&flusher) as Arc<dyn SessionListener>);
    let authority = KarakaIdentity::new(
        Arc::clone(&store),
        Some(Arc::clone(&persistence) as Arc<dyn SessionPersistence>),
    );
    Fixture {
        store,
        elsewhere: SessionStore::new(),
        stored,
        persistence,
        flusher,
        authority,
    }
}

impl Fixture {
    /// Reserves, creates, binds and acknowledges chat `name`.
    async fn create(&self, name: &str, claimed: &ApplicationOwner) -> Session {
        assert_eq!(
            self.authority.reserve(&id(name), claimed).await.unwrap(),
            Reservation::Create
        );
        let session = self.store.create(id(name)).unwrap();
        self.authority.bind(&session, claimed).await.unwrap();
        self.authority.mark_ready(&session, claimed).await.unwrap();
        session
    }

    fn child_header(&self, name: &str, parent: &SessionId) -> SessionHeader {
        SessionHeader {
            parent_session: Some(parent.clone()),
            ..SessionHeader::new(id(name), 1_000)
        }
    }

    fn store_header(&self, header: SessionHeader) {
        self.stored
            .lock()
            .unwrap()
            .insert(header.id.clone(), header);
    }
}

fn code<T: std::fmt::Debug>(result: Result<T, IdentityError>) -> IdentityErrorCode {
    result.unwrap_err().code
}

fn message<T: std::fmt::Debug>(result: Result<T, IdentityError>) -> String {
    result.unwrap_err().message
}

// records.spec.ts

#[test]
fn a_binding_holds_only_authority_fields_and_defaults_the_depth() {
    let header = SessionHeader::new(id("root"), 7);
    let binding = binding_of(&header);
    assert_eq!((binding.id.as_str(), binding.created_at), ("root", 7));
    assert_eq!((binding.cwd, binding.parent_session), (None, None));
    assert_eq!((binding.is_seeded, binding.delegation_depth), (false, 0));
    assert_eq!((binding.origin, binding.agent_preset), (None, None));
}

#[test]
fn a_binding_takes_the_optional_fields_and_refuses_another_source() {
    let header = SessionHeader {
        cwd: Some("/workspace".into()),
        parent_session: Some(id("parent")),
        is_seeded: true,
        origin: Some(Origin::Subagent),
        delegation_depth: 1,
        agent_preset: Some("coding".into()),
        ..SessionHeader::new(id("bound"), 7)
    };
    let binding = binding_of(&header);
    assert_eq!(binding.agent_preset.as_deref(), Some("coding"));
    assert!(matches_binding(&binding, &header));
    let moved = SessionHeader {
        cwd: Some("/other".into()),
        ..header.clone()
    };
    assert!(!matches_binding(&binding, &moved));
    let other = SessionHeader {
        agent_preset: Some("other".into()),
        ..header
    };
    assert!(!matches_binding(&binding, &other));
}

// plugin.spec.ts

#[tokio::test]
async fn the_plugin_mounts_without_scanning_and_a_closed_authority_refuses() {
    let runtime = Runtime::start(vec![
        Box::new(SessionsPlugin),
        Box::new(KarakaIdentityPlugin::new()),
    ])
    .await
    .unwrap();
    let identity = runtime.get::<KarakaIdentity>();
    assert_eq!(
        identity.reserve(&id("chat"), &owner()).await.unwrap(),
        Reservation::Create
    );
    // The same authority is the owners port.
    let owners = runtime.get::<dyn SessionOwners>();
    assert_eq!(owners.owner_of_cached(&id("chat")), Ok(None));
    identity.close().await;
    assert_eq!(
        message(identity.reserve(&id("closed"), &owner()).await),
        "Karaka authority is closed"
    );
    // A new authority starts empty: another owner may reserve the chat.
    let next = KarakaIdentity::new(runtime.get::<SessionStore>(), None);
    assert_eq!(
        next.reserve(&id("chat"), &foreign()).await.unwrap(),
        Reservation::Create
    );
}

// authority.spec.ts

#[tokio::test]
async fn ownership_is_recorded_and_each_foreign_component_refused() {
    let f = fixture();
    let session = f.create("chat", &owner()).await;
    assert_eq!(
        &f.authority.authorize(session.id(), &owner()).await.unwrap(),
        session.header()
    );
    for claimed in [
        ApplicationOwner {
            application_id: ApplicationId::new("other"),
            ..owner()
        },
        ApplicationOwner {
            tenant_id: TenantId::new("other"),
            ..owner()
        },
        foreign(),
    ] {
        assert_eq!(
            code(f.authority.reserve(session.id(), &claimed).await),
            Forbidden
        );
        assert_eq!(
            code(f.authority.restore(session.id(), &claimed).await),
            Forbidden
        );
        assert_eq!(
            code(f.authority.authorize(session.id(), &claimed).await),
            Forbidden
        );
        assert_eq!(
            message(f.authority.authorize(session.id(), &claimed).await),
            "Chat is not owned by this caller"
        );
    }
}

#[tokio::test]
async fn a_restart_starts_empty_and_needs_explicit_restoration() {
    let f = fixture();
    let root = f
        .elsewhere
        .create_with(SessionHeader::new(id("cold-root"), 1))
        .unwrap();
    let child = f
        .elsewhere
        .create_with(f.child_header("cold-child", root.id()))
        .unwrap();
    f.store_header(root.header().clone());
    f.store_header(child.header().clone());
    let restarted = KarakaIdentity::new(
        Arc::clone(&f.store),
        Some(Arc::clone(&f.persistence) as Arc<dyn SessionPersistence>),
    );
    assert_eq!(restarted.owner_of_id(root.id()).await, Ok(None));
    assert_eq!(restarted.owner_of_id(child.id()).await, Ok(None));
    assert_eq!(restarted.owner_of_cached_session(&root), Ok(None));
    assert_eq!(
        code(restarted.authorize(root.id(), &owner()).await),
        Forbidden
    );
    assert_eq!(
        code(restarted.reserve(root.id(), &owner()).await),
        Forbidden
    );
    assert_eq!(
        &restarted.restore(root.id(), &owner()).await.unwrap(),
        root.header()
    );
    assert_eq!(
        restarted.reserve(root.id(), &owner()).await.unwrap(),
        Reservation::Existing
    );
    assert_eq!(restarted.owner_of_id(child.id()).await, Ok(Some(owner())));
    assert_eq!(restarted.owner_of_cached_session(&child), Ok(Some(owner())));
    // Neither was opened, and nothing was listed.
    assert!(f.store.get(root.id()).is_none());
    assert!(f.store.get(child.id()).is_none());
    assert_eq!(f.persistence.lists.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn missing_unpublished_and_child_sessions_are_not_restored() {
    let f = fixture();
    let live = f.store.create(id("unpublished")).unwrap();
    assert_eq!(
        code(f.authority.restore(live.id(), &owner()).await),
        Forbidden
    );
    assert_eq!(
        message(f.authority.restore(&id("missing"), &owner()).await),
        "Chat is unavailable for restoration"
    );
    assert_eq!(
        code(f.authority.authorize(&id("missing"), &owner()).await),
        Forbidden
    );
    assert_eq!(
        code(f.authority.authorize_session(&live, &owner()).await),
        Forbidden
    );
    assert_eq!(f.authority.owner_of_id(&id("missing")).await, Ok(None));
    let parent = f.create("parent", &owner()).await;
    let child = f
        .store
        .create_with(f.child_header("child", parent.id()))
        .unwrap();
    f.store_header(child.header().clone());
    assert_eq!(
        message(f.authority.restore(child.id(), &owner()).await),
        "Only application root chats can restore ownership"
    );
    assert_eq!(f.authority.owner_of_session(&live).await, Ok(None));
    assert_eq!(
        f.authority.owner_of_session(&child).await,
        Ok(Some(owner()))
    );
}

#[tokio::test]
async fn a_child_inherits_its_owner_and_a_conflicting_reservation_is_refused() {
    let f = fixture();
    let orphan = f
        .store
        .create_with(f.child_header("orphan", &id("missing")))
        .unwrap();
    assert_eq!(f.authority.owner_of_session(&orphan).await, Ok(None));
    assert_eq!(f.authority.owner_of_cached_session(&orphan), Ok(None));
    let parent = f.create("parent", &owner()).await;
    let child = f
        .store
        .create_with(f.child_header("child", parent.id()))
        .unwrap();
    assert_eq!(
        f.authority.owner_of_session(&child).await,
        Ok(Some(owner()))
    );
    assert_eq!(
        f.authority.owner_of_cached_session(&child),
        Ok(Some(owner()))
    );
    // The owners port reads the same.
    assert_eq!(f.authority.owner_of(child.id()).await, Ok(Some(owner())));
    assert_eq!(
        SessionOwners::owner_of_cached(&f.authority, child.id()),
        Ok(Some(owner()))
    );
    f.authority.mark_ready(&child, &owner()).await.unwrap();
    assert_eq!(
        code(f.authority.authorize_session(&child, &foreign()).await),
        Forbidden
    );
    f.authority
        .reserve(&id("foreign-child"), &foreign())
        .await
        .unwrap();
    let conflicting = f
        .store
        .create_with(f.child_header("foreign-child", parent.id()))
        .unwrap();
    assert_eq!(
        code(f.authority.bind(&conflicting, &foreign()).await),
        Forbidden
    );
}

#[tokio::test]
async fn an_explicitly_reserved_child_checks_its_own_and_its_roots_owner() {
    let f = fixture();
    let parent = f.create("parent", &owner()).await;
    f.authority
        .reserve(&id("reserved-child"), &owner())
        .await
        .unwrap();
    let child = f
        .store
        .create_with(f.child_header("reserved-child", parent.id()))
        .unwrap();
    f.authority.bind(&child, &owner()).await.unwrap();
    assert_eq!(
        f.authority.owner_of_session(&child).await,
        Ok(Some(owner()))
    );
    assert_eq!(
        f.authority.owner_of_cached_session(&child),
        Ok(Some(owner()))
    );
}

#[tokio::test]
async fn changed_stored_or_live_fields_are_refused_before_restoring() {
    let f = fixture();
    let root = f.create("root", &owner()).await;
    for changed in [
        SessionHeader {
            created_at: root.header().created_at + 1,
            ..root.header().clone()
        },
        SessionHeader {
            agent_preset: Some("other".into()),
            ..root.header().clone()
        },
    ] {
        f.store_header(changed);
        assert_eq!(
            code(f.authority.restore(root.id(), &owner()).await),
            Unavailable
        );
    }
    f.store_header(root.header().clone());
    assert_eq!(
        &f.authority.restore(root.id(), &owner()).await.unwrap(),
        root.header()
    );
    let replaced = f
        .elsewhere
        .create_with(SessionHeader {
            created_at: root.header().created_at + 1,
            ..root.header().clone()
        })
        .unwrap();
    assert_eq!(
        code(f.authority.owner_of_session(&replaced).await),
        Unavailable
    );
    assert_eq!(
        message(f.authority.owner_of_cached_session(&replaced)),
        "Chat source does not match its authority"
    );
}

#[tokio::test]
async fn a_stored_header_disagreeing_with_an_unowned_live_session_is_refused() {
    let f = fixture();
    let live = f.store.create(id("different-live")).unwrap();
    f.store_header(SessionHeader {
        created_at: live.header().created_at + 1,
        ..live.header().clone()
    });
    assert_eq!(
        code(f.authority.restore(live.id(), &owner()).await),
        Unavailable
    );
    assert_eq!(f.authority.owner_of_id(live.id()).await, Ok(None));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_chat_is_serialized_through_failures_while_another_progresses() {
    let f = Arc::new(fixture());
    let order: Arc<Mutex<Vec<&str>>> = Arc::default();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel::<()>();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
    let first = {
        let (f, order) = (Arc::clone(&f), Arc::clone(&order));
        tokio::spawn(async move {
            f.authority
                .with_chat_lock(&id("chat"), async {
                    order.lock().unwrap().push("first");
                    let _ = entered_tx.send(());
                    let _ = release_rx.await;
                    Err::<(), &str>("create failed")
                })
                .await
        })
    };
    entered_rx.await.unwrap();
    let second = {
        let (f, order) = (Arc::clone(&f), Arc::clone(&order));
        tokio::spawn(async move {
            f.authority
                .with_chat_lock(&id("chat"), async {
                    order.lock().unwrap().push("second");
                })
                .await
        })
    };
    tokio::task::yield_now().await;
    f.authority
        .with_chat_lock(&id("other"), async {
            order.lock().unwrap().push("other");
        })
        .await
        .unwrap();
    assert_eq!(*order.lock().unwrap(), ["first", "other"]);
    let _ = release_tx.send(());
    assert_eq!(first.await.unwrap(), Ok(Err("create failed")));
    second.await.unwrap().unwrap();
    assert_eq!(*order.lock().unwrap(), ["first", "other", "second"]);
}

#[tokio::test]
async fn reservations_fail_closed_and_a_failed_unpublished_creation_may_retry() {
    let f = fixture();
    let ready = f.create("ready", &owner()).await;
    assert_eq!(
        f.authority.reserve(ready.id(), &owner()).await.unwrap(),
        Reservation::Existing
    );
    f.authority.bind(&ready, &owner()).await.unwrap();
    f.authority.mark_ready(&ready, &owner()).await.unwrap();
    let reserved = id("reserved");
    assert_eq!(
        f.authority.reserve(&reserved, &owner()).await.unwrap(),
        Reservation::Create
    );
    assert_eq!(
        f.authority.reserve(&reserved, &owner()).await.unwrap(),
        Reservation::Create
    );
    let session = f.store.create(reserved.clone()).unwrap();
    assert_eq!(f.authority.owner_of_cached_session(&session), Ok(None));
    assert_eq!(
        code(f.authority.authorize_session(&session, &owner()).await),
        Forbidden
    );
    assert_eq!(
        message(f.authority.reserve(&reserved, &owner()).await),
        "Chat has no bound authority"
    );
    f.store_header(session.header().clone());
    assert_eq!(
        code(f.authority.restore(&reserved, &owner()).await),
        Unavailable
    );
    let attempted = f
        .elsewhere
        .create_with(SessionHeader::new(id("unmaterialized"), 5))
        .unwrap();
    f.authority.reserve(attempted.id(), &owner()).await.unwrap();
    f.authority.bind(&attempted, &owner()).await.unwrap();
    assert_eq!(
        f.authority.reserve(attempted.id(), &owner()).await.unwrap(),
        Reservation::Create
    );
    let retry = SessionStore::new()
        .create_with(SessionHeader::new(id("unmaterialized"), 6))
        .unwrap();
    f.authority.bind(&retry, &owner()).await.unwrap();
    assert_eq!(
        f.authority.owner_of_session(&retry).await,
        Ok(Some(owner()))
    );
}

#[tokio::test]
async fn missing_acknowledged_data_and_bad_bindings_are_refused() {
    let f = fixture();
    let ready = f
        .elsewhere
        .create_with(SessionHeader::new(id("ready"), 1))
        .unwrap();
    f.store_header(ready.header().clone());
    f.authority.restore(ready.id(), &owner()).await.unwrap();
    f.stored.lock().unwrap().remove(ready.id());
    assert_eq!(
        message(f.authority.reserve(ready.id(), &owner()).await),
        "Acknowledged chat data is missing"
    );
    let unreserved = f.store.create(id("unreserved")).unwrap();
    assert_eq!(
        message(f.authority.bind(&unreserved, &owner()).await),
        "Chat has no application authority"
    );
    f.authority
        .reserve(&id("missing-parent-child"), &owner())
        .await
        .unwrap();
    let missing = f
        .store
        .create_with(f.child_header("missing-parent-child", &id("absent")))
        .unwrap();
    assert_eq!(
        message(f.authority.bind(&missing, &owner()).await),
        "Chat parent is unavailable"
    );
    let parent = f.store.create(id("unowned-parent")).unwrap();
    f.authority
        .reserve(&id("unowned-parent-child"), &owner())
        .await
        .unwrap();
    let child = f
        .store
        .create_with(f.child_header("unowned-parent-child", parent.id()))
        .unwrap();
    assert_eq!(
        message(f.authority.bind(&child, &owner()).await),
        "Chat parent has no application owner"
    );
}

#[tokio::test]
async fn cyclic_lineage_is_refused_by_both_lookups() {
    let f = fixture();
    let cyclic = f
        .elsewhere
        .create_with(f.child_header("cyclic", &id("cyclic")))
        .unwrap();
    f.store_header(cyclic.header().clone());
    assert_eq!(
        message(f.authority.owner_of_session(&cyclic).await),
        "Cyclic chat lineage"
    );
    assert_eq!(
        message(f.authority.owner_of_cached_session(&cyclic)),
        "Cyclic chat lineage"
    );
}

#[tokio::test]
async fn failed_flushes_absent_data_and_mismatched_headers_are_not_acknowledged() {
    let f = fixture();
    let pending = id("pending");
    f.authority.reserve(&pending, &owner()).await.unwrap();
    let session = f.store.create(pending.clone()).unwrap();
    f.authority.bind(&session, &owner()).await.unwrap();
    *f.flusher.mode.lock().unwrap() = Flush::Absent;
    assert_eq!(
        message(f.authority.mark_ready(&session, &owner()).await),
        "Chat has no durability provider"
    );
    *f.flusher.mode.lock().unwrap() = Flush::Empty;
    assert_eq!(
        message(f.authority.mark_ready(&session, &owner()).await),
        "Flushed chat data is unavailable"
    );
    f.store_header(SessionHeader {
        created_at: session.header().created_at + 1,
        ..session.header().clone()
    });
    assert_eq!(
        message(f.authority.mark_ready(&session, &owner()).await),
        "Chat source does not match its authority"
    );
    // Without a live or stored session, the unacknowledged chat may be
    // made again. (Karaka detaches the session; one in another store
    // stands in.)
    f.stored.lock().unwrap().remove(&pending);
    let fresh = fixture();
    fresh.authority.reserve(&pending, &owner()).await.unwrap();
    let detached = fresh
        .elsewhere
        .create_with(SessionHeader::new(pending.clone(), 3))
        .unwrap();
    fresh.authority.bind(&detached, &owner()).await.unwrap();
    assert_eq!(
        fresh.authority.reserve(&pending, &owner()).await.unwrap(),
        Reservation::Create
    );
    *fresh.flusher.store.lock().unwrap() = None;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn close_waits_for_admitted_operations_and_refuses_later_calls() {
    let f = Arc::new(fixture());
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel::<()>();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
    let pending = {
        let f = Arc::clone(&f);
        tokio::spawn(async move {
            f.authority
                .with_chat_lock(&id("pending"), async {
                    let _ = entered_tx.send(());
                    let _ = release_rx.await;
                })
                .await
        })
    };
    entered_rx.await.unwrap();
    let closing = {
        let f = Arc::clone(&f);
        tokio::spawn(async move { f.authority.close().await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    assert_eq!(
        code(f.authority.reserve(&id("other"), &owner()).await),
        Unavailable
    );
    let closed = f.elsewhere.create(id("closed")).unwrap();
    assert_eq!(
        message(f.authority.owner_of_cached_session(&closed)),
        "Karaka authority is closed"
    );
    assert!(!closing.is_finished());
    let _ = release_tx.send(());
    pending.await.unwrap().unwrap();
    closing.await.unwrap();
    assert_eq!(
        code(f.authority.with_chat_lock(&id("closed"), async {}).await),
        Unavailable
    );
}
