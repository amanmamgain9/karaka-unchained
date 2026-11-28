//! Who owns a session: the application server that opened it, and the
//! tenant and user that application asserted. (Karaka's own:
//! `@karaka-ai/identity`, `identity/src/owner.ts`.)
//!
//! Only what the MCP application needs is ported: the ids, the owner,
//! [`same_owner`], [`IdentityError`], and a [`SessionOwners`] port with
//! Karaka's two lookups, `ownerOf` and `ownerOfCached`. Karaka's
//! `KarakaIdentity` service also reserves, binds and readies chats, locks
//! them per chat, and authorizes callers against them
//! (`identity/src/index.ts`); that comes with the app ingress. Until then a
//! host sets owners on [`InMemorySessionOwners`].

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, PoisonError, RwLock};

use karaka_kernel::{BoxError, Plugin, PluginContext, PortId};
use karaka_session::SessionId;

macro_rules! id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

id!(
    /// An authenticated application server. (Karaka: `ApplicationId`.)
    ApplicationId
);
id!(
    /// A tenant, as an authenticated application asserts it. (Karaka:
    /// `TenantId`.)
    TenantId
);
id!(
    /// A user within that application and tenant. (Karaka: `UserId`.)
    UserId
);

/// Who owns a session, as authenticated ingress established it. (Karaka:
/// `ApplicationOwner`.)
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ApplicationOwner {
    pub application_id: ApplicationId,
    pub tenant_id: TenantId,
    pub user_id: UserId,
}

/// Whether all three ids agree. (Karaka: `sameOwner`.)
pub fn same_owner(left: &ApplicationOwner, right: &ApplicationOwner) -> bool {
    left == right
}

/// Why an identity check failed. Its message names no other owner.
/// (Karaka: `IdentityError`.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdentityError {
    pub code: IdentityErrorCode,
    pub message: String,
}

/// (Karaka: `IdentityError.code`.)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdentityErrorCode {
    Forbidden,
    Unavailable,
}

impl IdentityErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Forbidden => "forbidden",
            Self::Unavailable => "unavailable",
        }
    }
}

impl IdentityError {
    /// (Karaka: `IdentityError.name`.)
    pub const NAME: &'static str = "IdentityError";

    pub fn new(code: IdentityErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl fmt::Display for IdentityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for IdentityError {}

/// What [`SessionOwners::owner_of`] resolves to.
pub type OwnerFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Option<ApplicationOwner>, IdentityError>> + Send + 'a>>;

/// The port: which application owns a session. (Karaka: `KarakaIdentity`'s
/// `ownerOf` and `ownerOfCached`.)
pub trait SessionOwners: Send + Sync {
    /// The trusted owner, for a tool call: `None` until ownership is
    /// established. A call checks it every time, so a revoked owner is
    /// refused even where a cached lookup admitted it. (Karaka: `ownerOf`.)
    fn owner_of<'a>(&'a self, session: &'a SessionId) -> OwnerFuture<'a>;

    /// The owner known now, without waiting: what decides which catalog an
    /// agent is offered. `None` grants no tools. (Karaka: `ownerOfCached`.)
    ///
    /// # Errors
    ///
    /// The owner's records disagree.
    fn owner_of_cached(
        &self,
        session: &SessionId,
    ) -> Result<Option<ApplicationOwner>, IdentityError>;
}

/// Owners a host sets, held in memory: the stand-in for Karaka's identity
/// service until the app ingress is ported.
#[derive(Debug, Default)]
pub struct InMemorySessionOwners {
    owners: RwLock<HashMap<SessionId, ApplicationOwner>>,
}

impl InMemorySessionOwners {
    pub fn new() -> Self {
        Self::default()
    }

    /// Makes `owner` own `session`, giving back the owner it replaced.
    pub fn set(&self, session: SessionId, owner: ApplicationOwner) -> Option<ApplicationOwner> {
        self.owners
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(session, owner)
    }

    /// Forgets `session`'s owner.
    pub fn remove(&self, session: &SessionId) -> Option<ApplicationOwner> {
        self.owners
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(session)
    }

    fn get(&self, session: &SessionId) -> Option<ApplicationOwner> {
        self.owners
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(session)
            .cloned()
    }
}

impl SessionOwners for InMemorySessionOwners {
    fn owner_of<'a>(&'a self, session: &'a SessionId) -> OwnerFuture<'a> {
        Box::pin(async move { Ok(self.get(session)) })
    }

    fn owner_of_cached(
        &self,
        session: &SessionId,
    ) -> Result<Option<ApplicationOwner>, IdentityError> {
        Ok(self.get(session))
    }
}

/// Provides [`SessionOwners`], backed by the [`InMemorySessionOwners`] it
/// also provides, so the host can set owners on it.
#[derive(Debug, Default)]
pub struct SessionOwnersPlugin {
    owners: Arc<InMemorySessionOwners>,
}

impl SessionOwnersPlugin {
    pub fn new() -> Self {
        Self::default()
    }

    /// A plugin providing `owners`, which the caller keeps to set owners on.
    pub fn with(owners: Arc<InMemorySessionOwners>) -> Self {
        Self { owners }
    }
}

impl Plugin for SessionOwnersPlugin {
    fn name(&self) -> &str {
        "session-owners"
    }

    fn provides(&self) -> Vec<PortId> {
        vec![
            PortId::of::<dyn SessionOwners>(),
            PortId::of::<InMemorySessionOwners>(),
        ]
    }

    async fn setup(&self, ctx: &mut PluginContext<'_>) -> Result<(), BoxError> {
        let owners = Arc::clone(&self.owners);
        ctx.provide::<dyn SessionOwners>(Arc::clone(&owners) as Arc<dyn SessionOwners>);
        ctx.provide(owners);
        Ok(())
    }
}
