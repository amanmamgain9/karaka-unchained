//! A session persistence backend: one JSONL log per session, under a root
//! directory. (dsh: `@deepseek-ai/dsh-session-persistence-jsonl`,
//! 0.2.1-rc.2.)
//!
//! # Layout
//!
//! ```text
//! <root>/
//!   --<normalized-cwd>--/       the project directory, or _no-cwd/
//!     <encoded-id>/             the session's own directory
//!       session.v4.jsonl.zstd   the log, compressed (the default), or
//!       session.v4.jsonl        the log as raw lines
//!       session.lock            the write lock
//! ```
//!
//! # Encodings
//!
//! The log is a header line, then one row per event. Compressed, the
//! default as in dsh, it is a series of independent, checksummed
//! Zstandard frames: one holding the header line, then one per durable
//! batch. Raw, the lines are written as they are, for outside readers. A
//! root belongs to one encoding, set by [`JsonlConfig::compression`]; a
//! log in the other is refused, never read or overwritten.
//!
//! # Durability and crashes
//!
//! - **Lazy.** `create` writes nothing. The first append, or a flush,
//!   publishes the header and the first batch with `link`, so it never
//!   lands on an existing log. A session closed before then never existed.
//! - **Synced batches.** Each append, a frame or lines, is written and
//!   `fsync`ed before it resolves. If the write or the sync fails, the
//!   file is cut back to its earlier length, the end of the last complete
//!   batch, so the batch can be retried at the same seq.
//! - **Torn tails.** What a crash mid-append left is never served whole. An
//!   incomplete last line is dropped. A torn last frame gives back the
//!   complete records already flushed into it, and nothing else. A write
//!   handle cuts the torn bytes before its first append, and writes the
//!   recovered records again as a frame of their own. Committed bytes are
//!   never rewritten. A turn the crash left open is stored as it was;
//!   closing it is the reader's job.
//! - **One writer.** One write handle per session in a backend, and the
//!   `flock` on `session.lock` against other processes and instances.
//!
//! # Not yet
//!
//! - Historical generations and their migration, and with them the storage
//!   verifier's stream check: an older log is refused, naming its path, and
//!   never written.
//! - Windows: dsh locks a named kernel semaphore there; this crate builds
//!   on Unix only.
//!
//! - `layout.rs`: paths, id escaping, encodings and generation names.
//! - `frames.rs`: Zstandard frames: the structural scan, and encoding and
//!   decoding them.
//! - `format.rs`: the header line, the row codec and the scanner, for both
//!   encodings.
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
pub mod frames;
mod layout;
mod lease;
mod plugin;

pub use backend::{JsonlConfig, JsonlPersistence};
pub use layout::{
    Compression, LOCK_FILENAME, encode_segment, generation_filename, generation_path, project_dir,
    project_key, session_dir,
};
pub use plugin::JsonlPlugin;
