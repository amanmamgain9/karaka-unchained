//! The default selection and the call-config checks. (dsh:
//! `core/agent-default-model/tests/agent-default-model.spec.ts`.)
//!
//! Not ported: the volatile-config and `saveSelection` cases, which need
//! dsh's settings service and config editor.

use std::sync::Arc;

use karaka_agent::ModelSelection;
use karaka_agent_default_model::{
    AgentDefaultModel, AgentDefaultModelPlugin, list_providers, resolve_call_config,
};
use karaka_kernel::Runtime;
use karaka_model::{Model, ModelRegistry};

fn selection(provider: &str, model: &str, effort: Option<&str>) -> ModelSelection {
    ModelSelection {
        provider: provider.into(),
        model: model.into(),
        reasoning_effort: effort.map(str::to_owned),
    }
}

#[tokio::test]
async fn the_plugin_gives_a_detached_selection() {
    let runtime = Runtime::start(vec![Box::new(AgentDefaultModelPlugin::new(selection(
        "q",
        "n",
        Some("high"),
    )))])
    .await
    .unwrap();
    let default = runtime.get::<AgentDefaultModel>();
    assert_eq!(
        default.current_selection(),
        selection("q", "n", Some("high"))
    );
    assert_eq!(
        AgentDefaultModel::new(selection("p", "m", None)).current_selection(),
        selection("p", "m", None)
    );
}

struct Nothing;

impl Model for Nothing {
    fn provider(&self) -> &str {
        "nothing"
    }

    fn stream(&self, _: karaka_model::Request) -> karaka_model::EventStream {
        unimplemented!("never called")
    }
}

#[test]
fn a_call_config_needs_a_registered_provider_and_a_known_effort() {
    let registry = ModelRegistry::new();
    registry.register("mock", Arc::new(Nothing)).unwrap();
    assert_eq!(list_providers(&registry), ["mock"]);
    assert_eq!(
        resolve_call_config(&registry, selection("mock", "chat", Some("high"))),
        Ok(selection("mock", "chat", Some("high")))
    );
    let missing = resolve_call_config(&registry, selection("other", "chat", None)).unwrap_err();
    assert_eq!(
        (missing.message.as_str(), missing.code),
        ("no adapter registered for provider \"other\"", "NO_ADAPTER")
    );
    let effort =
        resolve_call_config(&registry, selection("mock", "chat", Some("off"))).unwrap_err();
    assert_eq!(
        (effort.message.as_str(), effort.code),
        (
            "provider \"mock\" model \"chat\" does not support reasoning effort \"off\"",
            "UNSUPPORTED_REASONING_EFFORT"
        )
    );
}
