//! The port: [`SessionPersistence`], which creates, opens, observes and
//! lists stored sessions, and the [`SessionHandle`] it hands out.
//! (dsh: `session-persistence/src/index.ts` and `handle.ts`.)
//!
//! dsh's methods return promises; here they return boxed futures, so the
//! port is a trait object a binary can choose at startup. dsh's
//! `AbortSignal` options aren't ported: dropping a future cancels it.

use std::future::Future;
use std::pin::Pin;

use karaka_session::{Entry, SessionHeader, SessionId};

use crate::PersistenceError;

/// A future a persistence call returns.
pub type PersistenceFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, PersistenceError>> + Send + 'a>>;

/// Whether a handle only reads, or owns the session's writes.
/// (dsh: `SessionAccess`.)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionAccess {
    Read,
    /// One write handle per session, per backend. (dsh: `'write'`.)
    Write,
}

/// How a stored log's format compares with this build's, from its header
/// alone. (dsh: `formatStatus`, the format catalog's header classification.)
///
/// Only the current format is readable until historical migration is
/// ported, so it has one value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormatStatus {
    Current,
}

/// What `stat` and `list` say about one stored session, without reading its
/// events. (dsh: `SessionPersistenceSnapshot`.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionSnapshot {
    pub header: SessionHeader,
    /// `None` for a session created in this process and not yet written.
    pub format_status: Option<FormatStatus>,
    /// A best-effort change token: equal revisions mean nothing changed.
    pub revision: String,
    /// The stored artifact's size, once it exists.
    pub size_bytes: Option<u64>,
}

/// Options for [`SessionPersistence::create`].
/// (dsh: `SessionPersistenceCreateOptions`.)
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CreateOptions {
    /// How many events at the start of a seeded log were inherited. Must be
    /// 0 for an unseeded header.
    pub inherited_event_count: u64,
}

/// Durable storage for sessions. (dsh: the `sessionPersistence` service.)
///
/// # Contract
///
/// - **One writer.** At most one write handle per session in one backend
///   instance; a second `create` or write `open` is refused until it
///   closes. A backend that can be shared between processes, such as files
///   on disk, adds its own lock for those.
/// - **Lazy materialization.** `create` stores nothing. The session exists
///   for this instance at once, and for others after its first append or
///   flush. Closing a handle that never stored anything erases the session.
/// - **Append-only, contiguous.** Every batch continues the stored log at
///   the next seq. An append resolves once it is durable.
/// - **Fail closed.** A log in a format this build doesn't read, or with an
///   event type it doesn't know and that isn't marked `ignorable`, is
///   refused, never partly interpreted.
/// - **Physically valid, not repaired.** Reads give the committed log; a
///   torn tail from a crash is never served. Closing a turn a crash left
///   open is the reader's job, not storage's.
pub trait SessionPersistence: Send + Sync {
    /// Starts a new session with `header` and takes its write handle.
    fn create(
        &self,
        header: SessionHeader,
        options: CreateOptions,
    ) -> PersistenceFuture<'_, Box<dyn SessionHandle>>;

    /// Opens a stored session for reading, or as its one writer.
    fn open<'a>(
        &'a self,
        id: &'a SessionId,
        access: SessionAccess,
    ) -> PersistenceFuture<'a, Box<dyn SessionHandle>>;

    /// Observes one session without reading its events: `None` when it
    /// doesn't exist.
    fn stat<'a>(&'a self, id: &'a SessionId) -> PersistenceFuture<'a, Option<SessionSnapshot>>;

    /// Every stored session visible here, in no promised order: stored
    /// ones, and ones this instance created and hasn't written yet.
    fn list(&self) -> PersistenceFuture<'_, Vec<SessionSnapshot>>;

    /// Flushes every write handle open at the call. Handles that close
    /// meanwhile count as flushed; the other failures are reported together.
    fn flush(&self) -> PersistenceFuture<'_, ()>;
}

/// An open session in a [`SessionPersistence`]. (dsh: `SessionHandle`.)
///
/// Dropping a handle without [`close`](Self::close) still gives up its
/// write ownership; `close` is the one way to learn whether that worked.
pub trait SessionHandle: Send + Sync {
    fn id(&self) -> &SessionId;

    fn header(&self) -> &SessionHeader;

    /// How many events at the start of the log were inherited.
    fn inherited_event_count(&self) -> u64;

    fn access(&self) -> SessionAccess;

    /// Up to `length` stored events from `offset` on (all of them when
    /// `length` is `None`); empty at or past the end. Never fewer events
    /// than an earlier read of this handle saw.
    fn read(&self, offset: usize, length: Option<usize>) -> PersistenceFuture<'_, Vec<Entry>>;

    /// Durably appends `events`, which must continue the log at the next
    /// seq. An empty batch does nothing. Write handles only.
    fn append(&self, events: Vec<Entry>) -> PersistenceFuture<'_, ()>;

    /// A durability barrier: everything appended is durable, and a session
    /// with nothing appended yet is stored empty. Write handles only.
    fn flush(&self) -> PersistenceFuture<'_, ()>;

    /// Waits for in-flight operations, then gives up the handle and its
    /// write ownership. Calling it again does nothing.
    fn close(&self) -> PersistenceFuture<'_, ()>;
}
