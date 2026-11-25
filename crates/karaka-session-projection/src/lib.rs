//! The session projection seam: per-session state folded from each
//! session's log, which clients read whole and hear change. (dsh:
//! `@deepseek-ai/dsh-session-projection`, 0.2.1-rc.2.)
//!
//! A domain registers a [`Projection`] unit: a key, a state version, an
//! initial state from the session's header, and a synchronous fold over
//! each appended event. With a [`ClientView`] it is client-visible;
//! without, host-only. [`SessionProjections`] drives every unit over every
//! event of the store's sessions at once, and serves:
//!
//! - [`snapshot`](SessionProjections::snapshot): every client-visible
//!   view, one consistent cut, with the seq it reflects;
//! - [`on_changed`](SessionProjections::on_changed): each view as it
//!   changes, with the event that changed it;
//! - [`state_of`](SessionProjections::state_of): one unit's state;
//! - [`checkpoint`](SessionProjections::checkpoint),
//!   [`restore_floor`](SessionProjections::restore_floor),
//!   [`restore`](SessionProjections::restore) and
//!   [`view_checkpoint`](SessionProjections::view_checkpoint): states as
//!   JSON rows, for a projection cache to persist and read back.
//!
//! One crate, the seam and its only implementation together: the registry
//! is concrete, like the session store, and the units are what vary.
//!
//! - `unit.rs`: the unit contract, and its type-erased form.
//! - `registry.rs`: the registry, its drive and its reads.
//! - `cut.rs`: snapshots, checkpoint rows and changes.
//! - `plugin.rs`: the plugin that provides the port.

mod cut;
mod plugin;
mod registry;
mod unit;

pub use cut::{
    CheckpointRow, ProjectionChange, ProjectionCheckpoint, ProjectionError, ProjectionSnapshot,
    Restored,
};
pub use plugin::SessionProjectionsPlugin;
pub use registry::{ChangeSubscription, ProjectionRegistration, SessionProjections};
pub use unit::{ClientView, Projection};
