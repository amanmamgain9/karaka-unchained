use std::error::Error;
use std::fmt;
use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard};

use crate::Model;

/// Every model provider in a runtime, each under its own route.
/// (dsh: the `llm` service, with `registerAdapter` and `listProviders`.)
///
/// The kernel allows one provider per port, so providers can't each provide
/// `dyn Model`. Instead one plugin provides this registry, and every provider
/// plugin registers its model in it. That lets a runtime hold Anthropic and
/// DeepSeek at once, or two OpenAI-compatible servers.
///
/// A route names one configured provider, such as "anthropic", "deepseek" or
/// "ollama". It picks where a request goes; the request's `model` field
/// still picks the model at that route.
///
/// # When to register
///
/// Provider plugins register during their setup, so every route is in place
/// once the runtime has started. Registering later is allowed too: the lock
/// makes it safe from any thread. But nothing announces a new route, so code
/// that read [`routes`](Self::routes) earlier won't hear about it. There is
/// no way to remove a route.
///
/// # Locking
///
/// One `RwLock` guards the list. A `get` holds the read lock only long
/// enough to clone the model's `Arc` out, so many calls can look up models
/// at once, and no lock is held while a model runs. It is a `std::sync` lock
/// because nothing waits while holding it.
#[derive(Default)]
pub struct ModelRegistry {
    /// In the order they were registered. A runtime has a handful of
    /// routes, so finding one by walking the list is as fast as a map, and
    /// the list keeps the order for free.
    routes: RwLock<Vec<(String, Arc<dyn Model>)>>,
}

impl ModelRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds `model` under `route`.
    ///
    /// Fails if the route is empty, or if another model has it already. A
    /// route is never replaced: two providers claiming one name is a mistake
    /// in the config, and silently keeping one would hide it.
    pub fn register(
        &self,
        route: impl Into<String>,
        model: Arc<dyn Model>,
    ) -> Result<(), RegistryError> {
        let route = route.into();
        if route.trim().is_empty() {
            return Err(RegistryError::EmptyRoute);
        }
        // Checking and adding happen under one write lock, so two threads
        // registering the same route can't both succeed.
        let mut routes = self.routes.write().unwrap_or_else(PoisonError::into_inner);
        if routes.iter().any(|(name, _)| *name == route) {
            return Err(RegistryError::DuplicateRoute(route));
        }
        routes.push((route, model));
        Ok(())
    }

    /// The model registered under `route`, if there is one.
    pub fn get(&self, route: &str) -> Option<Arc<dyn Model>> {
        self.read()
            .iter()
            .find(|(name, _)| name == route)
            .map(|(_, model)| Arc::clone(model))
    }

    /// Every route, in the order they were registered. (dsh: `listProviders`.)
    pub fn routes(&self) -> Vec<String> {
        self.read().iter().map(|(name, _)| name.clone()).collect()
    }

    // A lock is "poisoned" when a thread panicked while holding it. The only
    // change to the list is one push, which either happened or didn't, so
    // the list is still whole, and it's safe to carry on with it.
    fn read(&self) -> RwLockReadGuard<'_, Vec<(String, Arc<dyn Model>)>> {
        self.routes.read().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Lists the routes. A `dyn Model` has no `Debug` of its own to show.
impl fmt::Debug for ModelRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ModelRegistry")
            .field("routes", &self.routes())
            .finish()
    }
}

/// Why [`ModelRegistry::register`] refused a model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegistryError {
    /// The route was empty, or only spaces.
    EmptyRoute,
    /// Another model is registered under this route already.
    /// (dsh: `DUPLICATE_ADAPTER`.)
    DuplicateRoute(String),
}

impl fmt::Display for RegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyRoute => write!(
                f,
                "the model route is empty; give it a name such as \"anthropic\" or \"deepseek\""
            ),
            Self::DuplicateRoute(route) => {
                write!(
                    f,
                    "a model is already registered under the route \"{route}\""
                )
            }
        }
    }
}

impl Error for RegistryError {}

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::sync::Arc;
    use std::task::{Context, Poll};
    use std::thread;

    use futures_core::Stream;

    use super::{ModelRegistry, RegistryError};
    use crate::{Event, EventStream, Model, ModelError, Request};

    /// A model that is never called. The tests only check which model a
    /// route gives back, by comparing pointers.
    struct Idle;

    impl Model for Idle {
        fn provider(&self) -> &str {
            "idle"
        }

        fn stream(&self, _request: Request) -> EventStream {
            Box::pin(Ended)
        }
    }

    /// A stream that has already ended.
    struct Ended;

    impl Stream for Ended {
        type Item = Result<Event, ModelError>;

        fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Poll::Ready(None)
        }
    }

    fn idle() -> Arc<dyn Model> {
        Arc::new(Idle)
    }

    #[test]
    fn a_registered_model_comes_back_by_its_route() {
        let registry = ModelRegistry::new();
        let anthropic = idle();
        let deepseek = idle();
        registry
            .register("anthropic", Arc::clone(&anthropic))
            .unwrap();
        registry
            .register("deepseek", Arc::clone(&deepseek))
            .unwrap();

        let found = registry.get("deepseek").expect("the route is registered");
        assert!(Arc::ptr_eq(&found, &deepseek));
        assert!(!Arc::ptr_eq(&found, &anthropic));
    }

    #[test]
    fn a_second_model_on_the_same_route_is_rejected() {
        let registry = ModelRegistry::new();
        let first = idle();
        registry.register("deepseek", Arc::clone(&first)).unwrap();

        let error = registry
            .register("deepseek", idle())
            .expect_err("the route is taken");
        assert_eq!(error, RegistryError::DuplicateRoute("deepseek".into()));
        assert_eq!(
            error.to_string(),
            "a model is already registered under the route \"deepseek\""
        );
        // The first model keeps the route.
        assert!(Arc::ptr_eq(&registry.get("deepseek").unwrap(), &first));
    }

    #[test]
    fn an_empty_route_is_rejected() {
        let registry = ModelRegistry::new();
        for route in ["", "  "] {
            let error = registry.register(route, idle()).expect_err("no name");
            assert_eq!(error, RegistryError::EmptyRoute, "{route:?}");
        }
        assert!(registry.routes().is_empty());
    }

    #[test]
    fn an_unknown_route_gives_none() {
        let registry = ModelRegistry::new();
        registry.register("anthropic", idle()).unwrap();
        assert!(registry.get("ollama").is_none());
    }

    #[test]
    fn routes_come_in_the_order_they_were_registered() {
        let registry = ModelRegistry::new();
        for route in ["ollama", "anthropic", "deepseek"] {
            registry.register(route, idle()).unwrap();
        }
        assert_eq!(registry.routes(), ["ollama", "anthropic", "deepseek"]);
    }

    #[test]
    fn debug_output_lists_the_routes() {
        let registry = ModelRegistry::new();
        registry.register("anthropic", idle()).unwrap();
        registry.register("deepseek", idle()).unwrap();
        assert_eq!(
            format!("{registry:?}"),
            r#"ModelRegistry { routes: ["anthropic", "deepseek"] }"#
        );
    }

    /// Compiles only if the registry can be shared between threads, as a
    /// port must be.
    #[test]
    fn the_registry_works_across_threads() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<ModelRegistry>();
    }

    #[test]
    fn many_threads_can_look_up_models_at_once() {
        let registry = ModelRegistry::new();
        let anthropic = idle();
        let deepseek = idle();
        registry
            .register("anthropic", Arc::clone(&anthropic))
            .unwrap();
        registry
            .register("deepseek", Arc::clone(&deepseek))
            .unwrap();

        // Scoped threads may borrow `registry`, and all of them have
        // finished when the scope ends.
        thread::scope(|scope| {
            for index in 0..8 {
                let (route, expected) = if index % 2 == 0 {
                    ("anthropic", &anthropic)
                } else {
                    ("deepseek", &deepseek)
                };
                let registry = &registry;
                scope.spawn(move || {
                    for _ in 0..100 {
                        let found = registry.get(route).expect("the route is registered");
                        assert!(Arc::ptr_eq(&found, expected));
                    }
                });
            }
        });
    }
}
