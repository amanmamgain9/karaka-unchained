//! The session persistence seam: durable storage for session logs.
//! (dsh: `@deepseek-ai/dsh-session-persistence`, 0.2.1-rc.2.)
//!
//! [`SessionPersistence`] is the port a backend provides. It creates and
//! opens stored sessions, observes them by their headers, and hands out
//! [`SessionHandle`]s that read and append. Each backend is its own crate;
//! `karaka-session-jsonl` is the first.
//!
//! [`DurableSessions`] keeps the in-memory [`SessionStore`]'s sessions on a
//! backend. It hears every append through a [`SessionListener`], and its
//! [`LiveWriter`]s write them in batches. Without it, sessions stay in
//! memory, as before.
//!
//! - `persistence.rs`: the port and the handle.
//! - `error.rs`: dsh's error kinds and their messages.
//! - `contract.rs`: checks every backend makes the same way.
//! - `live.rs`: the live write path and its batching window.
//! - `durable.rs`: the store kept on a backend, and its plugin.
//!
//! [`SessionStore`]: karaka_session::SessionStore
//! [`SessionListener`]: karaka_session::SessionListener

mod contract;
mod durable;
mod error;
mod live;
mod persistence;

pub use contract::{
    MAX_SAFE_INTEGER, assert_contiguous, assert_create_header, assert_stored_id,
    validate_stored_events,
};
pub use durable::{DurableError, DurableSessions, DurableSessionsPlugin};
pub use error::{PersistenceError, SessionLocation, format_version_refusal};
pub use live::{BATCH_WINDOW, LiveWriter};
pub use persistence::{
    CreateOptions, FormatStatus, PersistenceFuture, SessionAccess, SessionHandle,
    SessionPersistence, SessionSnapshot,
};
