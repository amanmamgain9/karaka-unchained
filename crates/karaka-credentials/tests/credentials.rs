//! References, and the environment resolver, through a lookup of the
//! test's own so the real environment is never read.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use karaka_credentials::{CredentialRef, Credentials, EnvCredentials};

#[test]
fn a_reference_is_an_environment_variable_name() {
    for name in ["TOOLS", "_private", "a1_B2"] {
        assert_eq!(CredentialRef::new(name).unwrap().as_str(), name);
    }
    for name in ["", "1ST", "not a reference", "with-dash", "é"] {
        assert_eq!(
            CredentialRef::new(name).unwrap_err().to_string(),
            format!("credential ref \"{name}\" must match /^[A-Za-z_][A-Za-z0-9_]*$/")
        );
    }
}

#[tokio::test]
async fn the_environment_is_read_at_each_resolve_and_empty_is_absent() {
    let values: Arc<Mutex<HashMap<String, String>>> = Arc::default();
    let read = Arc::clone(&values);
    let credentials = EnvCredentials::with_lookup(Arc::new(move |name| {
        read.lock().unwrap().get(name).cloned()
    }));
    let tools = CredentialRef::new("TOOLS").unwrap();
    assert_eq!(credentials.resolve(&tools).await, Ok(None));
    values.lock().unwrap().insert("TOOLS".into(), String::new());
    assert_eq!(credentials.resolve(&tools).await, Ok(None));
    values
        .lock()
        .unwrap()
        .insert("TOOLS".into(), "first".into());
    let resolved = credentials.resolve(&tools).await.unwrap().unwrap();
    assert_eq!(
        (resolved.value.as_str(), resolved.source.as_str()),
        ("first", "env")
    );
    assert!(!format!("{resolved:?}").contains("first"));
    values
        .lock()
        .unwrap()
        .insert("TOOLS".into(), "rotated".into());
    assert_eq!(
        credentials.resolve(&tools).await.unwrap().unwrap().value,
        "rotated"
    );
}
