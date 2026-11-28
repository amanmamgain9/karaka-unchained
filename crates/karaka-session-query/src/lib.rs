//! One session's events, from the live store or, cold, from storage,
//! without opening or repairing it. (dsh: `@deepseek-ai/dsh-session-query`,
//! `observeSession` with `projectionMode: 'none'`, `observation.ts`.)
//!
//! Only that is ported. dsh's search, its corpus and cursors, projections,
//! leases on cold preparations, and the SQLite index
//! (`session-query-sqlite`) are still to port.

use std::fmt;
use std::sync::Arc;

use karaka_kernel::{BoxError, Plugin, PluginContext, PortId};
use karaka_session::{Entry, SessionHeader, SessionId, SessionStore};
use karaka_session_persistence::{PersistenceError, SessionAccess, SessionPersistence};

/// What one observation saw. (dsh: `SessionObservation`.)
#[derive(Clone, Debug, PartialEq)]
pub struct ObservedSession {
    pub header: SessionHeader,
    pub events: Vec<Entry>,
    /// Read from the live store, rather than from storage.
    pub live: bool,
}

/// Why a session can't be observed. (dsh: `SessionQueryError`.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionQueryError {
    pub message: String,
    pub code: &'static str,
}

impl fmt::Display for SessionQueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for SessionQueryError {}

/// The port. (dsh: the `sessionQuery` service.)
pub struct SessionQuery {
    store: Arc<SessionStore>,
    persistence: Option<Arc<dyn SessionPersistence>>,
}

impl SessionQuery {
    pub fn new(store: Arc<SessionStore>, persistence: Option<Arc<dyn SessionPersistence>>) -> Self {
        Self { store, persistence }
    }

    /// The session `id`, live first, else stored. (dsh: `observeSession`,
    /// live-preferred.)
    ///
    /// # Errors
    ///
    /// In dsh's words: `session "x" not found`
    /// (`SESSION_QUERY_SESSION_NOT_FOUND`), or `failed to observe session
    /// "x": …` (`SESSION_QUERY_PERSISTENCE_FAILED`).
    pub async fn observe_session(
        &self,
        id: &SessionId,
    ) -> Result<ObservedSession, SessionQueryError> {
        if let Some(session) = self.store.get(id) {
            return Ok(ObservedSession {
                header: session.header().clone(),
                events: session.events(),
                live: true,
            });
        }
        let not_found = || SessionQueryError {
            message: format!("session \"{id}\" not found"),
            code: "SESSION_QUERY_SESSION_NOT_FOUND",
        };
        let failed = |error: PersistenceError| SessionQueryError {
            message: format!("failed to observe session \"{id}\": {error}"),
            code: "SESSION_QUERY_PERSISTENCE_FAILED",
        };
        let Some(persistence) = &self.persistence else {
            return Err(not_found());
        };
        let handle = match persistence.open(id, SessionAccess::Read).await {
            Ok(handle) => handle,
            Err(PersistenceError::NotFound(_)) => {
                // Attached meanwhile: the live store is the fresher source.
                return match self.store.get(id) {
                    Some(session) => Ok(ObservedSession {
                        header: session.header().clone(),
                        events: session.events(),
                        live: true,
                    }),
                    None => Err(not_found()),
                };
            }
            Err(error) => return Err(failed(error)),
        };
        let header = handle.header().clone();
        let read = handle.read(0, None).await;
        let _ = handle.close().await;
        Ok(ObservedSession {
            header,
            events: read.map_err(failed)?,
            live: false,
        })
    }
}

impl fmt::Debug for SessionQuery {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionQuery")
            .field("persistence", &self.persistence.is_some())
            .finish_non_exhaustive()
    }
}

/// Provides [`SessionQuery`] over the store and, with
/// [`with_persistence`](Self::with_persistence), storage.
#[derive(Debug, Default)]
pub struct SessionQueryPlugin {
    persistence: bool,
}

impl SessionQueryPlugin {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_persistence() -> Self {
        Self { persistence: true }
    }
}

impl Plugin for SessionQueryPlugin {
    fn name(&self) -> &str {
        "session-query"
    }

    fn provides(&self) -> Vec<PortId> {
        vec![PortId::of::<SessionQuery>()]
    }

    fn requires(&self) -> Vec<PortId> {
        let mut ports = vec![PortId::of::<SessionStore>()];
        if self.persistence {
            ports.push(PortId::of::<dyn SessionPersistence>());
        }
        ports
    }

    async fn setup(&self, ctx: &mut PluginContext<'_>) -> Result<(), BoxError> {
        let persistence = self
            .persistence
            .then(|| ctx.get::<dyn SessionPersistence>());
        ctx.provide(Arc::new(SessionQuery::new(
            ctx.get::<SessionStore>(),
            persistence,
        )));
        Ok(())
    }
}
