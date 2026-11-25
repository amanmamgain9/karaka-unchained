//! A projection unit: what a domain contributes, and the type-erased form
//! the registry drives. (dsh: `ProjectionDefinition` and `ErasedDefinition`
//! in `session-projection/src/index.ts`.)

use std::any::{Any, TypeId};
use std::sync::Arc;

use karaka_session::{Entry, SessionHeader};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

/// One domain's per-session state, folded from the session's log. (dsh:
/// `ProjectionDefinition`, without `wire`, which is [`ClientView`].)
///
/// The registry calls these on every appended event, so they must be
/// quick, synchronous and pure: no I/O, no locks shared with anything else,
/// and no calls back into the registry or the session, which are locked
/// meanwhile. A state-carrying event must carry the whole state after the
/// change, never a delta, so each step stays cheap.
pub trait Projection: Send + Sync + 'static {
    /// The key the unit owns. (dsh: `key`, its `SessionProjectionStateMap`
    /// entry.)
    const KEY: &'static str;

    /// The unit's state. It is checkpointed as JSON and read back the same
    /// way, so its serde form is the persisted one. (dsh: the state type,
    /// whose `stateSchema` here is its `Deserialize`.)
    type State: Serialize + DeserializeOwned + Send + Sync + 'static;

    /// Bump it whenever the state's fields or the fold's meaning change, so
    /// a checkpoint from an older unit is folded again rather than carried
    /// on. (dsh: `stateVersion`.)
    fn state_version(&self) -> u64;

    /// The state of an empty log, from the session's fixed metadata and the
    /// exact number of events a fork inherited. That count is never to be
    /// worked out from a `session/end-seed` marker. (dsh: `init`.)
    fn init(&self, header: &SessionHeader, inherited_event_count: u64) -> Self::State;

    /// The state after `entry`, or `None` when the event leaves it
    /// unchanged. `None` costs nothing downstream: no view is worked out,
    /// and nothing is heard. (dsh: `apply`, where returning the same
    /// reference means unchanged.)
    fn apply(&self, state: &Self::State, entry: &Entry) -> Option<Self::State>;
}

/// The view of a unit's state that clients see. A unit without one is
/// host-only: it is folded and checkpointed, but never served or heard.
/// (dsh: `wire`.)
pub trait ClientView: Projection {
    /// The whole current value clients get for the key. (dsh: the
    /// `SessionProjectionMap` entry, which `viewSchema` checks.)
    type View: PartialEq + Serialize + Send + Sync + 'static;

    /// The view of `state`. A view equal to the last one heard is not heard
    /// again. (dsh: `view`, compared by reference.)
    fn view(&self, state: &Self::State) -> Self::View;
}

/// A unit's state, of the unit's own type.
pub(crate) type AnyState = Arc<dyn Any + Send + Sync>;

/// A view, of the unit's own type.
pub(crate) trait AnyView: Send + Sync {
    fn same(&self, other: &dyn AnyView) -> bool;
    fn json(&self, key: &str) -> Value;
    fn as_any(&self) -> &dyn Any;
}

struct ViewOf<V>(V);

impl<V: PartialEq + Serialize + Send + Sync + 'static> AnyView for ViewOf<V> {
    fn same(&self, other: &dyn AnyView) -> bool {
        other.as_any().downcast_ref::<V>() == Some(&self.0)
    }

    fn json(&self, key: &str) -> Value {
        to_json(key, "view", &self.0)
    }

    fn as_any(&self) -> &dyn Any {
        &self.0
    }
}

/// A unit's value as JSON. One that doesn't serialize is the unit's bug,
/// and fails loud, as dsh's schema parse throws.
fn to_json(key: &str, what: &str, value: &impl Serialize) -> Value {
    serde_json::to_value(value).unwrap_or_else(|error| {
        panic!("session projection \"{key}\" {what} doesn't serialize to JSON: {error}")
    })
}

/// A unit, its state and view types erased. (dsh: `ErasedDefinition`.)
pub(crate) trait Unit: Send + Sync {
    fn key(&self) -> &'static str;
    fn state_version(&self) -> u64;
    fn state_type(&self) -> TypeId;
    fn is_client_visible(&self) -> bool;
    fn init(&self, header: &SessionHeader, inherited_event_count: u64) -> AnyState;
    fn apply(&self, state: &AnyState, entry: &Entry) -> Option<AnyState>;
    /// `None` for a host-only unit.
    fn view(&self, state: &AnyState) -> Option<Box<dyn AnyView>>;
    fn state_json(&self, state: &AnyState) -> Value;
    fn state_from_json(&self, value: &Value) -> Result<AnyState, serde_json::Error>;
}

type ViewFn<P> = fn(&P, &<P as Projection>::State) -> Box<dyn AnyView>;

/// The erased form of a `P`, with its view when it has one.
pub(crate) struct Erased<P: Projection> {
    unit: P,
    view: Option<ViewFn<P>>,
}

impl<P: Projection> Erased<P> {
    pub(crate) fn host_only(unit: P) -> Self {
        Self { unit, view: None }
    }
}

impl<P: ClientView> Erased<P> {
    pub(crate) fn client_visible(unit: P) -> Self {
        Self {
            unit,
            view: Some(|unit, state| Box::new(ViewOf(unit.view(state)))),
        }
    }
}

impl<P: Projection> Erased<P> {
    fn state<'a>(&self, state: &'a AnyState) -> &'a P::State {
        // The registry keeps one state type per key, so a cell always holds
        // its unit's type.
        state
            .downcast_ref()
            .expect("a projection cell holds its unit's state type")
    }
}

impl<P: Projection> Unit for Erased<P> {
    fn key(&self) -> &'static str {
        P::KEY
    }

    fn state_version(&self) -> u64 {
        self.unit.state_version()
    }

    fn state_type(&self) -> TypeId {
        TypeId::of::<P::State>()
    }

    fn is_client_visible(&self) -> bool {
        self.view.is_some()
    }

    fn init(&self, header: &SessionHeader, inherited_event_count: u64) -> AnyState {
        Arc::new(self.unit.init(header, inherited_event_count))
    }

    fn apply(&self, state: &AnyState, entry: &Entry) -> Option<AnyState> {
        let next = self.unit.apply(self.state(state), entry)?;
        Some(Arc::new(next))
    }

    fn view(&self, state: &AnyState) -> Option<Box<dyn AnyView>> {
        self.view.map(|view| view(&self.unit, self.state(state)))
    }

    fn state_json(&self, state: &AnyState) -> Value {
        to_json(P::KEY, "state", self.state(state))
    }

    fn state_from_json(&self, value: &Value) -> Result<AnyState, serde_json::Error> {
        let state = <P::State as serde::Deserialize>::deserialize(value)?;
        Ok(Arc::new(state))
    }
}
