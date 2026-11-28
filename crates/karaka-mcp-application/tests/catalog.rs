//! The catalog's readiness and contributions. (Karaka:
//! `mcp-application/tests/catalog.spec.ts`.) Its specs through the tool
//! policy are in `karaka-tool-policy`'s `tests/application.rs`.
//!
//! Not ported:
//! - "does not activate a disposed consumer when discovery finishes later"
//!   and "waits for dependent cleanup before the catalog owner finishes
//!   disposal": Cordis's activation and unloading; the kernel starts each
//!   plugin once and unloads none.

use std::collections::BTreeSet;

use karaka_mcp_application::ApplicationCatalog;

fn set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

#[test]
fn readiness_waits_for_every_endpoint_and_outlives_replacement() {
    let catalog = ApplicationCatalog::new(vec!["first".into(), "second".into()]).unwrap();
    assert_eq!(catalog.tools(), None);
    let first = catalog.endpoint("first").unwrap();
    let second = catalog.endpoint("second").unwrap();
    let contribution = catalog.register_tool("mcp__first__read");
    first.settle();
    assert_eq!(catalog.tools(), None);
    assert_eq!(catalog.pending(), ["second"]);
    second.settle();
    assert_eq!(catalog.tools(), Some(set(&["mcp__first__read"])));
    // Readiness stays while an endpoint is replaced.
    first.dispose();
    assert!(catalog.tools().is_some());
    contribution.remove();
    // A stale claim's settle does nothing.
    first.settle();
    assert!(catalog.tools().is_some());
    let replacement = catalog.endpoint("first").unwrap();
    let _kept = catalog.register_tool("mcp__first__read");
    replacement.settle();
    assert_eq!(catalog.tools(), Some(set(&["mcp__first__read"])));
    replacement.dispose();
    second.dispose();
}

#[test]
fn a_catalog_with_no_servers_is_ready_and_empty() {
    let catalog = ApplicationCatalog::new(Vec::new()).unwrap();
    assert_eq!(catalog.tools(), Some(BTreeSet::new()));
    assert!(catalog.pending().is_empty());
}

#[test]
fn undeclared_and_duplicate_owners_are_refused() {
    let catalog = ApplicationCatalog::new(vec!["app".into()]).unwrap();
    assert_eq!(
        catalog.endpoint("other").unwrap_err(),
        "Application catalog does not configure server \"other\""
    );
    let endpoint = catalog.endpoint("app").unwrap();
    assert_eq!(
        catalog.endpoint("app").unwrap_err(),
        "Application catalog server \"app\" already has an owner"
    );
    endpoint.dispose();
    endpoint.dispose();
    assert_eq!(catalog.tools(), None);
}

#[test]
fn contributions_are_counted_and_names_detached() {
    let catalog = ApplicationCatalog::new(Vec::new()).unwrap();
    let other = ApplicationCatalog::new(Vec::new()).unwrap();
    let first = catalog.register_tool("read");
    let second = catalog.register_tool("read");
    let _write = other.register_tool("write");
    let detached = catalog.names();
    first.remove();
    assert_eq!(catalog.names(), set(&["read"]));
    second.remove();
    assert!(catalog.names().is_empty());
    assert_eq!(detached, set(&["read"]));
    assert_eq!(other.names(), set(&["write"]));
}

#[test]
fn a_server_declared_twice_is_refused() {
    assert_eq!(
        ApplicationCatalog::new(vec!["app".into(), "app".into()]).unwrap_err(),
        "Application catalog server names must be unique"
    );
}

// The endpoint's row config, with Karaka's defaults. (Karaka: `Config`'s
// schema.)
#[test]
fn an_endpoint_row_reads_with_karakas_defaults() {
    use karaka_mcp_application::ApplicationConfig;
    use serde_json::json;

    let config = ApplicationConfig::from_json(
        &json!({ "applicationId": "app", "serverName": "app", "url": "http://127.0.0.1:1/mcp" }),
        "row",
    )
    .unwrap();
    assert_eq!(
        config,
        ApplicationConfig::new("app", "app", "http://127.0.0.1:1/mcp")
    );
    assert_eq!(config.tool_call_timeout_ms, 60_000);
    assert!(config.fail_on_startup_error);
    let config = ApplicationConfig::from_json(
        &json!({
            "applicationId": "app", "serverName": "app", "url": "u",
            "headers": { "x-static": "kept" }, "toolCallTimeoutMs": 5,
            "failOnStartupError": false, "allow": ["a"], "deny": ["b"],
            "reconnect": { "enabled": false },
        }),
        "row",
    )
    .unwrap();
    assert_eq!(config.headers, [("x-static".to_owned(), "kept".to_owned())]);
    assert_eq!(
        (config.tool_call_timeout_ms, config.fail_on_startup_error),
        (5, false)
    );
    assert_eq!(
        (config.allow, config.deny),
        (vec!["a".to_owned()], vec!["b".to_owned()])
    );
    assert_eq!(config.reconnect, Some(json!({ "enabled": false })));
    for (value, message) in [
        (json!([]), "row must be a mapping"),
        (
            json!({ "serverName": "app", "url": "u" }),
            "row needs \"applicationId\"",
        ),
        (
            json!({ "applicationId": "app", "serverName": "app", "url": "u", "colour": 1 }),
            "row: unknown key \"colour\"",
        ),
        (
            json!({ "applicationId": "app", "serverName": "app", "url": "u", "allow": "a" }),
            "row: \"allow\" must be a list of strings",
        ),
        (
            json!({ "applicationId": "app", "serverName": "app", "url": "u", "toolCallTimeoutMs": 0 }),
            "row: \"toolCallTimeoutMs\" must be a positive whole number",
        ),
    ] {
        assert_eq!(
            ApplicationConfig::from_json(&value, "row").unwrap_err(),
            message
        );
    }
}
