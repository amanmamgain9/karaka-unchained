//! The shared-bearer provider. (Karaka: `server-auth/tests/authentication.spec.ts`.)

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use karaka_credentials::{CredentialFuture, CredentialRef, Credentials, ResolvedCredential};
use karaka_identity::ApplicationId;
use karaka_server_auth::{
    ApplicationCredentialConfig, AuthenticatedApplication, BearerConfig, BearerServerAuth,
    ServerAuth, bearer_token,
};

/// Values the test sets, and how many resolves were made.
#[derive(Default)]
struct Fixture {
    values: Mutex<HashMap<String, String>>,
    resolves: AtomicUsize,
    fail: Mutex<bool>,
}

impl Credentials for Fixture {
    fn resolve<'a>(&'a self, reference: &'a CredentialRef) -> CredentialFuture<'a> {
        Box::pin(async move {
            self.resolves.fetch_add(1, Ordering::SeqCst);
            if *self.fail.lock().unwrap() {
                return Err("provider unavailable".into());
            }
            Ok(self
                .values
                .lock()
                .unwrap()
                .get(reference.as_str())
                .map(|value| ResolvedCredential {
                    value: value.clone(),
                    source: "fixture".into(),
                }))
        })
    }
}

fn application(id: &str, chat: &str, tool: &str) -> ApplicationCredentialConfig {
    ApplicationCredentialConfig {
        id: id.into(),
        chat_credential: chat.into(),
        tool_credential: tool.into(),
    }
}

fn applications() -> Vec<ApplicationCredentialConfig> {
    vec![
        application("app", "CHAT", "TOOLS"),
        application("other", "OTHER_CHAT", "OTHER_TOOLS"),
    ]
}

fn fixture(values: &[(&str, &str)]) -> (Arc<Fixture>, BearerServerAuth) {
    let credentials = Arc::new(Fixture::default());
    for (name, value) in values {
        credentials
            .values
            .lock()
            .unwrap()
            .insert((*name).into(), (*value).into());
    }
    let auth = BearerServerAuth::new(
        &BearerConfig {
            applications: applications(),
        },
        Arc::clone(&credentials) as _,
    )
    .unwrap();
    (credentials, auth)
}

fn app(id: &str) -> Option<AuthenticatedApplication> {
    Some(AuthenticatedApplication {
        application_id: ApplicationId::new(id),
    })
}

#[tokio::test]
async fn inbound_and_outbound_credentials_stay_separate_and_rotate() {
    let (credentials, auth) = fixture(&[("CHAT", "inbound"), ("TOOLS", "outbound")]);
    assert_eq!(
        auth.authenticate(Some("Bearer inbound")).await,
        Ok(app("app"))
    );
    assert_eq!(auth.authenticate(Some("Bearer outbound")).await, Ok(None));
    let id = ApplicationId::new("app");
    assert_eq!(auth.authorize_tools(&id).await.unwrap(), "Bearer outbound");
    {
        let mut values = credentials.values.lock().unwrap();
        values.insert("CHAT".into(), "rotated-inbound".into());
        values.insert("TOOLS".into(), "rotated-outbound".into());
    }
    assert_eq!(auth.authenticate(Some("Bearer inbound")).await, Ok(None));
    assert_eq!(
        auth.authenticate(Some("bearer rotated-inbound")).await,
        Ok(app("app"))
    );
    assert_eq!(
        auth.authorize_tools(&id).await.unwrap(),
        "Bearer rotated-outbound"
    );
}

#[tokio::test]
async fn malformed_authorization_is_denied_without_resolving() {
    let (credentials, auth) = fixture(&[("CHAT", "inbound"), ("TOOLS", "outbound")]);
    for header in [
        None,
        Some(""),
        Some("Basic inbound"),
        Some("Bearer"),
        Some("Bearer  inbound"),
        Some("Bearer inbound extra"),
    ] {
        assert_eq!(auth.authenticate(header).await, Ok(None), "{header:?}");
    }
    assert_eq!(credentials.resolves.load(Ordering::SeqCst), 0);
    assert_eq!(bearer_token("BEARER token"), Some("token"));
    assert_eq!(bearer_token("Bearer tok\u{a0}en"), None);
}

#[tokio::test]
async fn ambiguous_and_absent_credentials_are_denied() {
    let (_, auth) = fixture(&[("CHAT", "shared"), ("OTHER_CHAT", "shared")]);
    assert_eq!(auth.authenticate(Some("Bearer shared")).await, Ok(None));
    assert_eq!(auth.authenticate(Some("Bearer unknown")).await, Ok(None));
    assert_eq!(
        auth.authorize_tools(&ApplicationId::new("app")).await,
        Err("server-auth: tool credential for application \"app\" is not configured".into())
    );
    assert_eq!(
        auth.authorize_tools(&ApplicationId::new("unknown")).await,
        Err("server-auth: unknown application \"unknown\"".into())
    );
}

#[test]
fn bad_configurations_are_refused_at_construction() {
    let construct = |applications: Vec<ApplicationCredentialConfig>| {
        BearerServerAuth::new(&BearerConfig { applications }, Arc::new(Fixture::default()))
            .map(|_| ())
    };
    let mut doubled = applications();
    doubled.extend(applications());
    assert_eq!(
        construct(doubled),
        Err("server-auth: duplicate application id \"app\"".into())
    );
    assert_eq!(
        construct(vec![application("", "CHAT", "TOOLS")]),
        Err("server-auth: application id must not be empty".into())
    );
    assert_eq!(
        construct(vec![application("app", "not a reference", "TOOLS")]),
        Err("credential ref \"not a reference\" must match /^[A-Za-z_][A-Za-z0-9_]*$/".into())
    );
}

#[tokio::test]
async fn a_provider_failure_propagates() {
    let (credentials, auth) = fixture(&[("CHAT", "inbound"), ("TOOLS", "outbound")]);
    *credentials.fail.lock().unwrap() = true;
    assert_eq!(
        auth.authenticate(Some("Bearer inbound")).await,
        Err("provider unavailable".into())
    );
    assert_eq!(
        auth.authorize_tools(&ApplicationId::new("app")).await,
        Err("provider unavailable".into())
    );
}
