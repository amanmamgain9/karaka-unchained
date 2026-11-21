use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

use karaka_model::Message;

use crate::event::{Entry, Seq, SessionEvent};
use crate::log::SessionLog;

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
}

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(id) => write!(f, "no session has the id \"{id}\""),
            Self::AlreadyExists(id) => write!(f, "a session with the id \"{id}\" already exists"),
        }
    }
}

impl Error for SessionError {}

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
/// active turn at a time.
#[derive(Clone, Debug)]
pub struct Session {
    id: SessionId,
    log: Arc<Mutex<SessionLog>>,
}

impl Session {
    fn new(id: SessionId) -> Self {
        Self {
            id,
            log: Arc::default(),
        }
    }

    pub fn id(&self) -> &SessionId {
        &self.id
    }

    /// See [`SessionLog::append`].
    pub fn append(&self, event: SessionEvent) -> Seq {
        self.lock().append(event)
    }

    /// See [`SessionLog::messages`].
    pub fn messages(&self) -> Vec<Message> {
        self.lock().messages()
    }

    /// A copy of every event so far. Later appends don't change it.
    pub fn events(&self) -> Vec<Entry> {
        self.lock().events().to_vec()
    }

    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    fn lock(&self) -> MutexGuard<'_, SessionLog> {
        // A lock is "poisoned" when a thread panicked while holding it. Every
        // change to a log is one push that either happened or didn't, so the
        // log is still whole, and it's safe to carry on with it.
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
#[derive(Debug, Default)]
pub struct SessionStore {
    sessions: RwLock<Sessions>,
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

    /// Starts an empty session called `id`.
    pub fn create(&self, id: SessionId) -> Result<Session, SessionError> {
        let mut sessions = self.write();
        if sessions.by_id.contains_key(&id) {
            return Err(SessionError::AlreadyExists(id));
        }
        Ok(sessions.insert(id))
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
            None => sessions.insert(id),
        }
    }

    /// The session called `id`, if there is one. (dsh: `get`.)
    pub fn get(&self, id: &SessionId) -> Option<Session> {
        self.read().by_id.get(id).cloned()
    }

    /// Appends `event` to the session called `id` and returns its seq.
    pub fn append(&self, id: &SessionId, event: SessionEvent) -> Result<Seq, SessionError> {
        Ok(self.session(id)?.append(event))
    }

    /// The model's history for the session called `id`.
    pub fn messages(&self, id: &SessionId) -> Result<Vec<Message>, SessionError> {
        Ok(self.session(id)?.messages())
    }

    /// A copy of the events of the session called `id`.
    pub fn events(&self, id: &SessionId) -> Result<Vec<Entry>, SessionError> {
        Ok(self.session(id)?.events())
    }

    /// Every session's id, in the order they were created. (dsh: `list`.)
    pub fn list(&self) -> Vec<SessionId> {
        self.read().order.clone()
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

impl Sessions {
    /// Adds a new, empty session. The caller has checked `id` is free.
    fn insert(&mut self, id: SessionId) -> Session {
        let session = Session::new(id.clone());
        self.by_id.insert(id.clone(), session.clone());
        self.order.push(id);
        session
    }
}
