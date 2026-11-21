//! `ModelsPlugin` started in a Karaka runtime, and providers registering
//! their models in the registry it provides.

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use futures_core::Stream;
use karaka_kernel::{BoxError, Plugin, PluginContext, PortId, Runtime};
use karaka_model::{
    Block, Event, EventStream, Message, Model, ModelError, ModelRegistry, ModelsPlugin, Request,
    StopReason, Usage, complete,
};

/// A model that always answers with its own name, so a test can tell which
/// route a request reached.
struct Named(&'static str);

impl Model for Named {
    fn provider(&self) -> &str {
        self.0
    }

    fn stream(&self, _request: Request) -> EventStream {
        Box::pin(Playback(VecDeque::from([
            Ok(Event::Block(Block::Text(self.0.into()))),
            Ok(Event::Done {
                stop: StopReason::EndTurn,
                usage: Usage::default(),
            }),
        ])))
    }
}

/// Every poll is immediately ready with the next event.
struct Playback(VecDeque<Result<Event, ModelError>>);

impl Stream for Playback {
    type Item = Result<Event, ModelError>;

    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Ready(self.get_mut().0.pop_front())
    }
}

/// A provider plugin in miniature: it registers a [`Named`] model under its
/// route, as the real providers do.
struct Provider(&'static str);

impl Plugin for Provider {
    fn name(&self) -> &str {
        self.0
    }

    fn requires(&self) -> Vec<PortId> {
        vec![PortId::of::<ModelRegistry>()]
    }

    async fn setup(&self, ctx: &mut PluginContext<'_>) -> Result<(), BoxError> {
        ctx.get::<ModelRegistry>()
            .register(self.0, Arc::new(Named(self.0)))?;
        Ok(())
    }
}

async fn answer(registry: &ModelRegistry, route: &str) -> String {
    let model = registry.get(route).expect("the route is registered");
    let request = Request::new("any-model", vec![Message::user("hi")]);
    complete(model.as_ref(), request)
        .await
        .expect("the call works")
        .message
        .text()
}

#[tokio::test]
async fn the_plugin_provides_an_empty_registry() {
    let runtime = Runtime::start(vec![Box::new(ModelsPlugin)])
        .await
        .expect("the runtime starts");
    assert_eq!(runtime.plugins(), ["models"]);
    assert!(runtime.get::<ModelRegistry>().routes().is_empty());
}

#[tokio::test]
async fn each_provider_answers_on_its_own_route() {
    // The providers are listed first; the runtime still starts the registry
    // before them, because they require it.
    let runtime = Runtime::start(vec![
        Box::new(Provider("anthropic")),
        Box::new(Provider("deepseek")),
        Box::new(ModelsPlugin),
    ])
    .await
    .expect("the runtime starts");
    assert_eq!(runtime.plugins(), ["models", "anthropic", "deepseek"]);

    let registry = runtime.get::<ModelRegistry>();
    assert_eq!(registry.routes(), ["anthropic", "deepseek"]);
    assert_eq!(answer(&registry, "anthropic").await, "anthropic");
    assert_eq!(answer(&registry, "deepseek").await, "deepseek");
}

#[tokio::test]
async fn a_model_registered_after_start_is_found() {
    let runtime = Runtime::start(vec![Box::new(ModelsPlugin)])
        .await
        .expect("the runtime starts");

    let registry = runtime.get::<ModelRegistry>();
    registry
        .register("ollama", Arc::new(Named("ollama")))
        .unwrap();
    // Every read of the port reaches the same registry.
    let again = runtime.get::<ModelRegistry>();
    assert_eq!(answer(&again, "ollama").await, "ollama");
}

#[tokio::test]
async fn a_provider_without_the_registry_plugin_cannot_start() {
    let Err(error) = Runtime::start(vec![Box::new(Provider("anthropic"))]).await else {
        panic!("expected an invalid plugin list");
    };
    // Only the end of the port's name is checked: Rust doesn't promise the
    // exact text of a type's name.
    let message = error.to_string();
    assert!(message.contains("\"anthropic\" requires port"), "{message}");
    assert!(
        message.contains("ModelRegistry`, which no plugin provides"),
        "{message}"
    );
}
