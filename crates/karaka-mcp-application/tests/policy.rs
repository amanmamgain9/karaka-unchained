//! Application tool policy per scope. (Karaka:
//! `mcp-application/tests/policy.spec.ts`.)
//!
//! Karaka's agents nest, each its own scope under its parent's; here a
//! chain is a preset's scope, then a session's, so the preset stands in
//! for the parent agent. A second [`ApplicationPolicies`] stands in for a
//! second root.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use karaka_mcp_application::{ApplicationPolicies, ApplicationToolPolicy};
use karaka_scope::{PresetId, ScopeChain, ScopeKey};
use karaka_session::SessionId;

fn list(names: &[&str]) -> Option<Vec<String>> {
    Some(names.iter().map(|name| (*name).to_owned()).collect())
}

fn allow(names: &[&str]) -> ApplicationToolPolicy {
    ApplicationToolPolicy {
        allow: list(names),
        deny: None,
    }
}

fn session(id: &str) -> ScopeKey {
    ScopeKey::Session(SessionId::new(id))
}

fn chain(preset: Option<&str>, id: &str) -> ScopeChain {
    ScopeChain::new(preset.map(PresetId::new), Some(SessionId::new(id)))
}

fn preset(id: &str) -> ScopeKey {
    ScopeKey::Preset(PresetId::new(id))
}

#[test]
fn an_explicit_allow_is_needed_and_scopes_and_roots_are_isolated() {
    let policies = ApplicationPolicies::new();
    let other_root = ApplicationPolicies::new();
    let first = chain(None, "first");
    assert!(!policies.allows(&first, "mcp__app__read"));
    let _kept = policies
        .register(Some(&session("first")), allow(&["mcp__app__read"]))
        .unwrap();
    assert!(policies.allows(&first, "mcp__app__read"));
    assert!(!policies.allows(&first, "mcp__app__write"));
    assert!(!policies.allows(&chain(None, "sibling"), "mcp__app__read"));
    assert!(!other_root.allows(&first, "mcp__app__read"));
}

#[test]
fn inherited_allows_intersect_and_an_ancestor_deny_wins() {
    let policies = ApplicationPolicies::new();
    let _parent = policies
        .register(
            Some(&preset("parent")),
            ApplicationToolPolicy {
                allow: list(&["read", "write"]),
                deny: list(&["write"]),
            },
        )
        .unwrap();
    let _child = policies
        .register(Some(&session("child")), allow(&["read", "write", "admin"]))
        .unwrap();
    let child = chain(Some("parent"), "child");
    assert!(policies.allows(&child, "read"));
    assert!(!policies.allows(&child, "write"));
    assert!(!policies.allows(&child, "admin"));
}

#[test]
fn a_deny_alone_is_no_allow_and_an_omitted_child_allow_inherits() {
    let policies = ApplicationPolicies::new();
    let child = chain(Some("parent"), "child");
    let _child = policies
        .register(
            Some(&session("child")),
            ApplicationToolPolicy {
                allow: None,
                deny: list(&["write"]),
            },
        )
        .unwrap();
    assert!(!policies.allows(&child, "read"));
    let _parent = policies
        .register(Some(&preset("parent")), allow(&["read", "write"]))
        .unwrap();
    assert!(policies.allows(&child, "read"));
    assert!(!policies.allows(&child, "write"));
}

#[test]
fn removal_takes_the_policy_out_and_removed_watches_hear_nothing() {
    let policies = ApplicationPolicies::new();
    let foreign = ApplicationPolicies::new();
    let changed = Arc::new(AtomicUsize::new(0));
    let foreign_changed = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&changed);
    let _watch = policies.watch(Arc::new(move || {
        counter.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }));
    let counter = Arc::clone(&foreign_changed);
    let _foreign = foreign.watch(Arc::new(move || {
        counter.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }));
    let scoped = chain(None, "scoped");
    let registration = policies
        .register(Some(&session("scoped")), allow(&["read"]))
        .unwrap();
    assert!(policies.allows(&scoped, "read"));
    assert_eq!(changed.load(Ordering::SeqCst), 1);
    registration.remove().unwrap();
    assert!(!policies.allows(&scoped, "read"));
    assert_eq!(changed.load(Ordering::SeqCst), 2);
    assert_eq!(foreign_changed.load(Ordering::SeqCst), 0);
    let transient = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&transient);
    let stop = policies.watch(Arc::new(move || {
        counter.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }));
    stop.remove();
    let _kept = policies
        .register(Some(&session("scoped")), allow(&["read"]))
        .unwrap();
    assert_eq!(transient.load(Ordering::SeqCst), 0);
}

#[test]
fn an_unscoped_policy_is_refused_and_a_listener_error_rolls_back() {
    let policies = ApplicationPolicies::new();
    assert_eq!(
        policies.register(None, allow(&["read"])).unwrap_err(),
        "Karaka tool policy must be mounted inside an Agent preset"
    );
    let _watch = policies.watch(Arc::new(|| Err("registration conflict".into())));
    assert_eq!(
        policies
            .register(Some(&session("scoped")), allow(&["read"]))
            .unwrap_err(),
        "registration conflict"
    );
    assert!(!policies.allows(&chain(None, "scoped"), "read"));
}
