//! Scope keys, chains and layers. (dsh: `core/scope/tests/scope.spec.ts`
//! and `store.spec.ts`, for what a fixed three-level chain keeps of them.)

use karaka_scope::{PresetId, ScopeChain, ScopeKey, ScopedLayers};
use karaka_session::SessionId;

fn preset(id: &str) -> ScopeKey {
    ScopeKey::Preset(PresetId::new(id))
}

fn session(id: &str) -> ScopeKey {
    ScopeKey::Session(SessionId::new(id))
}

fn full() -> ScopeChain {
    ScopeChain::new(Some(PresetId::new("coach")), Some(SessionId::new("s1")))
}

// dsh's `scopeChainOf`: nearest first, `[key, parent, …]`; `chainLayers`
// walks it farthest first.
#[test]
fn a_chain_lists_its_scopes_both_ways() {
    let chain = full();
    assert_eq!(chain.nearest_first(), [session("s1"), preset("coach")]);
    assert_eq!(chain.farthest_first(), [preset("coach"), session("s1")]);
    assert_eq!(chain.nearest(), Some(session("s1")));
    assert!(ScopeChain::global().nearest_first().is_empty());
    assert_eq!(ScopeChain::global().nearest(), None);
    assert!(ScopeChain::global().is_global());
    let preset_only = ScopeChain::new(Some(PresetId::new("coach")), None);
    assert_eq!(preset_only.nearest(), Some(preset("coach")));
    assert_eq!(
        ScopeChain::session(SessionId::new("s1")).farthest_first(),
        [session("s1")]
    );
}

// dsh's `scopeTarget`: a global listener hears everyone, a scoped one hears
// its own scope and everything nested in it, and nothing else.
#[test]
fn a_chain_admits_global_registrations_and_its_own_scopes() {
    let chain = full();
    assert!(chain.admits(None));
    assert!(chain.admits(Some(&preset("coach"))));
    assert!(chain.admits(Some(&session("s1"))));
    assert!(!chain.admits(Some(&preset("other"))));
    assert!(!chain.admits(Some(&session("s2"))));
    // A preset's id and a session's are never the same scope.
    assert!(!chain.admits(Some(&preset("s1"))));
    assert!(!ScopeChain::global().admits(Some(&session("s1"))));
}

#[derive(Default)]
struct Names(Vec<&'static str>);

// dsh's `ScopedLayers`: the global layer always, a scoped one made by its
// first registration, read farthest first, and forgotten when empty.
#[test]
fn layers_are_read_along_the_chain_and_pruned_when_empty() {
    let mut layers: ScopedLayers<Names> = ScopedLayers::default();
    layers.layer_mut(None).0.push("global");
    layers.layer_mut(Some(&session("s1"))).0.push("session");
    layers.layer_mut(Some(&preset("coach"))).0.push("preset");
    layers.layer_mut(Some(&preset("other"))).0.push("other");

    let read: Vec<&str> = layers
        .chain_layers(&full())
        .iter()
        .flat_map(|(_, layer)| layer.0.clone())
        .collect();
    assert_eq!(read, ["preset", "session"]);
    let keys: Vec<ScopeKey> = layers
        .chain_layers(&full())
        .into_iter()
        .map(|(key, _)| key)
        .collect();
    assert_eq!(keys, [preset("coach"), session("s1")]);
    assert!(layers.chain_layers(&ScopeChain::global()).is_empty());
    assert_eq!(layers.global().0, ["global"]);
    assert_eq!(layers.peek(&preset("other")).unwrap().0, ["other"]);
    // Reads never make a layer.
    assert!(layers.peek(&session("s9")).is_none());
    assert_eq!(layers.scoped_len(), 3);

    layers.layer_mut(Some(&preset("other"))).0.clear();
    layers.prune(Some(&preset("other")), |layer| layer.0.is_empty());
    assert!(layers.peek(&preset("other")).is_none());
    // The global layer is never forgotten.
    layers.layer_mut(None).0.clear();
    layers.prune(None, |layer| layer.0.is_empty());
    assert!(layers.get(None).is_some());
    assert_eq!(layers.scoped_len(), 2);
}

#[test]
fn keys_say_what_they_are() {
    assert_eq!(preset("coach").to_string(), "preset \"coach\"");
    assert_eq!(session("s1").to_string(), "session \"s1\"");
    assert_eq!(ScopeKey::from(SessionId::new("s1")), session("s1"));
    assert_eq!(ScopeKey::from(PresetId::new("coach")), preset("coach"));
}
