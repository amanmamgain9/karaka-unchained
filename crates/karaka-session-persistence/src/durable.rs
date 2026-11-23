//! [`DurableSessions`]: the session store kept on a [`SessionPersistence`].
//!
//! In dsh, two parts do this. The backend listens to every session's
//! `session/event`, `session/flush` and `session/disposed`, and routes each
//! to the session's one write handle by id (`JsonlBackendTracker.install`
//! in `session-persistence-jsonl/src/storage.ts`). The agent loop takes
//! that handle: it resumes a stored session by opening it for writing,
//! reading it, appending the closers of a turn a crash left open, and
//! restoring the session from what it read, or creates the session when
//! none is stored (`resumeWith` and `restoreOrCreateConfigured` in
//! `agent-loop/src/index.ts`).
//!
//! Here both are one type, generic over the backend. The routing is a
//! [`SessionListener`] on the store, and [`DurableSessions::open`] is the
//! resume-or-create. Karaka's `Agents::open` is synchronous and storage
//! isn't, so the caller opens the session here first; the loop then finds
//! it in the store, already repaired.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use karaka_kernel::{BoxError, Plugin, PluginContext, PortId};
use karaka_session::{
    Entry, FlushFuture, Session, SessionError, SessionHeader, SessionId, SessionListener,
    SessionStore, Time, interrupted_turn_closers,
};

use crate::{CreateOptions, LiveWriter, PersistenceError, SessionAccess, SessionPersistence};

/// Why [`DurableSessions::open`] failed.
#[derive(Debug)]
pub enum DurableError {
    /// Storage refused, such as a session another process is writing.
    Persistence(PersistenceError),
    /// The store refused, such as a stored log that doesn't restore.
    Session(SessionError),
    /// The session is already live in the store, but was never stored: it
    /// was started without storage, and its earlier events were never
    /// written.
    NotStored(SessionId),
}

impl fmt::Display for DurableError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Persistence(error) => error.fmt(f),
            Self::Session(error) => error.fmt(f),
            Self::NotStored(id) => write!(
                f,
                "session \"{id}\" is live in memory but was never stored, so it can't be made \
                 durable now"
            ),
        }
    }
}

impl std::error::Error for DurableError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Persistence(error) => Some(error),
            Self::Session(error) => Some(error),
            Self::NotStored(_) => None,
        }
    }
}

impl From<PersistenceError> for DurableError {
    fn from(error: PersistenceError) -> Self {
        Self::Persistence(error)
    }
}

impl From<SessionError> for DurableError {
    fn from(error: SessionError) -> Self {
        Self::Session(error)
    }
}

/// Every live session that is stored, with its writer. It hears the store's
/// events and sends each to its session's writer. (dsh: the listeners
/// `JsonlBackendTracker.install` registers.)
#[derive(Default)]
struct Router {
    writers: Mutex<HashMap<SessionId, LiveWriter>>,
}

impl Router {
    fn writers(&self) -> MutexGuard<'_, HashMap<SessionId, LiveWriter>> {
        self.writers.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn writer(&self, id: &SessionId) -> Option<LiveWriter> {
        self.writers().get(id).cloned()
    }
}

impl SessionListener for Router {
    fn event(&self, id: &SessionId, entry: &Entry) {
        if let Some(writer) = self.writers().get(id) {
            writer.enqueue(entry);
        }
    }

    fn flush(&self, id: &SessionId) -> Option<FlushFuture> {
        let writer = self.writer(id)?;
        Some(Box::pin(async move {
            writer.flush().await.map_err(BoxError::from)
        }))
    }
}

/// The session store, with every session it opens kept on a
/// [`SessionPersistence`].
///
/// Appends stay synchronous: each session's [`LiveWriter`] buffers them,
/// and writes a batch within [`BATCH_WINDOW`](crate::BATCH_WINDOW).
/// [`SessionStore::flush`] is the barrier for one session, and
/// [`flush`](Self::flush) for all. Sessions opened in the store directly,
/// not through here, stay in memory only.
pub struct DurableSessions {
    store: Arc<SessionStore>,
    persistence: Arc<dyn SessionPersistence>,
    router: Arc<Router>,
    /// One open at a time, so two callers opening one id get one session.
    opening: tokio::sync::Mutex<()>,
}

impl DurableSessions {
    /// Keeps `store`'s sessions on `persistence`, by listening to the store.
    pub fn new(store: Arc<SessionStore>, persistence: Arc<dyn SessionPersistence>) -> Self {
        let router = Arc::new(Router::default());
        store.listen(Arc::clone(&router) as Arc<dyn SessionListener>);
        Self {
            store,
            persistence,
            router,
            opening: tokio::sync::Mutex::new(()),
        }
    }

    /// The live session `id`: already open here, resumed from storage, or
    /// created when none is stored. A new session works in `cwd`.
    ///
    /// Resuming takes the session's write handle first, so no one else can
    /// write it meanwhile. It appends the closers of a turn a crash left
    /// open through the handle, then restores the session. If either fails,
    /// the handle is closed and the session not published, so a later
    /// `open` tries again. A new session is stored at its first event.
    ///
    /// # Panics
    ///
    /// If called outside a Tokio runtime.
    pub async fn open(&self, id: SessionId, cwd: Option<String>) -> Result<Session, DurableError> {
        let _one_at_a_time = self.opening.lock().await;
        if let Some(session) = self.store.get(&id) {
            if self.router.writer(&id).is_some() {
                return Ok(session);
            }
            return Err(DurableError::NotStored(id));
        }
        match self.persistence.open(&id, SessionAccess::Write).await {
            Ok(handle) => self.resume(id, handle).await,
            Err(PersistenceError::NotFound(_)) => self.create(id, cwd).await,
            Err(error) => Err(error.into()),
        }
    }

    async fn resume(
        &self,
        id: SessionId,
        handle: Box<dyn crate::SessionHandle>,
    ) -> Result<Session, DurableError> {
        let mut entries = match handle.read(0, None).await {
            Ok(entries) => entries,
            Err(error) => {
                let _ = handle.close().await;
                return Err(error.into());
            }
        };
        // Semantic repair is the reader's. As in dsh, the closers of a turn
        // a crash left open are appended through the handle before the
        // session is restored, and if that fails, the handle is given up
        // and the session never published.
        let closers = interrupted_turn_closers(&entries);
        if !closers.is_empty() {
            if let Err(error) = handle.append(closers.clone()).await {
                let _ = handle.close().await;
                return Err(error.into());
            }
            entries.extend(closers);
        }
        let writer = LiveWriter::new(handle);
        self.router.writers().insert(id.clone(), writer.clone());
        match self.store.restore(id.clone(), entries) {
            Ok(session) => Ok(session),
            Err(error) => {
                self.router.writers().remove(&id);
                let _ = writer.close().await;
                Err(error.into())
            }
        }
    }

    async fn create(&self, id: SessionId, cwd: Option<String>) -> Result<Session, DurableError> {
        let created_at = u64::try_from(Time::now().0).unwrap_or(0);
        let mut header = SessionHeader::new(id.clone(), created_at);
        header.cwd = cwd;
        let handle = self
            .persistence
            .create(header, CreateOptions::default())
            .await?;
        let writer = LiveWriter::new(handle);
        self.router.writers().insert(id.clone(), writer.clone());
        match self.store.create(id.clone()) {
            Ok(session) => Ok(session),
            Err(error) => {
                self.router.writers().remove(&id);
                let _ = writer.close().await;
                Err(error.into())
            }
        }
    }

    /// Whether the live session `id` is kept here.
    pub fn is_stored(&self, id: &SessionId) -> bool {
        self.router.writer(id).is_some()
    }

    /// Makes every stored session durable: each writer drains, and its
    /// handle flushes. Every writer is tried; the failures are reported
    /// together. (dsh: `sessionPersistence.flush`, with the live buffers
    /// drained first, as its backend does.)
    pub async fn flush(&self) -> Result<(), PersistenceError> {
        let writers: Vec<LiveWriter> = self.router.writers().values().cloned().collect();
        let mut errors = Vec::new();
        for writer in writers {
            if let Err(error) = writer.flush().await {
                errors.push(error);
            }
        }
        aggregate(errors, "session persistence flush failed")
    }

    /// Drains and closes every writer, giving up the sessions' write
    /// ownership, as a process does before it exits. (dsh: the backend's
    /// teardown, which closes every open handle.)
    ///
    /// The sessions stay in the store, but later events aren't stored.
    pub async fn close(&self) -> Result<(), PersistenceError> {
        let writers: Vec<LiveWriter> = self.router.writers().drain().map(|(_, w)| w).collect();
        let mut errors = Vec::new();
        for writer in writers {
            if let Err(error) = writer.close().await {
                errors.push(error);
            }
        }
        aggregate(errors, "session persistence close failed")
    }

    /// The backend the sessions are kept on.
    pub fn persistence(&self) -> &Arc<dyn SessionPersistence> {
        &self.persistence
    }
}

fn aggregate(mut errors: Vec<PersistenceError>, message: &str) -> Result<(), PersistenceError> {
    match errors.len() {
        0 => Ok(()),
        1 => Err(errors.remove(0)),
        _ => Err(PersistenceError::Aggregate {
            message: message.to_owned(),
            errors,
        }),
    }
}

impl fmt::Debug for DurableSessions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let stored: Vec<SessionId> = self.router.writers().keys().cloned().collect();
        f.debug_struct("DurableSessions")
            .field("stored", &stored)
            .finish_non_exhaustive()
    }
}

/// Provides [`DurableSessions`], keeping the [`SessionStore`] port's
/// sessions on the [`SessionPersistence`] port.
///
/// It requires both, so a binary mounts it with a backend plugin, such as
/// `karaka-session-jsonl`'s. Without them, sessions stay in memory.
#[derive(Debug, Default)]
pub struct DurableSessionsPlugin;

impl Plugin for DurableSessionsPlugin {
    fn name(&self) -> &str {
        "durable-sessions"
    }

    fn provides(&self) -> Vec<PortId> {
        vec![PortId::of::<DurableSessions>()]
    }

    fn requires(&self) -> Vec<PortId> {
        vec![
            PortId::of::<SessionStore>(),
            PortId::of::<dyn SessionPersistence>(),
        ]
    }

    async fn setup(&self, ctx: &mut PluginContext<'_>) -> Result<(), BoxError> {
        let durable = DurableSessions::new(
            ctx.get::<SessionStore>(),
            ctx.get::<dyn SessionPersistence>(),
        );
        ctx.provide(Arc::new(durable));
        Ok(())
    }
}
