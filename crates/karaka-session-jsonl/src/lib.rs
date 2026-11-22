//! A session persistence backend: one JSONL log per session, under a root
//! directory. (dsh: `@deepseek-ai/dsh-session-persistence-jsonl`,
//! 0.2.1-rc.2, with `compression: 'none'`.)
//!
//! # Layout
//!
//! ```text
//! <root>/
//!   --<normalized-cwd>--/     the project directory, or _no-cwd/
//!     <encoded-id>/           the session's own directory
//!       session.v4.jsonl      the log: a header line, then one row per event
//!       session.lock          the write lock
//! ```
//!
//! # Durability and crashes
//!
//! - **Lazy.** `create` writes nothing. The first append, or a flush,
//!   publishes the header and the first batch with `link`, so it never
//!   lands on an existing log. A session closed before then never existed.
//! - **Synced batches.** Each append is written and `fsync`ed before it
//!   resolves. If the write or the sync fails, the file is cut back to its
//!   earlier length, so the batch can be retried at the same seq.
//! - **Torn tails.** An incomplete last line, from a crash mid-append, is
//!   never served, and a write handle cuts it off before its first append.
//!   Committed events are never rewritten. A turn the crash left open is
//!   stored as it was; closing it is the reader's job.
//! - **One writer.** One write handle per session in a backend, and the
//!   `flock` on `session.lock` against other processes and instances.
//!
//! # Not yet
//!
//! - Zstandard compression, dsh's default, and so the torn-frame recovery
//!   and the storage verifier's stream check.
//! - Historical generations and their migration: an older log is refused,
//!   naming its path, and never written.
//! - Windows: dsh locks a named kernel semaphore there; this crate builds
//!   on Unix only.
//!
//! - `layout.rs`: paths, id escaping and generation names.
//! - `format.rs`: the header line, the row codec and the scanner.
//! - `admission.rs`: v4's structural checks on each row.
//! - `lease.rs`: the write lock.
//! - `file.rs`: publishing, appending, truncating and reading files.
//! - `backend.rs`: the backend and its handle.

#[cfg(not(unix))]
compile_error!("karaka-session-jsonl supports only Unix for now: dsh's Windows lock isn't ported");

mod admission;
mod backend;
mod file;
mod format;
mod layout;
mod lease;
mod plugin;

pub use backend::{JsonlConfig, JsonlPersistence};
pub use layout::{
    LOCK_FILENAME, encode_segment, generation_filename, generation_path, project_dir, project_key,
    session_dir,
};
pub use plugin::JsonlPlugin;
