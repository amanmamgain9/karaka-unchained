//! [`SessionProjections`]: the table of units, the drive that folds every
//! appended event into each of them, and the reads. (dsh:
//! `SessionProjectionRegistry` in `session-projection/src/index.ts`.)

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use karaka_session::{
    Appended, Entry, FlushFuture, Seq, Session, SessionHeader, SessionId, SessionListener,
    SessionStore,
};
use serde_json::{Map, Value};

use crate::cut::{
    CheckpointRow, ProjectionChange, ProjectionCheckpoint, ProjectionError, ProjectionSnapshot,
    Restored,
};
use crate::unit::{AnyState, AnyView, ClientView, Erased, Projection, Unit};

/// One unit's state for one session. (dsh: `UnitCell`.)
struct Cell {
    state: AnyState,
    /// The seq of the last event folded, changed or not; `None` before any.
    observed: Option<Seq>,
    /// The last view worked out on the live drive, to compare the next one
    /// with; `None` when there is nothing to compare. dsh keeps the last two
    /// raw views; the one before is only ever compared, so one is kept.
    last_view: Option<Box<dyn AnyView>>,
}

/// A registered unit, its cells, and how many registrants share it. (dsh:
/// `Registration`, with cells by session id for its `WeakMap` by session.)
struct Registered {
    unit: Arc<dyn Unit>,
    cells: HashMap<SessionId, Cell>,
    refs: usize,
}

type Listener = Arc<dyn Fn(&ProjectionChange<'_>) + Send + Sync>;

#[derive(Default)]
struct Inner {
    /// In registration order, which every drive and read follows.
    units: Vec<Registered>,
    listeners: Vec<(u64, Listener)>,
    next_listener: u64,
}

/// The session projection registry. (dsh: `SessionProjectionRegistry`,
/// `ctx.sessionProjections`.)
///
/// Domains register units; carriers read snapshots and hear changes;
/// neither knows the other. Every event appended to a session of the store
/// it follows passes every unit's [`apply`](Projection::apply) at once, in
/// registration order. A unit's cells are built when first needed: a unit
/// registered after events flowed, or a session restored from storage,
/// folds the session's log from [`init`](Projection::init) on first touch.
///
/// # Locks
///
/// The drive runs as the store's listener, with the session's log locked,
/// and takes the registry's lock inside it. Reads never hold the registry's
/// lock while they read a log, so the two can't deadlock. A read copies the
/// events its cells haven't seen, then locks the registry; if an append
/// drove the cells past that copy meanwhile, it reads again. So every
/// snapshot is one cut, as dsh's synchronous tick makes it.
///
/// Units and change listeners run under these locks, so neither may call
/// back into the registry or the session.
pub struct SessionProjections {
    inner: Arc<Mutex<Inner>>,
}

impl SessionProjections {
    /// A registry following every session of `store`. (dsh: the
    /// constructor's `session/event` subscription.)
    pub fn new(store: &SessionStore) -> Self {
        let inner = Arc::new(Mutex::new(Inner::default()));
        store.listen(Arc::new(Follower(Arc::downgrade(&inner))));
        Self { inner }
    }

    /// Registers a client-visible unit. A registrant of a key already
    /// registered at the same state version shares its unit, and its cells.
    /// (dsh: `register` with `wire`.)
    ///
    /// # Errors
    ///
    /// The key is registered at another state version, or with another
    /// state type.
    pub fn register<P: ClientView>(
        &self,
        unit: P,
    ) -> Result<ProjectionRegistration, ProjectionError> {
        self.add(Arc::new(Erased::client_visible(unit)))
    }

    /// Registers a host-only unit: folded and checkpointed, never served or
    /// heard. (dsh: `register` without `wire`.)
    ///
    /// # Errors
    ///
    /// As [`register`](Self::register).
    pub fn register_host_only<P: Projection>(
        &self,
        unit: P,
    ) -> Result<ProjectionRegistration, ProjectionError> {
        self.add(Arc::new(Erased::host_only(unit)))
    }

    fn add(&self, unit: Arc<dyn Unit>) -> Result<ProjectionRegistration, ProjectionError> {
        let key = unit.key();
        let mut inner = self.lock();
        match inner.units.iter_mut().find(|known| known.unit.key() == key) {
            Some(known) => {
                if known.unit.state_version() != unit.state_version() {
                    return Err(ProjectionError::new(format!(
                        "session projection key {} is already registered at stateVersion {}; \
                         refusing to share it with stateVersion {}",
                        quoted(key),
                        known.unit.state_version(),
                        unit.state_version()
                    )));
                }
                if known.unit.state_type() != unit.state_type() {
                    return Err(ProjectionError::new(format!(
                        "session projection key {} is already registered with another state \
                         type; refusing to share it",
                        quoted(key)
                    )));
                }
                known.refs += 1;
            }
            None => inner.units.push(Registered {
                unit,
                cells: HashMap::new(),
                refs: 1,
            }),
        }
        Ok(ProjectionRegistration {
            inner: Arc::downgrade(&self.inner),
            key,
        })
    }

    /// Hears every change of a client-visible unit's view, for every
    /// session: once per unit whose view an event changed. (dsh:
    /// `onChanged`.)
    ///
    /// It is called as the event is appended, with the session's log
    /// locked, so it must be quick and must not call back into the
    /// registry or the session.
    pub fn on_changed(
        &self,
        listener: impl Fn(&ProjectionChange<'_>) + Send + Sync + 'static,
    ) -> ChangeSubscription {
        let mut inner = self.lock();
        let id = inner.next_listener;
        inner.next_listener += 1;
        inner.listeners.push((id, Arc::new(listener)));
        ChangeSubscription {
            inner: Arc::downgrade(&self.inner),
            id,
        }
    }

    /// `P`'s current state for `session`, read-only, once every unit has
    /// folded the session's log; no view is worked out. `None` when no unit
    /// of `P`'s key and state type is registered. (dsh: `stateOf`.)
    pub fn state_of<P: Projection>(&self, session: &Session) -> Option<Arc<P::State>> {
        if !self
            .lock()
            .units
            .iter()
            .any(|known| known.unit.key() == P::KEY)
        {
            return None;
        }
        self.read(session, |inner, _| {
            let known = inner
                .units
                .iter()
                .find(|known| known.unit.key() == P::KEY)?;
            let state = Arc::clone(&known.cells.get(session.id())?.state);
            state.downcast::<P::State>().ok()
        })
    }

    /// One consistent cut over every client-visible unit of `session`, or
    /// just the units `keys` names. Every unit is folded either way. (dsh:
    /// `snapshot`.)
    pub fn snapshot(&self, session: &Session, keys: Option<&[&str]>) -> ProjectionSnapshot {
        self.read(session, |inner, as_of_seq| {
            let mut values = Map::new();
            for known in &inner.units {
                let key = known.unit.key();
                if !selected(keys, key) {
                    continue;
                }
                let Some(cell) = known.cells.get(session.id()) else {
                    continue;
                };
                if let Some(view) = known.unit.view(&cell.state) {
                    values.insert(key.to_owned(), view.json(key));
                }
            }
            ProjectionSnapshot { as_of_seq, values }
        })
    }

    /// Every unit's state for `session`, client-visible and host-only, with
    /// its version and seq: the rows a projection cache persists. Each
    /// state is JSON, detached from the cells. (dsh: `checkpoint`.)
    pub fn checkpoint(&self, session: &Session) -> ProjectionCheckpoint {
        self.read(session, |inner, _| {
            let mut rows = ProjectionCheckpoint::new();
            for known in &inner.units {
                if let Some(cell) = known.cells.get(session.id()) {
                    rows.insert(
                        known.unit.key().to_owned(),
                        CheckpointRow {
                            ver: known.unit.state_version(),
                            seq: cell.observed,
                            val: known.unit.state_json(&cell.state),
                        },
                    );
                }
            }
            rows
        })
    }

    /// Where a [`restore`](Self::restore) over `checkpoint` must read the
    /// stored log from: one event below the lowest usable watermark, or 0
    /// when a unit's row is missing or at another version. The event below
    /// proves the stored log still reaches the watermark, so a log that
    /// shrank under a row is caught. `None` when no unit is registered, and
    /// nothing needs reading. (dsh: `restoreFloor`.)
    pub fn restore_floor(&self, checkpoint: &ProjectionCheckpoint) -> Option<u64> {
        let inner = self.lock();
        let floor = inner
            .units
            .iter()
            .map(|known| match checkpoint.get(known.unit.key()) {
                Some(row) if row.ver == known.unit.state_version() => after(row.seq),
                _ => 0,
            })
            .min()?;
        Some(floor.saturating_sub(1))
    }

    /// The views of `checkpoint`'s rows, without reading any log: each
    /// client-visible unit, or each `keys` names, whose row is at its
    /// version and reads back as its state. Other keys are left out. (dsh:
    /// `viewCheckpoint`.)
    pub fn view_checkpoint(
        &self,
        checkpoint: &ProjectionCheckpoint,
        keys: Option<&[&str]>,
    ) -> Map<String, Value> {
        let inner = self.lock();
        let mut values = Map::new();
        for known in &inner.units {
            let unit = &known.unit;
            if !unit.is_client_visible() || !selected(keys, unit.key()) {
                continue;
            }
            let Some(row) = checkpoint.get(unit.key()) else {
                continue;
            };
            if row.ver != unit.state_version() {
                continue;
            }
            let Ok(state) = unit.state_from_json(&row.val) else {
                continue;
            };
            if let Some(view) = unit.view(&state) {
                values.insert(unit.key().to_owned(), view.json(unit.key()));
            }
        }
        values
    }

    /// Folds every unit over `events`, the stored log from `base_seq` on,
    /// each from its row when usable, without a live session. A row is
    /// usable when it is at the unit's version, not before `base_seq - 1`,
    /// and not past the events' end. (dsh: `restore`.)
    ///
    /// `events` should start at [`restore_floor`](Self::restore_floor), so
    /// its end is honest: an empty tail from there is a log that shrank.
    ///
    /// # Errors
    ///
    /// An unusable row when `base_seq` isn't 0, since only the whole log can
    /// be folded from `init`: the caller reads again from 0. A usable row
    /// whose state doesn't read back as its unit's type. A gap in `events`.
    pub fn restore(
        &self,
        checkpoint: &ProjectionCheckpoint,
        events: &[Entry],
        base_seq: u64,
        header: &SessionHeader,
        inherited_event_count: u64,
    ) -> Result<Restored, ProjectionError> {
        let before_base = base_seq.checked_sub(1).map(Seq);
        let end = events.last().map(|entry| entry.seq).or(before_base);
        let inner = self.lock();
        let mut values = Map::new();
        let mut rows = ProjectionCheckpoint::new();
        for known in &inner.units {
            let unit = &known.unit;
            let key = unit.key();
            let row = checkpoint.get(key).filter(|row| {
                row.ver == unit.state_version() && row.seq >= before_base && row.seq <= end
            });
            let (mut state, from) = match row {
                Some(row) => (
                    unit.state_from_json(&row.val).map_err(|error| {
                        ProjectionError::new(format!(
                            "session projection {} checkpoint state doesn't fit its type: {error}",
                            quoted(key)
                        ))
                    })?,
                    row.seq,
                ),
                None if base_seq > 0 => {
                    return Err(ProjectionError::new(format!(
                        "session projection {} cannot restore from seq {base_seq}: its \
                         checkpoint row is missing, version-mismatched, or beyond the supplied \
                         log end; re-read from seq 0",
                        quoted(key)
                    )));
                }
                None => (unit.init(header, inherited_event_count), before_base),
            };
            let start = usize::try_from(after(from) - base_seq).unwrap_or(usize::MAX);
            for (index, entry) in events.iter().enumerate().skip(start) {
                let expected = Seq(base_seq + index as u64);
                if entry.seq != expected {
                    return Err(ProjectionError::new(format!(
                        "session projection {} cannot restore across missing seq {expected}",
                        quoted(key)
                    )));
                }
                if let Some(next) = unit.apply(&state, entry) {
                    state = next;
                }
            }
            if let Some(view) = unit.view(&state) {
                values.insert(key.to_owned(), view.json(key));
            }
            rows.insert(
                key.to_owned(),
                CheckpointRow {
                    ver: unit.state_version(),
                    seq: end,
                    val: unit.state_json(&state),
                },
            );
        }
        Ok(Restored {
            snapshot: ProjectionSnapshot {
                as_of_seq: end,
                values,
            },
            checkpoint: rows,
        })
    }

    /// Brings every unit's cell for `session` up to one cut of its log, and
    /// runs `read` there, with the cut's last seq.
    fn read<R>(&self, session: &Session, read: impl FnOnce(&Inner, Option<Seq>) -> R) -> R {
        let id = session.id();
        let mut from = self.lock().needed_from(id);
        loop {
            // The log is read with the registry unlocked; see "Locks".
            let tail = match from {
                Some(from) => session.events_since(Seq(from)),
                None => Vec::new(),
            };
            let end = match from {
                Some(from) => from + tail.len() as u64,
                None => session.len() as u64,
            };
            let mut inner = self.lock();
            let needed = inner.needed_from(id);
            let covered = match (needed, from) {
                (None, _) => true,
                (Some(needed), Some(from)) => needed >= from && inner.reached(id) <= end,
                (Some(_), None) => false,
            };
            if !covered {
                from = needed;
                continue;
            }
            let base = from.unwrap_or(end);
            for known in &mut inner.units {
                let unit = Arc::clone(&known.unit);
                match known.cells.get_mut(id) {
                    Some(cell) => advance(&*unit, cell, &tail, base),
                    None => {
                        // Only a cut from 0 leaves a cell to build.
                        let cell = build(
                            &*unit,
                            session.header(),
                            session.inherited_event_count(),
                            &tail,
                        );
                        known.cells.insert(id.clone(), cell);
                    }
                }
            }
            return read(&inner, end.checked_sub(1).map(Seq));
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        lock(&self.inner)
    }
}

impl fmt::Debug for SessionProjections {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let inner = self.lock();
        let keys: Vec<&str> = inner.units.iter().map(|known| known.unit.key()).collect();
        f.debug_struct("SessionProjections")
            .field("keys", &keys)
            .field("listeners", &inner.listeners.len())
            .finish()
    }
}

impl Inner {
    /// The first seq some unit's cell for `id` hasn't folded: 0 when a cell
    /// is missing; `None` when no unit is registered.
    fn needed_from(&self, id: &SessionId) -> Option<u64> {
        self.units
            .iter()
            .map(|known| known.cells.get(id).map_or(0, |cell| after(cell.observed)))
            .min()
    }

    /// The first seq no cell for `id` has folded.
    fn reached(&self, id: &SessionId) -> u64 {
        self.units
            .iter()
            .filter_map(|known| known.cells.get(id))
            .map(|cell| after(cell.observed))
            .max()
            .unwrap_or(0)
    }

    /// Folds `entry`, just appended to `session`, into every unit, and says
    /// which views changed. (dsh: `drive`.)
    fn drive(&mut self, session: &Appended<'_>, entry: &Entry) -> Vec<(&'static str, Value)> {
        let listening = !self.listeners.is_empty();
        let mut changes = Vec::new();
        let seq = entry.seq;
        let before = &session.events()[..session.events().len() - 1];
        for known in &mut self.units {
            let unit = Arc::clone(&known.unit);
            match known.cells.get_mut(session.id()) {
                Some(cell) if cell.observed >= Some(seq) => continue,
                Some(cell) => advance(&*unit, cell, before, 0),
                // Built late, mid-stream: fold the log before this event,
                // then take the usual gates.
                None => {
                    let cell = build(
                        &*unit,
                        session.header(),
                        session.inherited_event_count(),
                        before,
                    );
                    known.cells.insert(session.id().clone(), cell);
                }
            }
            let Some(cell) = known.cells.get_mut(session.id()) else {
                continue;
            };
            cell.observed = Some(seq);
            let Some(next) = unit.apply(&cell.state, entry) else {
                // An unchanged state keeps its view to compare the next
                // change with.
                continue;
            };
            cell.state = next;
            if !unit.is_client_visible() {
                continue;
            }
            let previous = cell.last_view.take();
            if !listening {
                continue;
            }
            let Some(view) = unit.view(&cell.state) else {
                continue;
            };
            if previous.is_none_or(|previous| !previous.same(&*view)) {
                changes.push((unit.key(), view.json(unit.key())));
            }
            cell.last_view = Some(view);
        }
        changes
    }
}

/// The state of `unit` for a session, folded from `init` over `events`, the
/// log from its start.
fn build(unit: &dyn Unit, header: &SessionHeader, inherited: u64, events: &[Entry]) -> Cell {
    let mut state = unit.init(header, inherited);
    for entry in events {
        if let Some(next) = unit.apply(&state, entry) {
            state = next;
        }
    }
    Cell {
        state,
        observed: events.last().map(|entry| entry.seq),
        last_view: None,
    }
}

/// Folds into `cell` the events of `events` it hasn't seen; `events` is
/// the log from seq `base` on. A change on the way leaves no view to
/// compare the next one with. (dsh: `advanceCell`.)
fn advance(unit: &dyn Unit, cell: &mut Cell, events: &[Entry], base: u64) {
    let start = usize::try_from(after(cell.observed).saturating_sub(base)).unwrap_or(usize::MAX);
    for entry in events.iter().skip(start) {
        if let Some(next) = unit.apply(&cell.state, entry) {
            cell.state = next;
            cell.last_view = None;
        }
        cell.observed = Some(entry.seq);
    }
}

/// The first seq after `seq`, or 0 before any.
fn after(seq: Option<Seq>) -> u64 {
    seq.map_or(0, |seq| seq.0 + 1)
}

fn selected(keys: Option<&[&str]>, key: &str) -> bool {
    keys.is_none_or(|keys| keys.contains(&key))
}

fn quoted(key: &str) -> String {
    serde_json::to_string(key).unwrap_or_else(|_| format!("\"{key}\""))
}

fn lock(inner: &Mutex<Inner>) -> MutexGuard<'_, Inner> {
    // A unit that panicked mid-drive leaves each cell whole, at the last
    // event it folded, so carrying on is safe.
    inner.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The registry's listener on the store. It holds the registry weakly, so
/// a dropped registry stops driving.
struct Follower(Weak<Mutex<Inner>>);

impl SessionListener for Follower {
    fn event(&self, session: &Appended<'_>, entry: &Entry) {
        let Some(inner) = self.0.upgrade() else {
            return;
        };
        let (changes, listeners) = {
            let mut inner = lock(&inner);
            let changes = inner.drive(session, entry);
            let listeners: Vec<Listener> = inner
                .listeners
                .iter()
                .map(|(_, listener)| Arc::clone(listener))
                .collect();
            (changes, listeners)
        };
        // Heard with the registry unlocked.
        for (key, value) in &changes {
            let change = ProjectionChange {
                session: session.id(),
                key,
                value,
                seq: entry.seq,
            };
            for listener in &listeners {
                listener(&change);
            }
        }
    }

    fn flush(&self, _: &SessionId) -> Option<FlushFuture> {
        None
    }
}

/// A unit's registration. [`remove`](Self::remove) takes it back; the last
/// registrant of a key to remove it removes the key and its cells. Dropping
/// it keeps the unit, as a dsh registration stays until its disposer runs.
#[must_use = "dropping a registration keeps the unit; call `remove` to take it back"]
pub struct ProjectionRegistration {
    inner: Weak<Mutex<Inner>>,
    key: &'static str,
}

impl ProjectionRegistration {
    /// Takes the registration back. (dsh: the disposer.)
    pub fn remove(self) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        let mut inner = lock(&inner);
        if let Some(index) = inner
            .units
            .iter()
            .position(|known| known.unit.key() == self.key)
        {
            inner.units[index].refs -= 1;
            if inner.units[index].refs == 0 {
                inner.units.remove(index);
            }
        }
    }
}

impl fmt::Debug for ProjectionRegistration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProjectionRegistration")
            .field("key", &self.key)
            .finish()
    }
}

/// A change listener's subscription. [`remove`](Self::remove) stops it;
/// dropping it keeps it.
#[must_use = "dropping a subscription keeps the listener; call `remove` to stop it"]
pub struct ChangeSubscription {
    inner: Weak<Mutex<Inner>>,
    id: u64,
}

impl ChangeSubscription {
    /// Stops the listener. (dsh: the `onChanged` disposer.)
    pub fn remove(self) {
        if let Some(inner) = self.inner.upgrade() {
            lock(&inner).listeners.retain(|(id, _)| *id != self.id);
        }
    }
}

impl fmt::Debug for ChangeSubscription {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChangeSubscription")
            .field("id", &self.id)
            .finish()
    }
}
