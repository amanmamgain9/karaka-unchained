//! The startup gate. (Karaka: `transport-http/tests/startup.spec.ts`.)
//!
//! Not ported: the launcher-service checks (`appReady`, `appExit`), the
//! Loader audit of pending, inactive and unimportable entries, and the
//! `disabled` expressions. The kernel has no Loader: a plugin that fails
//! fails `Runtime::start`, so the gate is never committed, which the second
//! case shows.

use std::sync::Arc;

use karaka_kernel::{BoxError, Plugin, PluginContext, Runtime};
use karaka_startup::{KarakaStartup, KarakaStartupPlugin, STARTING_BODY};

#[tokio::test]
async fn the_gate_opens_only_when_committed_and_closes_on_withdrawal() {
    let runtime = Runtime::start(vec![Box::new(KarakaStartupPlugin)])
        .await
        .unwrap();
    let startup: Arc<KarakaStartup> = runtime.get::<KarakaStartup>();
    assert!(!startup.is_ready());
    startup.commit();
    assert!(startup.is_ready());
    startup.withdraw();
    assert!(!startup.is_ready());
    assert_eq!(
        STARTING_BODY,
        r#"{"code":"STARTING","message":"Application is starting"}"#
    );
}

struct FailedIdentity;

impl Plugin for FailedIdentity {
    fn name(&self) -> &str {
        "identity"
    }

    async fn setup(&self, _: &mut PluginContext<'_>) -> Result<(), BoxError> {
        Err("identity storage unavailable".into())
    }
}

#[tokio::test]
async fn a_failed_plugin_fails_the_start_so_nothing_commits() {
    let error = Runtime::start(vec![
        Box::new(KarakaStartupPlugin),
        Box::new(FailedIdentity),
    ])
    .await
    .err()
    .unwrap();
    assert!(
        error.to_string().contains("identity storage unavailable"),
        "{error}"
    );
}
