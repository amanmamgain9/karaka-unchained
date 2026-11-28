//! Karaka's application authority: which application owns each chat, held
//! in this process over unchanged sessions. (Karaka: `KarakaIdentity` in
//! `identity/src/index.ts`.)
//!
//! Nothing about an owner is stored. A chat is reserved, bound to the
//! session made for it and acknowledged once durable; after a restart the
//! ingress restores each root's owner explicitly, once the application has
//! verified access. A child session inherits its owner through its
//! `parentSession` chain. Lookups observe a session, live or stored, and
//! never open or repair one.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use karaka_session::{Session, SessionHeader, SessionId, SessionStore};
use karaka_session_persistence::SessionPersistence;

use crate::records::{IdentityRecord, SessionBinding, binding_of, matches_binding};
use crate::{
    ApplicationOwner, IdentityError, IdentityErrorCode, OwnerFuture, SessionOwners, same_owner,
};

use IdentityErrorCode::{Forbidden, Unavailable};

/// What a reservation found. (Karaka: `reserve`'s `'create' | 'existing'`.)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reservation {
    /// The controller makes the session.
    Create,
    /// The controller uses the bound session that exists.
    Existing,
}

#[derive(Default)]
struct State {
    chats: HashMap<SessionId, IdentityRecord>,
    /// Headers seen, live or stored, for the cached lineage walk.
    headers: HashMap<SessionId, SessionHeader>,
    closed: bool,
}

/// The authority. (Karaka: `KarakaIdentity`.)
pub struct KarakaIdentity {
    store: Arc<SessionStore>,
    /// `None` without durable storage: only live sessions are observed.
    persistence: Option<Arc<dyn SessionPersistence>>,
    state: Mutex<State>,
    /// One lock per chat with an operation admitted, FIFO.
    locks: Mutex<HashMap<SessionId, Arc<tokio::sync::Mutex<()>>>>,
}

fn error(code: IdentityErrorCode, message: &str) -> IdentityError {
    IdentityError::new(code, message)
}

fn not_owned() -> IdentityError {
    error(Forbidden, "Chat is not owned by this caller")
}

fn check_owner(actual: &ApplicationOwner, claimed: &ApplicationOwner) -> Result<(), IdentityError> {
    if same_owner(actual, claimed) {
        Ok(())
    } else {
        Err(not_owned())
    }
}

fn check_binding(binding: &SessionBinding, header: &SessionHeader) -> Result<(), IdentityError> {
    if matches_binding(binding, header) {
        Ok(())
    } else {
        Err(error(
            Unavailable,
            "Chat source does not match its authority",
        ))
    }
}

impl KarakaIdentity {
    pub fn new(store: Arc<SessionStore>, persistence: Option<Arc<dyn SessionPersistence>>) -> Self {
        Self {
            store,
            persistence,
            state: Mutex::default(),
            locks: Mutex::default(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn assert_open(&self) -> Result<(), IdentityError> {
        if self.lock().closed {
            return Err(error(Unavailable, "Karaka authority is closed"));
        }
        Ok(())
    }

    /// Runs `operation` once every operation admitted before it on chat
    /// `id` has settled; other chats run meanwhile. (Karaka:
    /// `withChatLock`.)
    ///
    /// # Errors
    ///
    /// The authority is closed; `operation` isn't run.
    pub async fn with_chat_lock<T>(
        &self,
        id: &SessionId,
        operation: impl Future<Output = T>,
    ) -> Result<T, IdentityError> {
        self.assert_open()?;
        let lock = {
            let mut locks = self.locks.lock().unwrap_or_else(PoisonError::into_inner);
            Arc::clone(locks.entry(id.clone()).or_default())
        };
        let result = {
            let _turn = lock.lock().await;
            operation.await
        };
        let mut locks = self.locks.lock().unwrap_or_else(PoisonError::into_inner);
        // The map's reference and this one: nobody else waits.
        if Arc::strong_count(&lock) == 2 {
            locks.remove(id);
        }
        Ok(result)
    }

    /// The header of session `id`, live or stored, or `None`; the cache
    /// follows. (Karaka: `observe`.)
    async fn observe(&self, id: &SessionId) -> Result<Option<SessionHeader>, IdentityError> {
        let header = match self.store.get(id) {
            Some(session) => Some(session.header().clone()),
            None => match &self.persistence {
                Some(persistence) => persistence
                    .stat(id)
                    .await
                    .map_err(|failure| error(Unavailable, &failure.to_string()))?
                    .map(|snapshot| snapshot.header),
                None => None,
            },
        };
        let mut state = self.lock();
        match &header {
            Some(header) => {
                state.headers.insert(id.clone(), header.clone());
            }
            None => {
                state.headers.remove(id);
            }
        }
        Ok(header)
    }

    /// The stored header of `id`, as `sessionPersistence.stat` gives it.
    async fn stat(&self, id: &SessionId) -> Result<Option<SessionHeader>, IdentityError> {
        match &self.persistence {
            Some(persistence) => Ok(persistence
                .stat(id)
                .await
                .map_err(|failure| error(Unavailable, &failure.to_string()))?
                .map(|snapshot| snapshot.header)),
            None => Ok(None),
        }
    }

    /// The record of `id`, refusing a reservation not yet bound with
    /// `code`. (Karaka: `boundRecord`.)
    fn bound_record(
        &self,
        id: &SessionId,
        code: IdentityErrorCode,
    ) -> Result<Option<IdentityRecord>, IdentityError> {
        match self.lock().chats.get(id) {
            Some(IdentityRecord::Reserved { .. }) => {
                Err(error(code, "Chat has no bound authority"))
            }
            other => Ok(other.cloned()),
        }
    }

    /// Reserves a new chat, or admits its owner to an existing one. Call it
    /// inside [`with_chat_lock`](Self::with_chat_lock). (Karaka: `reserve`.)
    ///
    /// # Errors
    ///
    /// In Karaka's words: another owner; a session that exists with no
    /// record; one reserved but never bound; acknowledged data gone; or a
    /// session that isn't the one bound.
    pub async fn reserve(
        &self,
        id: &SessionId,
        owner: &ApplicationOwner,
    ) -> Result<Reservation, IdentityError> {
        self.assert_open()?;
        let record = self.lock().chats.get(id).cloned();
        if let Some(record) = &record {
            check_owner(record.owner(), owner)?;
        }
        let header = self.observe(id).await?;
        let Some(record) = record else {
            if header.is_some() {
                return Err(not_owned());
            }
            self.lock().chats.insert(
                id.clone(),
                IdentityRecord::Reserved {
                    owner: owner.clone(),
                },
            );
            return Ok(Reservation::Create);
        };
        if let Some(header) = header {
            let Some(binding) = record.binding() else {
                return Err(error(Unavailable, "Chat has no bound authority"));
            };
            check_binding(binding, &header)?;
            return Ok(Reservation::Existing);
        }
        match record {
            IdentityRecord::Ready { .. } => {
                Err(error(Unavailable, "Acknowledged chat data is missing"))
            }
            // A failed, unpublished attempt never materialized; its owner
            // may retry with the session the next attempt makes.
            IdentityRecord::Bound { owner, .. } => {
                self.lock()
                    .chats
                    .insert(id.clone(), IdentityRecord::Reserved { owner });
                Ok(Reservation::Create)
            }
            IdentityRecord::Reserved { .. } => Ok(Reservation::Create),
        }
    }

    /// Binds the session made for a reservation, before it is published:
    /// a child's parent must exist and have this owner. (Karaka: `bind`.)
    ///
    /// # Errors
    ///
    /// In Karaka's words: no reservation; another owner; a parent missing
    /// or unowned; or, for a record already bound, another session.
    pub async fn bind(
        &self,
        session: &Session,
        owner: &ApplicationOwner,
    ) -> Result<(), IdentityError> {
        self.assert_open()?;
        let id = session.id();
        let header = session.header();
        let record = self
            .lock()
            .chats
            .get(id)
            .cloned()
            .ok_or_else(|| error(Forbidden, "Chat has no application authority"))?;
        check_owner(record.owner(), owner)?;
        if let Some(binding) = record.binding() {
            check_binding(binding, header)?;
        } else {
            if let Some(parent) = &header.parent_session {
                let parent = self
                    .observe(parent)
                    .await?
                    .ok_or_else(|| error(Forbidden, "Chat parent is unavailable"))?;
                let inherited = self
                    .resolve_owner(&parent)
                    .await?
                    .ok_or_else(|| error(Forbidden, "Chat parent has no application owner"))?;
                check_owner(&inherited, owner)?;
            }
            self.lock().chats.insert(
                id.clone(),
                IdentityRecord::Bound {
                    owner: record.owner().clone(),
                    binding: binding_of(header),
                },
            );
        }
        self.lock().headers.insert(id.clone(), header.clone());
        Ok(())
    }

    /// Acknowledges a chat once its session is flushed and the stored
    /// header is the bound one. A child inheriting its owner gets no record
    /// of its own. (Karaka: `markReady`.)
    ///
    /// # Errors
    ///
    /// In Karaka's words: not this owner's; no durable storage; nothing
    /// stored after the flush; or another session stored. A failed flush
    /// or stat is `unavailable` with its own message.
    pub async fn mark_ready(
        &self,
        session: &Session,
        owner: &ApplicationOwner,
    ) -> Result<(), IdentityError> {
        self.authorize_session(session, owner).await?;
        let id = session.id();
        let record = self.bound_record(id, Unavailable)?;
        let flushed = self
            .store
            .flush(id)
            .await
            .map_err(|failure| error(Unavailable, &failure.to_string()))?;
        if !flushed {
            return Err(error(Unavailable, "Chat has no durability provider"));
        }
        let stored = self
            .stat(id)
            .await?
            .ok_or_else(|| error(Unavailable, "Flushed chat data is unavailable"))?;
        let binding = record
            .as_ref()
            .and_then(IdentityRecord::binding)
            .cloned()
            .unwrap_or_else(|| binding_of(session.header()));
        check_binding(&binding, &stored)?;
        if let Some(IdentityRecord::Bound { owner, binding }) = record {
            self.lock()
                .chats
                .insert(id.clone(), IdentityRecord::Ready { owner, binding });
        }
        Ok(())
    }

    /// Restores a stored root's owner, after the ingress has verified the
    /// application's access; neither opens nor repairs it. Call it inside
    /// [`with_chat_lock`](Self::with_chat_lock). (Karaka: `restore`.)
    ///
    /// # Errors
    ///
    /// In Karaka's words: another owner; nothing stored; a child session;
    /// a reservation never bound; or a stored or live session that isn't
    /// the bound one.
    pub async fn restore(
        &self,
        id: &SessionId,
        owner: &ApplicationOwner,
    ) -> Result<SessionHeader, IdentityError> {
        self.assert_open()?;
        let record = self.lock().chats.get(id).cloned();
        if let Some(record) = &record {
            check_owner(record.owner(), owner)?;
        }
        let header = self
            .stat(id)
            .await?
            .ok_or_else(|| error(Forbidden, "Chat is unavailable for restoration"))?;
        if header.parent_session.is_some() {
            return Err(error(
                Forbidden,
                "Only application root chats can restore ownership",
            ));
        }
        let (owner, binding) = match record {
            Some(IdentityRecord::Reserved { .. }) => {
                return Err(error(Unavailable, "Chat has no bound authority"));
            }
            Some(
                IdentityRecord::Bound { owner, binding } | IdentityRecord::Ready { owner, binding },
            ) => (owner, binding),
            None => (owner.clone(), binding_of(&header)),
        };
        check_binding(&binding, &header)?;
        if let Some(live) = self.store.get(id) {
            check_binding(&binding, live.header())?;
        }
        let mut state = self.lock();
        state
            .chats
            .insert(id.clone(), IdentityRecord::Ready { owner, binding });
        state.headers.insert(id.clone(), header.clone());
        Ok(header)
    }

    /// Authorizes `owner` for chat `id`, observed and not opened. (Karaka:
    /// `authorize`.)
    ///
    /// # Errors
    ///
    /// `Chat is not owned by this caller` for a missing chat, an unowned
    /// one, or another owner's.
    pub async fn authorize(
        &self,
        id: &SessionId,
        owner: &ApplicationOwner,
    ) -> Result<SessionHeader, IdentityError> {
        self.assert_open()?;
        let header = self.observe(id).await?.ok_or_else(not_owned)?;
        let actual = self.resolve_owner(&header).await?.ok_or_else(not_owned)?;
        check_owner(&actual, owner)?;
        Ok(header)
    }

    /// Authorizes `owner` for this exact session. (Karaka:
    /// `authorizeSession`.)
    ///
    /// # Errors
    ///
    /// As [`authorize`](Self::authorize), and a session that isn't the
    /// bound one.
    pub async fn authorize_session(
        &self,
        session: &Session,
        owner: &ApplicationOwner,
    ) -> Result<(), IdentityError> {
        let actual = self
            .owner_of_session(session)
            .await?
            .ok_or_else(not_owned)?;
        check_owner(&actual, owner)
    }

    /// The owner of this session, through its lineage: what a tool call is
    /// authorized against. (Karaka: `ownerOf`.)
    ///
    /// # Errors
    ///
    /// The lineage is cyclic, a record unbound, or a session not the bound
    /// one.
    pub async fn owner_of_session(
        &self,
        session: &Session,
    ) -> Result<Option<ApplicationOwner>, IdentityError> {
        self.assert_open()?;
        self.resolve_owner(session.header()).await
    }

    /// The owner of chat `id`, live or stored, without opening it.
    /// (Karaka: `ownerOfId`.)
    ///
    /// # Errors
    ///
    /// As [`owner_of_session`](Self::owner_of_session).
    pub async fn owner_of_id(
        &self,
        id: &SessionId,
    ) -> Result<Option<ApplicationOwner>, IdentityError> {
        self.assert_open()?;
        match self.observe(id).await? {
            Some(header) => self.resolve_owner(&header).await,
            None => Ok(None),
        }
    }

    /// The owner known now, through the lineage seen so far, without
    /// waiting: what decides an agent's catalog. A reservation not yet
    /// bound gives `None`. (Karaka: `ownerOfCached`.)
    ///
    /// # Errors
    ///
    /// As [`owner_of_session`](Self::owner_of_session).
    pub fn owner_of_cached_session(
        &self,
        session: &Session,
    ) -> Result<Option<ApplicationOwner>, IdentityError> {
        self.assert_open()?;
        self.cached(session.header().clone())
    }

    fn cached(&self, start: SessionHeader) -> Result<Option<ApplicationOwner>, IdentityError> {
        let mut seen = HashSet::new();
        let mut owner: Option<ApplicationOwner> = None;
        let mut header = Some(start);
        while let Some(current) = header {
            if !seen.insert(current.id.clone()) {
                return Err(error(Forbidden, "Cyclic chat lineage"));
            }
            let record = self.lock().chats.get(&current.id).cloned();
            if let Some(record) = record {
                let Some(binding) = record.binding() else {
                    return Ok(None);
                };
                check_binding(binding, &current)?;
                if let Some(owner) = &owner {
                    check_owner(owner, record.owner())?;
                }
                owner = Some(record.owner().clone());
            }
            let Some(parent) = current.parent_session else {
                return Ok(owner);
            };
            header = match self.store.get(&parent) {
                Some(session) => Some(session.header().clone()),
                None => self.lock().headers.get(&parent).cloned(),
            };
        }
        Ok(None)
    }

    async fn resolve_owner(
        &self,
        initial: &SessionHeader,
    ) -> Result<Option<ApplicationOwner>, IdentityError> {
        let mut seen = HashSet::new();
        let mut owner: Option<ApplicationOwner> = None;
        let mut header = Some(initial.clone());
        while let Some(current) = header {
            if !seen.insert(current.id.clone()) {
                return Err(error(Forbidden, "Cyclic chat lineage"));
            }
            self.lock()
                .headers
                .insert(current.id.clone(), current.clone());
            if let Some(record) = self.bound_record(&current.id, Forbidden)? {
                if let Some(binding) = record.binding() {
                    check_binding(binding, &current)?;
                }
                if let Some(owner) = &owner {
                    check_owner(owner, record.owner())?;
                }
                owner = Some(record.owner().clone());
            }
            let Some(parent) = current.parent_session else {
                return Ok(owner);
            };
            header = self.observe(&parent).await?;
        }
        Ok(None)
    }

    /// Refuses every later call, waits for the operations admitted, and
    /// forgets every record. (Karaka: `close`.)
    pub async fn close(&self) {
        self.lock().closed = true;
        let admitted: Vec<Arc<tokio::sync::Mutex<()>>> = self
            .locks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .cloned()
            .collect();
        for lock in admitted {
            let _settled = lock.lock().await;
        }
        let mut state = self.lock();
        state.chats.clear();
        state.headers.clear();
    }
}

impl std::fmt::Debug for KarakaIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.lock();
        f.debug_struct("KarakaIdentity")
            .field("chats", &state.chats.len())
            .field("closed", &state.closed)
            .finish_non_exhaustive()
    }
}

/// The port mcp-application reads, by session id: a live session's own
/// lineage, otherwise what is observed or seen of it.
impl SessionOwners for KarakaIdentity {
    fn owner_of<'a>(&'a self, session: &'a SessionId) -> OwnerFuture<'a> {
        Box::pin(async move {
            match self.store.get(session) {
                Some(live) => self.owner_of_session(&live).await,
                None => self.owner_of_id(session).await,
            }
        })
    }

    fn owner_of_cached(
        &self,
        session: &SessionId,
    ) -> Result<Option<ApplicationOwner>, IdentityError> {
        self.assert_open()?;
        let header = match self.store.get(session) {
            Some(live) => Some(live.header().clone()),
            None => self.lock().headers.get(session).cloned(),
        };
        match header {
            Some(header) => self.cached(header),
            None => Ok(None),
        }
    }
}
