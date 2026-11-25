//! The session projection registry: units driven eagerly over appended
//! events, cells built lazily, the unchanged gates, one consistent cut per
//! snapshot, shared and versioned keys, removal, and checkpoints. (dsh:
//! `session-projection/tests/registry.spec.ts`; each test names its case.)
//!
//! dsh's cases that rest on JavaScript or Cordis alone aren't ported: a
//! non-integer `stateVersion`, and a view that returns a Promise, can't be
//! written in Rust; the fiber-unload cases are the handles' `remove` here;
//! and `hydrate` waits for the projection cache.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use karaka_kernel::Runtime;
use karaka_session::{
    Entry, Passthrough, Seq, Session, SessionEvent, SessionHeader, SessionId, SessionStore,
    SessionsPlugin, Time, TurnEndReason,
};
use karaka_session_projection::{
    ChangeSubscription, CheckpointRow, ClientView, Projection, ProjectionCheckpoint,
    SessionProjections, SessionProjectionsPlugin,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
struct Marks {
    marks: Vec<String>,
}

fn marks(items: &[&str]) -> Marks {
    Marks {
        marks: items.iter().map(|item| (*item).to_owned()).collect(),
    }
}

fn mark_event(items: &[&str]) -> SessionEvent {
    SessionEvent::Passthrough(Passthrough {
        event_type: "test/mark".into(),
        data: json!({ "marks": items }),
        surface_op: None,
        source_event_seqs: None,
    })
}

fn mark(session: &Session, items: &[&str]) -> Seq {
    session.append(mark_event(items)).unwrap()
}

fn marks_of(entry: &Entry) -> Option<Marks> {
    match &entry.event {
        SessionEvent::Passthrough(event) if event.event_type == "test/mark" => {
            serde_json::from_value(event.data.clone()).ok()
        }
        _ => None,
    }
}

fn is_turn_start(entry: &Entry) -> bool {
    matches!(entry.event, SessionEvent::TurnStart(_))
}

/// The latest `test/mark` wins; other events leave it unchanged. (dsh:
/// `marksUnit`.)
struct MarksUnit {
    version: u64,
}

impl MarksUnit {
    fn new() -> Self {
        Self { version: 1 }
    }
}

impl Projection for MarksUnit {
    const KEY: &'static str = "test/marks";
    type State = Option<Marks>;

    fn state_version(&self) -> u64 {
        self.version
    }

    fn init(&self, _: &SessionHeader, _: u64) -> Option<Marks> {
        None
    }

    fn apply(&self, _: &Option<Marks>, entry: &Entry) -> Option<Option<Marks>> {
        marks_of(entry).map(Some)
    }
}

impl ClientView for MarksUnit {
    type View = Marks;

    fn view(&self, state: &Option<Marks>) -> Marks {
        state.clone().unwrap_or_else(|| marks(&[]))
    }
}

/// A host-only count of every event. (dsh: `countUnit`.)
struct CountUnit {
    version: u64,
}

impl CountUnit {
    fn new() -> Self {
        Self { version: 1 }
    }
}

impl Projection for CountUnit {
    const KEY: &'static str = "test/count";
    type State = u64;

    fn state_version(&self) -> u64 {
        self.version
    }

    fn init(&self, _: &SessionHeader, _: u64) -> u64 {
        0
    }

    fn apply(&self, state: &u64, _: &Entry) -> Option<u64> {
        Some(state + 1)
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct Stable {
    revision: u64,
    value: Marks,
}

/// Its state changes on every turn start, its view only on a mark. (dsh:
/// `stableViewUnit`.)
struct StableUnit {
    views: Arc<AtomicUsize>,
}

impl Projection for StableUnit {
    const KEY: &'static str = "test/stable-view";
    type State = Stable;

    fn state_version(&self) -> u64 {
        1
    }

    fn init(&self, _: &SessionHeader, _: u64) -> Stable {
        Stable {
            revision: 0,
            value: marks(&[]),
        }
    }

    fn apply(&self, state: &Stable, entry: &Entry) -> Option<Stable> {
        if is_turn_start(entry) {
            return Some(Stable {
                revision: state.revision + 1,
                value: state.value.clone(),
            });
        }
        marks_of(entry).map(|value| Stable {
            revision: state.revision + 1,
            value,
        })
    }
}

impl ClientView for StableUnit {
    type View = Marks;

    fn view(&self, state: &Stable) -> Marks {
        self.views.fetch_add(1, Ordering::SeqCst);
        state.value.clone()
    }
}

/// Host-only; its initial state is the inherited cut. (dsh: `cutUnit`.)
struct CutUnit;

impl Projection for CutUnit {
    const KEY: &'static str = "test/cut";
    type State = u64;

    fn state_version(&self) -> u64 {
        1
    }

    fn init(&self, _: &SessionHeader, inherited_event_count: u64) -> u64 {
        inherited_event_count
    }

    fn apply(&self, _: &u64, _: &Entry) -> Option<u64> {
        None
    }
}

fn harness() -> (Arc<SessionStore>, SessionProjections) {
    let store = Arc::new(SessionStore::new());
    let projections = SessionProjections::new(&store);
    (store, projections)
}

fn fresh(store: &SessionStore) -> Session {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let id = format!("session-{}", NEXT.fetch_add(1, Ordering::SeqCst));
    store.create(SessionId::new(id)).unwrap()
}

fn values(projections: &SessionProjections, session: &Session) -> Value {
    Value::Object(projections.snapshot(session, None).values)
}

fn entry(seq: u64, event: SessionEvent) -> Entry {
    Entry {
        seq: Seq(seq),
        time: Time(seq as i64),
        ignorable: false,
        event,
    }
}

fn turn_end(turn: u64) -> SessionEvent {
    SessionEvent::turn_end(turn, TurnEndReason::Completed)
}

fn row(ver: u64, seq: Option<u64>, val: Value) -> CheckpointRow {
    CheckpointRow {
        ver,
        seq: seq.map(Seq),
        val,
    }
}

fn rows(list: &[(&str, CheckpointRow)]) -> ProjectionCheckpoint {
    list.iter()
        .map(|(key, row)| ((*key).to_owned(), row.clone()))
        .collect()
}

fn restore_header() -> SessionHeader {
    SessionHeader::new(SessionId::new("projection-restore"), 0)
}

/// Every change heard: the session, key, value and seq.
type Heard = Arc<Mutex<Vec<(String, String, Value, Seq)>>>;

fn listen(projections: &SessionProjections) -> (Heard, ChangeSubscription) {
    let heard = Heard::default();
    let sink = Arc::clone(&heard);
    let subscription = projections.on_changed(move |change| {
        sink.lock().unwrap().push((
            change.session.as_str().to_owned(),
            change.key.to_owned(),
            change.value.clone(),
            change.seq,
        ));
    });
    (heard, subscription)
}

// "supplies the exact inherited cut to live, restored, and hydrated
// projection initialization", without `hydrate`.
#[test]
fn units_start_from_the_exact_inherited_cut() {
    let (store, projections) = harness();
    let _cut = projections.register_host_only(CutUnit).unwrap();
    let inherited = vec![entry(0, SessionEvent::turn_start(1)), entry(1, turn_end(1))];
    let mut header = SessionHeader::new(SessionId::new("projection-cut"), 1);
    header.is_seeded = true;
    let session = store
        .restore_with(header.clone(), inherited.len() as u64, inherited.clone())
        .unwrap();

    assert_eq!(
        projections.state_of::<CutUnit>(&session).as_deref(),
        Some(&2)
    );
    let restored = projections
        .restore(&ProjectionCheckpoint::new(), &inherited, 0, &header, 2)
        .unwrap();
    assert_eq!(restored.checkpoint["test/cut"].val, json!(2));
}

// "drives a registered unit over committed events and snapshots the
// current value"
#[test]
fn a_registered_unit_follows_appends_and_snapshots_its_value() {
    let (store, projections) = harness();
    let session = fresh(&store);
    let _marks = projections.register(MarksUnit::new()).unwrap();
    mark(&session, &["a"]);
    mark(&session, &["a", "b"]);
    let snapshot = projections.snapshot(&session, None);
    assert_eq!(
        Value::Object(snapshot.values),
        json!({ "test/marks": { "marks": ["a", "b"] } })
    );
    assert_eq!(snapshot.as_of_seq, Some(Seq(session.len() as u64 - 1)));
}

// "builds the cell lazily from the full log for a unit registered after
// events flowed"
#[test]
fn a_unit_registered_after_events_folds_the_whole_log() {
    let (store, projections) = harness();
    let session = fresh(&store);
    mark(&session, &["pre-registration"]);
    let _marks = projections.register(MarksUnit::new()).unwrap();
    assert_eq!(
        values(&projections, &session),
        json!({ "test/marks": { "marks": ["pre-registration"] } })
    );
    // The cell built lazily then carries on with the appends.
    mark(&session, &["after"]);
    assert_eq!(
        values(&projections, &session),
        json!({ "test/marks": { "marks": ["after"] } })
    );
}

// "serves init-derived state and asOfSeq -1 for an empty log"
#[test]
fn an_empty_log_serves_its_initial_state_at_no_seq() {
    let (store, projections) = harness();
    let session = fresh(&store);
    let _marks = projections.register(MarksUnit::new()).unwrap();
    let snapshot = projections.snapshot(&session, None);
    assert_eq!(snapshot.as_of_seq, None);
    assert_eq!(
        serde_json::to_value(&snapshot).unwrap(),
        json!({ "asOfSeq": -1, "values": { "test/marks": { "marks": [] } } })
    );
}

// "notifies onChanged with the validated view and the causing seq, and
// skips same-reference applies"
#[test]
fn a_change_is_heard_with_its_view_and_seq_and_an_unchanged_apply_is_not() {
    let (store, projections) = harness();
    let session = fresh(&store);
    let _marks = projections.register(MarksUnit::new()).unwrap();
    let (heard, _listening) = listen(&projections);
    let seq = mark(&session, &["a"]);
    // An event the unit leaves unchanged is not heard.
    session.append(SessionEvent::turn_start(1)).unwrap();
    assert_eq!(
        *heard.lock().unwrap(),
        [(
            session.id().as_str().to_owned(),
            "test/marks".to_owned(),
            json!({ "marks": ["a"] }),
            seq,
        )]
    );
}

// "does not compute a view while no change listener exists"
#[test]
fn no_view_is_worked_out_while_nothing_listens() {
    let (store, projections) = harness();
    let session = fresh(&store);
    let views = Arc::new(AtomicUsize::new(0));
    let _stable = projections
        .register(StableUnit {
            views: Arc::clone(&views),
        })
        .unwrap();
    session.append(SessionEvent::turn_start(1)).unwrap();
    session.append(SessionEvent::turn_start(2)).unwrap();
    assert_eq!(
        projections
            .state_of::<StableUnit>(&session)
            .unwrap()
            .revision,
        2
    );
    assert_eq!(views.load(Ordering::SeqCst), 0);
}

// "publishes the first observed view and suppresses later same-reference
// views"
#[test]
fn the_first_view_is_heard_and_an_unchanged_view_is_not() {
    let (store, projections) = harness();
    let session = fresh(&store);
    let views = Arc::new(AtomicUsize::new(0));
    let _stable = projections
        .register(StableUnit {
            views: Arc::clone(&views),
        })
        .unwrap();
    let (heard, _listening) = listen(&projections);
    let heard_values = || -> Vec<Value> {
        heard
            .lock()
            .unwrap()
            .iter()
            .map(|(_, _, value, _)| value.clone())
            .collect()
    };

    session.append(SessionEvent::turn_start(1)).unwrap();
    session.append(SessionEvent::turn_start(2)).unwrap();
    assert_eq!(heard_values(), [json!({ "marks": [] })]);
    assert_eq!(views.load(Ordering::SeqCst), 2);

    mark(&session, &["changed"]);
    assert_eq!(
        heard_values(),
        [json!({ "marks": [] }), json!({ "marks": ["changed"] })]
    );
    assert_eq!(views.load(Ordering::SeqCst), 3);
}

// "publishes the first view after an unobserved state change"
#[test]
fn the_first_view_after_an_unheard_change_is_heard() {
    let (store, projections) = harness();
    let session = fresh(&store);
    let views = Arc::new(AtomicUsize::new(0));
    let _stable = projections
        .register(StableUnit {
            views: Arc::clone(&views),
        })
        .unwrap();
    let (first, listening) = listen(&projections);
    session.append(SessionEvent::turn_start(1)).unwrap();
    listening.remove();
    session.append(SessionEvent::turn_start(2)).unwrap();
    assert_eq!(views.load(Ordering::SeqCst), 1);

    let (resumed, _listening) = listen(&projections);
    session.append(SessionEvent::turn_start(3)).unwrap();
    assert_eq!(first.lock().unwrap().len(), 1);
    assert_eq!(resumed.lock().unwrap()[0].2, json!({ "marks": [] }));
    assert_eq!(resumed.lock().unwrap().len(), 1);
    assert_eq!(views.load(Ordering::SeqCst), 2);
}

/// States and views by token, for the identity sweep. (dsh: the unit
/// `matches every four-state identity sequence` registers.)
#[derive(Default)]
struct Identities {
    state_by_event: Mutex<HashMap<String, Marks>>,
    view_by_state: Mutex<HashMap<Marks, Marks>>,
    computed: Mutex<Vec<Marks>>,
}

struct IdentityUnit(Arc<Identities>);

impl Projection for IdentityUnit {
    const KEY: &'static str = "test/marks";
    type State = Marks;

    fn state_version(&self) -> u64 {
        1
    }

    fn init(&self, _: &SessionHeader, _: u64) -> Marks {
        marks(&["initial"])
    }

    fn apply(&self, state: &Marks, entry: &Entry) -> Option<Marks> {
        let token = marks_of(entry)?.marks.first()?.clone();
        let next = self.0.state_by_event.lock().unwrap().get(&token)?.clone();
        (next != *state).then_some(next)
    }
}

impl ClientView for IdentityUnit {
    type View = Marks;

    fn view(&self, state: &Marks) -> Marks {
        let view = self
            .0
            .view_by_state
            .lock()
            .unwrap()
            .get(state)
            .cloned()
            .expect("test state lacks a view");
        self.0.computed.lock().unwrap().push(view.clone());
        view
    }
}

const STATE_SEQUENCES: [[usize; 4]; 15] = [
    [0, 0, 0, 0],
    [0, 0, 0, 1],
    [0, 0, 1, 0],
    [0, 0, 1, 1],
    [0, 0, 1, 2],
    [0, 1, 0, 0],
    [0, 1, 0, 1],
    [0, 1, 0, 2],
    [0, 1, 1, 0],
    [0, 1, 1, 1],
    [0, 1, 1, 2],
    [0, 1, 2, 0],
    [0, 1, 2, 1],
    [0, 1, 2, 2],
    [0, 1, 2, 3],
];

/// Every way `length` values can share identities, first-use ordered.
fn identity_sequences(length: usize) -> Vec<Vec<usize>> {
    fn visit(sequence: Vec<usize>, highest: usize, length: usize, out: &mut Vec<Vec<usize>>) {
        if sequence.len() == length {
            out.push(sequence);
            return;
        }
        for value in 0..=highest + 1 {
            let mut next = sequence.clone();
            next.push(value);
            visit(next, highest.max(value), length, out);
        }
    }
    let mut out = Vec::new();
    visit(vec![0], 0, length, &mut out);
    out
}

// "matches every four-state identity sequence across listener gaps and
// raw-view identities". dsh tells views apart by reference; here a view's
// identity is its value, so each identity gets a distinct value.
#[test]
fn every_four_state_identity_sequence_matches_across_listener_gaps() {
    let (store, projections) = harness();
    let identities = Arc::new(Identities::default());
    let _unit = projections
        .register(IdentityUnit(Arc::clone(&identities)))
        .unwrap();

    let mut failures = Vec::new();
    let mut checked = 0;
    for state_sequence in STATE_SEQUENCES {
        let state_count = state_sequence.iter().max().unwrap() + 1;
        for view_sequence in identity_sequences(state_count) {
            for baseline_known in [false, true] {
                for listener_mask in 0..8u32 {
                    let scenario = checked;
                    checked += 1;
                    let states: Vec<Marks> = (0..state_count)
                        .map(|index| Marks {
                            marks: vec![format!("state-{scenario}-{index}")],
                        })
                        .collect();
                    let views: Vec<Marks> = (0..=*view_sequence.iter().max().unwrap())
                        .map(|index| Marks {
                            marks: vec![format!("view-{scenario}-{index}")],
                        })
                        .collect();
                    for (index, state) in states.iter().enumerate() {
                        identities
                            .view_by_state
                            .lock()
                            .unwrap()
                            .insert(state.clone(), views[view_sequence[index]].clone());
                    }
                    for (index, state) in state_sequence.iter().enumerate() {
                        identities
                            .state_by_event
                            .lock()
                            .unwrap()
                            .insert(format!("{scenario}:{index}"), states[*state].clone());
                    }

                    let session = fresh(&store);
                    let notifications = Arc::new(Mutex::new(Vec::<u64>::new()));
                    let mut subscription: Option<ChangeSubscription> = None;
                    let mut set_listening = |listening: bool| {
                        if listening && subscription.is_none() {
                            let sink = Arc::clone(&notifications);
                            let id = session.id().clone();
                            subscription = Some(projections.on_changed(move |change| {
                                if *change.session == id && change.key == "test/marks" {
                                    sink.lock().unwrap().push(change.seq.0);
                                }
                            }));
                        } else if !listening && let Some(stop) = subscription.take() {
                            stop.remove();
                        }
                    };

                    set_listening(baseline_known);
                    mark(&session, &[&format!("{scenario}:0")]);
                    identities.computed.lock().unwrap().clear();
                    notifications.lock().unwrap().clear();

                    let mut expected_views = Vec::new();
                    let mut expected_notifications = Vec::new();
                    let mut comparable =
                        baseline_known.then(|| views[view_sequence[state_sequence[0]]].clone());
                    for index in 1..state_sequence.len() {
                        let listening = listener_mask & (1 << (index - 1)) != 0;
                        set_listening(listening);
                        if state_sequence[index] != state_sequence[index - 1] {
                            if listening {
                                let current = views[view_sequence[state_sequence[index]]].clone();
                                expected_views.push(current.clone());
                                if comparable.as_ref() != Some(&current) {
                                    expected_notifications.push(index as u64);
                                }
                                comparable = Some(current);
                            } else {
                                comparable = None;
                            }
                        }
                        mark(&session, &[&format!("{scenario}:{index}")]);
                    }
                    set_listening(false);

                    let computed = identities.computed.lock().unwrap().clone();
                    let heard = notifications.lock().unwrap().clone();
                    if computed != expected_views || heard != expected_notifications {
                        failures.push(format!(
                            "states {state_sequence:?}, views {view_sequence:?}, baseline \
                             {baseline_known}, listeners {listener_mask:03b}: views {} for {}, \
                             heard {heard:?} for {expected_notifications:?}",
                            computed.len(),
                            expected_views.len(),
                        ));
                    }
                    identities.computed.lock().unwrap().clear();
                }
            }
        }
    }
    assert_eq!((checked, failures), (960, Vec::<String>::new()));
}

// "drives independently per session (cells are per-session watermarks)"
#[test]
fn each_session_is_followed_on_its_own() {
    let (store, projections) = harness();
    let session = fresh(&store);
    let other = fresh(&store);
    let _marks = projections.register(MarksUnit::new()).unwrap();
    mark(&session, &["one"]);
    mark(&other, &["two"]);
    assert_eq!(
        values(&projections, &session),
        json!({ "test/marks": { "marks": ["one"] } })
    );
    assert_eq!(
        values(&projections, &other),
        json!({ "test/marks": { "marks": ["two"] } })
    );
}

// "updates host-only units without publishing them to wire listeners"
#[test]
fn a_host_only_unit_changes_unheard() {
    let (store, projections) = harness();
    let session = fresh(&store);
    let _marks = projections.register(MarksUnit::new()).unwrap();
    let _count = projections.register_host_only(CountUnit::new()).unwrap();
    let (heard, _listening) = listen(&projections);
    session.append(SessionEvent::turn_start(1)).unwrap();
    assert!(heard.lock().unwrap().is_empty());
    assert_eq!(
        projections.state_of::<CountUnit>(&session).as_deref(),
        Some(&1)
    );
    assert_eq!(
        values(&projections, &session),
        json!({ "test/marks": { "marks": [] } })
    );
}

// "shares one unit between registrants of the same key"
#[test]
fn registrants_of_one_key_share_its_unit() {
    let (store, projections) = harness();
    let session = fresh(&store);
    let _first = projections.register(MarksUnit::new()).unwrap();
    let _second = projections.register(MarksUnit::new()).unwrap();
    mark(&session, &["kept"]);
    assert_eq!(
        values(&projections, &session),
        json!({ "test/marks": { "marks": ["kept"] } })
    );
}

// "keeps the unit until the last registrant releases it"
#[test]
fn a_shared_unit_stays_until_its_last_registrant_removes_it() {
    let (store, projections) = harness();
    let session = fresh(&store);
    let first = projections.register(MarksUnit::new()).unwrap();
    let second = projections.register(MarksUnit::new()).unwrap();
    mark(&session, &["kept"]);
    first.remove();
    assert_eq!(
        values(&projections, &session),
        json!({ "test/marks": { "marks": ["kept"] } })
    );
    second.remove();
    assert_eq!(values(&projections, &session), json!({}));
}

// "refuses to share a key across a stateVersion change"
#[test]
fn a_key_isnt_shared_across_a_state_version_change() {
    let (_store, projections) = harness();
    let _marks = projections.register(MarksUnit::new()).unwrap();
    let error = projections.register(MarksUnit { version: 9 }).unwrap_err();
    assert_eq!(
        error.to_string(),
        "session projection key \"test/marks\" is already registered at stateVersion 1; \
         refusing to share it with stateVersion 9"
    );
}

/// A unit under `test/marks` whose state is another type.
struct OtherMarks;

impl Projection for OtherMarks {
    const KEY: &'static str = "test/marks";
    type State = u64;

    fn state_version(&self) -> u64 {
        1
    }

    fn init(&self, _: &SessionHeader, _: u64) -> u64 {
        0
    }

    fn apply(&self, _: &u64, _: &Entry) -> Option<u64> {
        None
    }
}

// Not in dsh, whose shared cells are untyped JSON: here a unit's state is a
// type, and two types can't share cells.
#[test]
fn a_key_isnt_shared_across_state_types() {
    let (store, projections) = harness();
    let session = fresh(&store);
    let _marks = projections.register(MarksUnit::new()).unwrap();
    let error = projections.register_host_only(OtherMarks).unwrap_err();
    assert_eq!(
        error.to_string(),
        "session projection key \"test/marks\" is already registered with another state type; \
         refusing to share it"
    );
    assert_eq!(projections.state_of::<OtherMarks>(&session), None);
}

// "register() disposer removes the key (with its cells) and frees it for
// re-registration"
#[test]
fn removing_a_unit_drops_its_cells_and_frees_its_key() {
    let (store, projections) = harness();
    let session = fresh(&store);
    let registration = projections.register(MarksUnit::new()).unwrap();
    mark(&session, &["cached"]);
    registration.remove();
    assert_eq!(values(&projections, &session), json!({}));
    assert_eq!(projections.state_of::<MarksUnit>(&session), None);
    // A new registration folds the log again, not a stale cell.
    let _again = projections.register(MarksUnit { version: 2 }).unwrap();
    assert_eq!(
        values(&projections, &session),
        json!({ "test/marks": { "marks": ["cached"] } })
    );
}

// "removes registrations and change listeners when their owning fiber
// unloads (HMR safety)", with handles for the fiber.
#[test]
fn removed_units_and_listeners_stop_at_once() {
    let (store, projections) = harness();
    let session = fresh(&store);
    let registration = projections.register(MarksUnit::new()).unwrap();
    let (heard, listening) = listen(&projections);
    mark(&session, &["live"]);
    assert_eq!(heard.lock().unwrap().len(), 1);
    registration.remove();
    listening.remove();
    mark(&session, &["after-removal"]);
    assert_eq!(heard.lock().unwrap().len(), 1);
    assert_eq!(values(&projections, &session), json!({}));
}

// "snapshot serves client views and excludes host-only state"
#[test]
fn a_snapshot_serves_client_views_and_leaves_out_host_only_state() {
    let (store, projections) = harness();
    let session = fresh(&store);
    let _marks = projections.register(MarksUnit::new()).unwrap();
    let _count = projections.register_host_only(CountUnit::new()).unwrap();
    mark(&session, &["a", "b"]);
    assert_eq!(
        values(&projections, &session),
        json!({ "test/marks": { "marks": ["a", "b"] } })
    );
    assert_eq!(
        projections.state_of::<CountUnit>(&session).as_deref(),
        Some(&1)
    );
}

// dsh's `snapshot(session, keys)`: state is folded whole, but only the
// chosen views are served.
#[test]
fn a_snapshot_can_serve_only_some_keys() {
    let (store, projections) = harness();
    let session = fresh(&store);
    let _marks = projections.register(MarksUnit::new()).unwrap();
    let views = Arc::new(AtomicUsize::new(0));
    let _stable = projections
        .register(StableUnit {
            views: Arc::clone(&views),
        })
        .unwrap();
    mark(&session, &["a"]);
    let snapshot = projections.snapshot(&session, Some(&["test/marks", "test/unknown"]));
    assert_eq!(
        Value::Object(snapshot.values),
        json!({ "test/marks": { "marks": ["a"] } })
    );
    assert_eq!(snapshot.as_of_seq, Some(Seq(0)));
    assert_eq!(views.load(Ordering::SeqCst), 0);
}

// "checkpoints every persisted unit with its stateVersion and per-cell
// watermark"
#[test]
fn a_checkpoint_has_every_unit_with_its_version_and_watermark() {
    let (store, projections) = harness();
    let session = fresh(&store);
    let _marks = projections.register(MarksUnit::new()).unwrap();
    let _count = projections
        .register_host_only(CountUnit { version: 7 })
        .unwrap();
    let seq = mark(&session, &["a"]);
    let checkpoint = projections.checkpoint(&session);
    assert_eq!(
        serde_json::to_value(&checkpoint).unwrap(),
        json!({
            "test/count": { "ver": 7, "seq": seq.0, "val": 1 },
            "test/marks": { "ver": 1, "seq": seq.0, "val": { "marks": ["a"] } },
        })
    );
    // An empty log: the initial state, at no seq.
    let empty = fresh(&store);
    assert_eq!(
        serde_json::to_value(&projections.checkpoint(&empty)["test/marks"]).unwrap(),
        json!({ "ver": 1, "seq": -1, "val": null })
    );
}

// "checkpoint states are detached clones — mutating them cannot corrupt
// the watermark cache"
#[test]
fn a_checkpoint_is_detached_from_the_cells() {
    let (store, projections) = harness();
    let session = fresh(&store);
    let _marks = projections.register(MarksUnit::new()).unwrap();
    mark(&session, &["a"]);
    let mut checkpoint = projections.checkpoint(&session);
    checkpoint.get_mut("test/marks").unwrap().val["marks"]
        .as_array_mut()
        .unwrap()
        .push(json!("INJECTED"));
    assert_eq!(
        values(&projections, &session),
        json!({ "test/marks": { "marks": ["a"] } })
    );
    assert_eq!(
        projections.checkpoint(&session)["test/marks"].val,
        json!({ "marks": ["a"] })
    );
}

// "restoreFloor anchors one below the lowest usable watermark and at 0 for
// missing or mismatched rows"
#[test]
fn the_restore_floor_sits_one_below_the_lowest_usable_watermark() {
    let (_store, projections) = harness();
    assert_eq!(
        projections.restore_floor(&ProjectionCheckpoint::new()),
        None
    );
    let _marks = projections.register(MarksUnit::new()).unwrap();
    let _count = projections.register_host_only(CountUnit::new()).unwrap();
    assert_eq!(
        projections.restore_floor(&ProjectionCheckpoint::new()),
        Some(0)
    );
    // The lowest usable watermark is count's 5, so the tail starts at 5,
    // one below the first seq needed, which proves seq 5 still exists.
    assert_eq!(
        projections.restore_floor(&rows(&[
            ("test/marks", row(1, Some(10), json!({ "marks": [] }))),
            ("test/count", row(1, Some(5), json!(6))),
        ])),
        Some(5)
    );
    // A row at another version sends its key back to the whole log.
    assert_eq!(
        projections.restore_floor(&rows(&[
            ("test/marks", row(2, Some(10), json!({ "marks": [] }))),
            ("test/count", row(1, Some(5), json!(6))),
        ])),
        Some(0)
    );
    // Rows of an empty log need the tail from 0.
    assert_eq!(
        projections.restore_floor(&rows(&[
            ("test/marks", row(1, None, Value::Null)),
            ("test/count", row(1, None, json!(0))),
        ])),
        Some(0)
    );
}

// "restore folds the tail past each usable row and refolds from init on
// version mismatch"
#[test]
fn a_restore_folds_past_usable_rows_and_refolds_a_mismatched_one() {
    let (_store, projections) = harness();
    let _marks = projections.register(MarksUnit::new()).unwrap();
    let _count = projections.register_host_only(CountUnit::new()).unwrap();
    let tail = vec![entry(3, mark_event(&["new"])), entry(4, turn_end(1))];
    // The marks row is usable, the count row isn't; past seq 0 that can't
    // refold, so it asks for the whole log.
    let error = projections
        .restore(
            &rows(&[
                ("test/marks", row(1, Some(2), json!({ "marks": ["old"] }))),
                ("test/count", row(99, Some(2), json!(3))),
            ]),
            &tail,
            3,
            &restore_header(),
            0,
        )
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "session projection \"test/count\" cannot restore from seq 3: its checkpoint row is \
         missing, version-mismatched, or beyond the supplied log end; re-read from seq 0"
    );

    let mut full = vec![
        entry(0, SessionEvent::turn_start(1)),
        entry(1, mark_event(&["old"])),
        entry(2, mark_event(&["old", "2"])),
    ];
    full.extend(tail);
    let restored = projections
        .restore(
            &rows(&[
                (
                    "test/marks",
                    row(1, Some(2), json!({ "marks": ["old", "2"] })),
                ),
                ("test/count", row(99, Some(2), json!(3))),
            ]),
            &full,
            0,
            &restore_header(),
            0,
        )
        .unwrap();
    assert_eq!(restored.snapshot.as_of_seq, Some(Seq(4)));
    assert_eq!(
        Value::Object(restored.snapshot.values),
        json!({ "test/marks": { "marks": ["new"] } })
    );
    // The refreshed rows sit at the served cut, ready to be written back.
    assert_eq!(
        restored.checkpoint,
        rows(&[
            ("test/marks", row(1, Some(4), json!({ "marks": ["new"] }))),
            ("test/count", row(1, Some(4), json!(5))),
        ])
    );
}

// "restore over a suffix folds only past each row watermark and serves an
// exact empty-tail cut"
#[test]
fn a_restore_folds_only_past_each_watermark_and_serves_an_empty_tails_cut() {
    let (_store, projections) = harness();
    let _marks = projections.register(MarksUnit::new()).unwrap();
    let _count = projections.register_host_only(CountUnit::new()).unwrap();
    let tail = vec![entry(3, SessionEvent::turn_start(2)), entry(4, turn_end(2))];
    let restored = projections
        .restore(
            &rows(&[
                ("test/marks", row(1, Some(4), json!({ "marks": ["done"] }))),
                ("test/count", row(1, Some(2), json!(3))),
            ]),
            &tail,
            3,
            &restore_header(),
            0,
        )
        .unwrap();
    assert_eq!(restored.snapshot.as_of_seq, Some(Seq(4)));
    // Marks covers the tail already; count folds seqs 3 and 4 alone.
    assert_eq!(
        Value::Object(restored.snapshot.values),
        json!({ "test/marks": { "marks": ["done"] } })
    );
    assert_eq!(restored.checkpoint["test/count"], row(1, Some(4), json!(5)));

    // An empty tail, the checkpoint current: the cut is just below the base.
    let current = projections
        .restore(
            &rows(&[
                ("test/marks", row(1, Some(4), json!({ "marks": ["done"] }))),
                ("test/count", row(1, Some(4), json!(5))),
            ]),
            &[],
            5,
            &restore_header(),
            0,
        )
        .unwrap();
    assert_eq!(current.snapshot.as_of_seq, Some(Seq(4)));
    assert!(!current.snapshot.values.contains_key("test/count"));
    assert_eq!(current.checkpoint["test/count"], row(1, Some(4), json!(5)));
}

// "viewCheckpoint serves version-matching rows without any log and skips
// mismatched keys", and "viewCheckpoint and restore exclude host-only
// state while retaining its checkpoint"
#[test]
fn checkpoint_views_serve_matching_client_rows_alone() {
    let (_store, projections) = harness();
    let _marks = projections.register(MarksUnit::new()).unwrap();
    let _count = projections.register_host_only(CountUnit::new()).unwrap();
    let mismatched = rows(&[
        (
            "test/marks",
            row(1, Some(4), json!({ "marks": ["stored"] })),
        ),
        ("test/count", row(99, Some(4), json!(5))),
    ]);
    assert_eq!(
        Value::Object(projections.view_checkpoint(&mismatched, None)),
        json!({ "test/marks": { "marks": ["stored"] } })
    );
    assert_eq!(
        Value::Object(projections.view_checkpoint(&ProjectionCheckpoint::new(), None)),
        json!({})
    );
    assert_eq!(
        Value::Object(projections.view_checkpoint(&mismatched, Some(&["test/count"]))),
        json!({})
    );

    let matching = rows(&[
        (
            "test/marks",
            row(1, Some(4), json!({ "marks": ["stored"] })),
        ),
        ("test/count", row(1, Some(4), json!(5))),
    ]);
    let restored = projections
        .restore(&matching, &[], 5, &restore_header(), 0)
        .unwrap();
    assert_eq!(
        Value::Object(restored.snapshot.values),
        json!({ "test/marks": { "marks": ["stored"] } })
    );
    assert_eq!(restored.checkpoint["test/count"], matching["test/count"]);
}

// "rejects version-matching rows whose state no longer matches the
// registered schema"
#[test]
fn a_row_whose_state_no_longer_fits_its_type_is_refused() {
    let (_store, projections) = harness();
    let _marks = projections.register(MarksUnit::new()).unwrap();
    let drifted = rows(&[(
        "test/marks",
        row(1, Some(2), json!({ "marks": "not-an-array" })),
    )]);
    assert_eq!(
        Value::Object(projections.view_checkpoint(&drifted, None)),
        json!({})
    );
    let error = projections
        .restore(&drifted, &[], 3, &restore_header(), 0)
        .unwrap_err();
    assert!(
        error
            .to_string()
            .starts_with("session projection \"test/marks\" checkpoint state doesn't fit its type"),
        "{error}"
    );
}

// "restore rejects a row claiming events past the supplied log end (shrunk
// log ⇒ re-read)"
#[test]
fn a_row_past_the_supplied_end_asks_for_the_whole_log() {
    let (_store, projections) = harness();
    let _count = projections.register_host_only(CountUnit::new()).unwrap();
    let checkpoint = rows(&[("test/count", row(1, Some(9), json!(10)))]);
    // The floor sits on the watermark, so an intact log's tail has seq 9...
    assert_eq!(projections.restore_floor(&checkpoint), Some(9));
    let anchored = projections
        .restore(
            &checkpoint,
            &[entry(9, turn_end(2))],
            9,
            &restore_header(),
            0,
        )
        .unwrap();
    assert_eq!(Value::Object(anchored.snapshot.values), json!({}));
    assert_eq!(
        anchored.checkpoint["test/count"],
        row(1, Some(9), json!(10))
    );
    // ...and a log repaired down below it gives an empty tail, which no
    // tail read can fix.
    let error = projections
        .restore(&checkpoint, &[], 9, &restore_header(), 0)
        .unwrap_err();
    assert!(error.to_string().ends_with("re-read from seq 0"), "{error}");
    // The whole log drops the row and folds from the start.
    let events = vec![entry(0, SessionEvent::turn_start(1)), entry(1, turn_end(1))];
    let full = projections
        .restore(&checkpoint, &events, 0, &restore_header(), 0)
        .unwrap();
    assert_eq!(full.snapshot.as_of_seq, Some(Seq(1)));
    assert_eq!(Value::Object(full.snapshot.values), json!({}));
    assert_eq!(full.checkpoint["test/count"], row(1, Some(1), json!(2)));
}

// dsh's `restore` throws on a gap in the supplied events.
#[test]
fn a_restore_refuses_a_gap_in_its_events() {
    let (_store, projections) = harness();
    let _count = projections.register_host_only(CountUnit::new()).unwrap();
    let error = projections
        .restore(
            &ProjectionCheckpoint::new(),
            &[entry(0, SessionEvent::turn_start(1)), entry(2, turn_end(1))],
            0,
            &restore_header(),
            0,
        )
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "session projection \"test/count\" cannot restore across missing seq 1"
    );
}

// The store's restored sessions aren't heard, as dsh never publishes a
// seed: their units fold the stored log on first read, then follow appends.
#[test]
fn a_restored_session_is_folded_on_first_read_then_followed() {
    let (store, projections) = harness();
    let _marks = projections.register(MarksUnit::new()).unwrap();
    let (heard, _listening) = listen(&projections);
    let session = store
        .restore(
            SessionId::new("resumed"),
            [
                entry(0, mark_event(&["stored"])),
                entry(1, SessionEvent::turn_start(1)),
            ],
        )
        .unwrap();
    assert_eq!(
        values(&projections, &session),
        json!({ "test/marks": { "marks": ["stored"] } })
    );
    let seq = mark(&session, &["live"]);
    assert_eq!(seq, Seq(2));
    assert_eq!(
        heard
            .lock()
            .unwrap()
            .last()
            .map(|(_, _, value, seq)| (value.clone(), *seq)),
        Some((json!({ "marks": ["live"] }), Seq(2)))
    );
}

/// A client view of how many events there are.
struct Counted;

impl Projection for Counted {
    const KEY: &'static str = "test/counted";
    type State = u64;

    fn state_version(&self) -> u64 {
        1
    }

    fn init(&self, _: &SessionHeader, _: u64) -> u64 {
        0
    }

    fn apply(&self, state: &u64, _: &Entry) -> Option<u64> {
        Some(state + 1)
    }
}

impl ClientView for Counted {
    type View = u64;

    fn view(&self, state: &u64) -> u64 {
        *state
    }
}

// dsh's cut is one tick; here readers and appends run on threads at once,
// and every snapshot is still one cut: each value reflects exactly the
// events up to `asOfSeq`.
#[test]
fn a_snapshot_taken_during_appends_is_one_cut() {
    let (store, projections) = harness();
    let projections = Arc::new(projections);
    let session = fresh(&store);
    let _counted = projections.register(Counted).unwrap();
    let _marks = projections.register(MarksUnit::new()).unwrap();
    let writer = {
        let session = session.clone();
        thread::spawn(move || {
            for turn in 0..400 {
                session.append(SessionEvent::turn_start(turn)).unwrap();
            }
        })
    };
    let reader = {
        let projections = Arc::clone(&projections);
        let session = session.clone();
        thread::spawn(move || {
            let mut last = None;
            for _ in 0..400 {
                let snapshot = projections.snapshot(&session, None);
                let counted = snapshot.values["test/counted"].as_u64().unwrap();
                assert_eq!(counted, snapshot.as_of_seq.map_or(0, |seq| seq.0 + 1));
                assert!(snapshot.as_of_seq >= last, "a cut never goes back");
                last = snapshot.as_of_seq;
                let state = projections.state_of::<Counted>(&session).unwrap();
                assert!(*state >= counted);
            }
        })
    };
    writer.join().unwrap();
    reader.join().unwrap();
    assert_eq!(
        projections.snapshot(&session, None).values["test/counted"],
        json!(400)
    );
}

#[tokio::test]
async fn the_plugin_provides_a_registry_that_follows_the_store() {
    let runtime = Runtime::start(vec![
        Box::new(SessionsPlugin),
        Box::new(SessionProjectionsPlugin),
    ])
    .await
    .unwrap();
    let store = runtime.get::<SessionStore>();
    let projections = runtime.get::<SessionProjections>();
    let _marks = projections.register(MarksUnit::new()).unwrap();
    let (heard, _listening) = listen(&projections);
    let session = fresh(&store);
    mark(&session, &["plugged"]);
    assert_eq!(heard.lock().unwrap().len(), 1);
    assert_eq!(
        values(&projections, &session),
        json!({ "test/marks": { "marks": ["plugged"] } })
    );
}
