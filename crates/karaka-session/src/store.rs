use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

use karaka_kernel::BoxError;
use karaka_model::Message;

use crate::derive::History;
use crate::event::{Entry, Seq, SessionEvent, Time};
use crate::header::SessionHeader;
use crate::log::{LogError, SessionLog};
use crate::request::{RequestContext, RequestHeader};

/// Names one session in a [`SessionStore`]. (dsh: `SessionId`.)
///
/// Callers choose ids; the store never makes them up. The HTTP layer will
/// use the app's own ids, so a session can be found from the app's side
/// without a mapping table.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionId(String);

impl SessionId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Why a store call failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionError {
    /// No session has this id. (dsh: `session/not-found`.)
    NotFound(SessionId),
    /// A session with this id exists already. (dsh: `SESSION_ALREADY_EXISTS`.)
    AlreadyExists(SessionId),
    /// The session's log refused an event.
    Log(LogError),
}

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(id) => write!(f, "no session has the id \"{id}\""),
            Self::AlreadyExists(id) => write!(f, "a session with the id \"{id}\" already exists"),
            Self::Log(error) => error.fmt(f),
        }
    }
}

impl Error for SessionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Log(error) => Some(error),
            _ => None,
        }
    }
}

impl From<LogError> for SessionError {
    fn from(error: LogError) -> Self {
        Self::Log(error)
    }
}

/// What a [`SessionListener`] returns from
/// [`flush`](SessionListener::flush): the wait until the session's events
/// are durable.
pub type FlushFuture = Pin<Box<dyn Future<Output = Result<(), BoxError>> + Send>>;

/// The session an event was just appended to, as a [`SessionListener`]
/// hears it: what dsh's `session/event` passes, the `Session` itself.
///
/// The log is locked meanwhile, so this is the session's log read in place,
/// not a copy.
#[derive(Clone, Copy, Debug)]
pub struct Appended<'a> {
    id: &'a SessionId,
    meta: &'a Meta,
    events: &'a [Entry],
}

impl<'a> Appended<'a> {
    pub fn id(&self) -> &'a SessionId {
        self.id
    }

    /// See [`Session::header`].
    pub fn header(&self) -> &'a SessionHeader {
        &self.meta.header
    }

    /// See [`Session::inherited_event_count`].
    pub fn inherited_event_count(&self) -> u64 {
        self.meta.inherited_event_count
    }

    /// The log as of the event: every event up to it, ending with it. In a
    /// batch, the batch's later events aren't in it yet.
    pub fn events(&self) -> &'a [Entry] {
        self.events
    }
}

/// Hears every event appended to any session of a store, and is asked to
/// make them durable. (dsh: the `session/event` and `session/flush`
/// listeners a persistence backend installs.)
///
/// A durable backend registers one with [`SessionStore::listen`], and
/// routes each event to the session's write handle by id, as dsh's backends
/// do.
pub trait SessionListener: Send + Sync {
    /// `entry` has just been appended to `session`.
    ///
    /// It runs while the session's log is locked, so listeners hear a
    /// session's events in log order. It must not block, and must not call
    /// back into the session: what it needs of the session, `session`
    /// shows. Events a session was restored with are not heard, as dsh
    /// never publishes a session's seed.
    fn event(&self, session: &Appended<'_>, entry: &Entry);

    /// Starts making the session's events so far durable, or gives `None`
    /// when this listener keeps nothing for the session. (dsh:
    /// `session/flush`.)
    fn flush(&self, id: &SessionId) -> Option<FlushFuture>;
}

/// The listeners of one store, shared with each of its sessions.
#[derive(Default)]
struct Listeners(RwLock<Vec<Arc<dyn SessionListener>>>);

impl Listeners {
    fn all(&self) -> Vec<Arc<dyn SessionListener>> {
        self.0
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Tells every listener about the entries of `log` from `from` on.
    fn publish(&self, id: &SessionId, meta: &Meta, log: &SessionLog, from: usize) {
        let events = log.events();
        if from >= events.len() {
            return;
        }
        let listeners = self.0.read().unwrap_or_else(PoisonError::into_inner);
        for listener in listeners.iter() {
            for end in from + 1..=events.len() {
                let appended = Appended {
                    id,
                    meta,
                    events: &events[..end],
                };
                listener.event(&appended, &events[end - 1]);
            }
        }
    }
}

impl fmt::Debug for Listeners {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let count = self.0.read().unwrap_or_else(PoisonError::into_inner).len();
        write!(f, "{count} listener(s)")
    }
}

/// A handle to one session in a store.
///
/// Cloning it is cheap, and every clone reaches the same log. The loop takes
/// one at the start of a turn and appends through it, so it looks the id up
/// once, not on every append.
///
/// It isn't a lock. Each call locks the log for just that call, so a turn
/// can keep the handle across an `.await`, such as a model call, while other
/// connections read the same session. Keeping two turns of one session from
/// mixing their events is the loop's job, as in dsh, where a session has one
/// active turn at a time. Events that must sit together, such as a round's
/// results, go in through [`append_all`](Self::append_all), which holds the
/// lock for the whole batch.
#[derive(Clone, Debug)]
pub struct Session {
    id: SessionId,
    meta: Arc<Meta>,
    log: Arc<Mutex<SessionLog>>,
    listeners: Arc<Listeners>,
}

/// What a session is, apart from its log, fixed when it is made.
#[derive(Debug)]
struct Meta {
    header: SessionHeader,
    inherited_event_count: u64,
}

impl Session {
    fn new(meta: Meta, log: SessionLog, listeners: Arc<Listeners>) -> Self {
        Self {
            id: meta.header.id.clone(),
            meta: Arc::new(meta),
            log: Arc::new(Mutex::new(log)),
            listeners,
        }
    }

    pub fn id(&self) -> &SessionId {
        &self.id
    }

    /// The session's metadata, fixed when it was made. (dsh:
    /// `Session.header`.)
    pub fn header(&self) -> &SessionHeader {
        &self.meta.header
    }

    /// How many events at the start of the log a fork inherited: the exact
    /// cut, never worked out from a `session/end-seed` marker. 0 for a
    /// session that isn't seeded. (dsh: `Session.inheritedEventCount`.)
    pub fn inherited_event_count(&self) -> u64 {
        self.meta.inherited_event_count
    }

    /// See [`SessionLog::append`].
    pub fn append(&self, event: SessionEvent) -> Result<Seq, LogError> {
        self.changing(|log| log.append(event))
    }

    /// Appends every event in `events` under one lock, so no other writer's
    /// event lands between them. All or none are added. See
    /// [`SessionLog::append_all`].
    pub fn append_all(
        &self,
        events: impl IntoIterator<Item = SessionEvent>,
    ) -> Result<Vec<Seq>, LogError> {
        self.changing(|log| log.append_all(events))
    }

    /// Works out and appends the closers for a turn a crash left open,
    /// under one lock. See [`SessionLog::repair`].
    pub fn repair(&self) -> Result<Vec<Entry>, LogError> {
        self.changing(SessionLog::repair)
    }

    /// See [`SessionLog::history`].
    pub fn history(&self) -> History {
        self.lock().history()
    }

    /// See [`SessionLog::messages`].
    pub fn messages(&self) -> Vec<Message> {
        self.lock().messages()
    }

    /// A copy of every event so far. Later appends don't change it.
    pub fn events(&self) -> Vec<Entry> {
        self.lock().events().to_vec()
    }

    /// A copy of the events from `seq` on. See [`SessionLog::events_since`].
    pub fn events_since(&self, seq: Seq) -> Vec<Entry> {
        self.lock().events_since(seq).to_vec()
    }

    /// See [`SessionLog::request_header`].
    pub fn request_header(&self) -> Option<RequestHeader> {
        self.lock().request_header()
    }

    /// See [`SessionLog::request_context`].
    pub fn request_context(&self) -> Option<RequestContext> {
        self.lock().request_context()
    }

    /// See [`SessionLog::surface_generation`].
    pub fn surface_generation(&self) -> u64 {
        self.lock().surface_generation()
    }

    /// The seqs of the events the model sees, in order. See
    /// [`SessionLog::surface_nodes`].
    pub fn surface_nodes(&self) -> Vec<Seq> {
        self.lock().surface_nodes().to_vec()
    }

    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    /// Runs `change` on the locked log, then tells the store's listeners
    /// about every event it added, still under the lock.
    fn changing<T>(&self, change: impl FnOnce(&mut SessionLog) -> T) -> T {
        let mut log = self.lock();
        let before = log.len();
        let result = change(&mut log);
        self.listeners.publish(&self.id, &self.meta, &log, before);
        result
    }

    fn lock(&self) -> MutexGuard<'_, SessionLog> {
        // A lock is "poisoned" when a thread panicked while holding it. Every
        // change to a log is checked before anything is pushed, and a batch
        // is undone before its error is returned, so the log is still whole,
        // and it's safe to carry on with it.
        self.log.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Every live session, held in memory. (dsh: `SessionStore`, `ctx.sessions`.)
///
/// It's shared as a port, so many connections on many threads use it at
/// once.
///
/// # Locking
///
/// There are two levels of lock. The map from ids to sessions has one
/// `RwLock`, and each session's log has its own `Mutex`.
///
/// The map lock is held only to find a session or add one, never while a
/// log is read or changed. Finding a session clones its handle out and lets
/// go of the map before the log is locked. So two sessions never wait for
/// each other: appends to different sessions share only a brief read lock on
/// the map, and readers don't block readers. Creating a session takes the
/// write lock, also briefly.
///
/// A thread never holds both locks at once, so they can't deadlock.
///
/// The locks are `std::sync` ones, not async ones, because nothing waits
/// while holding them: no lock is held across an `.await`.
///
/// # Listeners
///
/// [`listen`](Self::listen) adds a [`SessionListener`], which hears every
/// appended event, and [`flush`](Self::flush) asks every listener to make a
/// session durable. That is how a durable backend follows the store, as
/// dsh's backends follow `session/event`. With no listener, sessions live
/// in memory only.
#[derive(Debug, Default)]
pub struct SessionStore {
    sessions: RwLock<Sessions>,
    listeners: Arc<Listeners>,
}

#[derive(Debug, Default)]
struct Sessions {
    by_id: HashMap<SessionId, Session>,
    /// The ids in the order they were created, which is the order `list`
    /// gives, as in dsh. A `HashMap` alone has no order.
    order: Vec<SessionId>,
}

impl SessionStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Starts an empty session called `id`, with a fresh header made now.
    /// (dsh: `sessions.create(id)`.)
    pub fn create(&self, id: SessionId) -> Result<Session, SessionError> {
        self.create_with(fresh_header(id))
    }

    /// Starts an empty session with `header`, which names it. (dsh:
    /// `sessions.create(id, { meta })`.)
    pub fn create_with(&self, header: SessionHeader) -> Result<Session, SessionError> {
        self.add(header, 0, SessionLog::new())
    }

    /// Adds the session `id` with the events storage gave back for it, every
    /// seq and time kept, and a fresh header made now. See
    /// [`restore_with`](Self::restore_with), which a stored session's header
    /// goes through.
    pub fn restore(
        &self,
        id: SessionId,
        entries: impl IntoIterator<Item = Entry>,
    ) -> Result<Session, SessionError> {
        self.restore_with(fresh_header(id), 0, entries)
    }

    /// Adds the session `header` names, with the events storage gave back
    /// for it, every seq and time kept, and how many of them a fork
    /// inherited. Fails if the id is taken or an event doesn't fit; either
    /// way the store is unchanged. See [`SessionLog::restore`].
    ///
    /// Nothing is repaired here. The code that resumes the session calls
    /// [`Session::repair`] before it continues the session.
    pub fn restore_with(
        &self,
        header: SessionHeader,
        inherited_event_count: u64,
        entries: impl IntoIterator<Item = Entry>,
    ) -> Result<Session, SessionError> {
        // The log is checked before the map is locked, so a long log never
        // holds up other sessions.
        let log = SessionLog::restore(entries)?;
        self.add(header, inherited_event_count, log)
    }

    fn add(
        &self,
        header: SessionHeader,
        inherited_event_count: u64,
        log: SessionLog,
    ) -> Result<Session, SessionError> {
        let mut sessions = self.write();
        if sessions.by_id.contains_key(&header.id) {
            return Err(SessionError::AlreadyExists(header.id));
        }
        let meta = Meta {
            header,
            inherited_event_count,
        };
        Ok(sessions.insert(meta, log, &self.listeners))
    }

    /// The session called `id`, started empty if there's none yet.
    ///
    /// Checking and creating happen under one write lock, so two connections
    /// opening the same new id at once get the same session.
    pub fn open_or_create(&self, id: SessionId) -> Session {
        if let Some(session) = self.get(&id) {
            return session;
        }
        let mut sessions = self.write();
        // Another thread may have created it between the read above and this
        // write lock, so look again.
        match sessions.by_id.get(&id) {
            Some(session) => session.clone(),
            None => sessions.insert(
                Meta {
                    header: fresh_header(id),
                    inherited_event_count: 0,
                },
                SessionLog::new(),
                &self.listeners,
            ),
        }
    }

    /// The session called `id`, if there is one. (dsh: `get`.)
    pub fn get(&self, id: &SessionId) -> Option<Session> {
        self.read().by_id.get(id).cloned()
    }

    /// Appends `event` to the session called `id` and returns its seq.
    pub fn append(&self, id: &SessionId, event: SessionEvent) -> Result<Seq, SessionError> {
        Ok(self.session(id)?.append(event)?)
    }

    /// Appends `events` to the session called `id` as one batch. See
    /// [`Session::append_all`].
    pub fn append_all(
        &self,
        id: &SessionId,
        events: impl IntoIterator<Item = SessionEvent>,
    ) -> Result<Vec<Seq>, SessionError> {
        Ok(self.session(id)?.append_all(events)?)
    }

    /// The system prompt and conversation for the session called `id`.
    pub fn history(&self, id: &SessionId) -> Result<History, SessionError> {
        Ok(self.session(id)?.history())
    }

    /// The model's conversation for the session called `id`.
    pub fn messages(&self, id: &SessionId) -> Result<Vec<Message>, SessionError> {
        Ok(self.session(id)?.messages())
    }

    /// A copy of the events of the session called `id`.
    pub fn events(&self, id: &SessionId) -> Result<Vec<Entry>, SessionError> {
        Ok(self.session(id)?.events())
    }

    /// A copy of the events of the session called `id`, from `seq` on.
    pub fn events_since(&self, id: &SessionId, seq: Seq) -> Result<Vec<Entry>, SessionError> {
        Ok(self.session(id)?.events_since(seq))
    }

    /// Every session's id, in the order they were created. (dsh: `list`.)
    pub fn list(&self) -> Vec<SessionId> {
        self.read().order.clone()
    }

    /// Adds `listener`, which hears every event appended to any session
    /// from now on.
    pub fn listen(&self, listener: Arc<dyn SessionListener>) {
        self.listeners
            .0
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .push(listener);
    }

    /// Waits until every listener has made the session `id` durable, and
    /// says whether any listener keeps it. (dsh: `SessionStore.flush`, the
    /// one entry point for the `session/flush` checkpoint.)
    ///
    /// Every listener is waited for; the first failure is returned after.
    /// With no listener, there is nothing to wait for, and it is `false`.
    pub async fn flush(&self, id: &SessionId) -> Result<bool, BoxError> {
        let flushes: Vec<FlushFuture> = self
            .listeners
            .all()
            .iter()
            .filter_map(|listener| listener.flush(id))
            .collect();
        let participated = !flushes.is_empty();
        let mut failure = None;
        for flush in flushes {
            if let Err(error) = flush.await {
                failure.get_or_insert(error);
            }
        }
        match failure {
            Some(error) => Err(error),
            None => Ok(participated),
        }
    }

    fn session(&self, id: &SessionId) -> Result<Session, SessionError> {
        self.get(id)
            .ok_or_else(|| SessionError::NotFound(id.clone()))
    }

    // Poisoning is ignored for the same reason as in `Session::lock`: each
    // change to the map is finished before anything that could panic.
    fn read(&self) -> RwLockReadGuard<'_, Sessions> {
        self.sessions.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self) -> RwLockWriteGuard<'_, Sessions> {
        self.sessions
            .write()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

/// The header of a session made here, rather than read from storage: made
/// now, by a person, unseeded. (dsh: the header `sessions.create` makes.)
fn fresh_header(id: SessionId) -> SessionHeader {
    SessionHeader::new(id, u64::try_from(Time::now().0).unwrap_or(0))
}

impl Sessions {
    /// Adds a new session holding `log`. The caller has checked its id is
    /// free.
    fn insert(&mut self, meta: Meta, log: SessionLog, listeners: &Arc<Listeners>) -> Session {
        let id = meta.header.id.clone();
        let session = Session::new(meta, log, Arc::clone(listeners));
        self.by_id.insert(id.clone(), session.clone());
        self.order.push(id);
        session
    }
}
