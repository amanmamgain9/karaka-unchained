//! Ownership records, and the immutable session fields a record is bound
//! to. (Karaka: `identity/src/records.ts`.)

use karaka_session::{Origin, SessionHeader, SessionId};

use crate::ApplicationOwner;

/// The logical fields that identify a session, fixed at creation; the
/// format version isn't one. (Karaka: `SessionBinding`.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionBinding {
    pub id: SessionId,
    pub created_at: u64,
    pub cwd: Option<String>,
    pub parent_session: Option<SessionId>,
    pub is_seeded: bool,
    pub origin: Option<Origin>,
    /// 0 when the header omits it.
    pub delegation_depth: u64,
    pub agent_preset: Option<String>,
}

/// The fields of `header` a record binds. (Karaka: `bindingOf`.)
pub fn binding_of(header: &SessionHeader) -> SessionBinding {
    SessionBinding {
        id: header.id.clone(),
        created_at: header.created_at,
        cwd: header.cwd.clone(),
        parent_session: header.parent_session.clone(),
        is_seeded: header.is_seeded,
        origin: header.origin,
        delegation_depth: header.delegation_depth,
        agent_preset: header.agent_preset.clone(),
    }
}

/// Whether `header` is the session `binding` was taken from. (Karaka:
/// `matchesBinding`.)
pub fn matches_binding(binding: &SessionBinding, header: &SessionHeader) -> bool {
    *binding == binding_of(header)
}

/// One chat's ownership: reserved before its session exists, bound to the
/// session once made, ready once durably acknowledged. (Karaka:
/// `IdentityRecord`.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IdentityRecord {
    Reserved {
        owner: ApplicationOwner,
    },
    Bound {
        owner: ApplicationOwner,
        binding: SessionBinding,
    },
    Ready {
        owner: ApplicationOwner,
        binding: SessionBinding,
    },
}

impl IdentityRecord {
    pub fn owner(&self) -> &ApplicationOwner {
        match self {
            Self::Reserved { owner } | Self::Bound { owner, .. } | Self::Ready { owner, .. } => {
                owner
            }
        }
    }

    /// The binding, for a bound or ready record.
    pub fn binding(&self) -> Option<&SessionBinding> {
        match self {
            Self::Reserved { .. } => None,
            Self::Bound { binding, .. } | Self::Ready { binding, .. } => Some(binding),
        }
    }
}
