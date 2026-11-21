//! The session seam: what a session is, and the store that holds them.
//!
//! A session is an append-only log of [`SessionEvent`]s, and the log is the
//! single source of truth. The message history the model sees is derived
//! from it by [`SessionLog::messages`], never stored on its own. Keeping only
//! the log is what makes replay, compaction, and reading dsh's stored logs
//! possible later: each of them is a different way of reading the same
//! events. (dsh: `@deepseek-ai/dsh-session`, `packages/core/session`.)
//!
//! Storage is a separate concern. This crate keeps sessions in memory only,
//! in a [`SessionStore`] that [`SessionsPlugin`] provides as a port. Durable
//! backends such as JSONL, SQLite or Postgres will come later as their own
//! plugins, saving these same events. They need two things from this crate,
//! and both already hold: events are only ever appended, and their sequence
//! numbers are contiguous from 0. When the first backend arrives, the events
//! get serde's `Serialize` and `Deserialize`; until then nothing here depends
//! on serde.

mod derive;
mod event;
mod log;
mod plugin;
mod repair;
mod store;

pub use event::{Entry, Seq, SessionEvent};
pub use log::SessionLog;
pub use plugin::SessionsPlugin;
pub use repair::{INTERRUPTED_TOOL_RESULT, interrupted_result};
pub use store::{Session, SessionError, SessionId, SessionStore};
