use std::any::Any;
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::error::Error;
use std::pin::Pin;
use std::sync::Arc;

use crate::PortId;

/// Any error a plugin's setup can fail with, such as an unreachable database.
pub type BoxError = Box<dyn Error + Send + Sync>;

/// One piece of the runtime: it provides some ports and uses others.
///
/// Before starting anything, the runtime reads every plugin's `provides` and
/// `requires`, checks that each required port has exactly one provider, and
/// orders the plugins so that providers start before the plugins that need
/// them.
pub trait Plugin: Send + Sync {
    /// A name unique within the runtime, used in error messages.
    fn name(&self) -> &str;

    /// The ports this plugin provides. Its `setup` must provide each one.
    fn provides(&self) -> Vec<PortId> {
        Vec::new()
    }

    /// The ports this plugin reads during `setup`.
    fn requires(&self) -> Vec<PortId> {
        Vec::new()
    }

    /// Starts the plugin: reads the ports it requires and provides the ports
    /// it declared. Implement it as `async fn setup(...)`.
    ///
    /// The returned future must be `Send`, so a multi-threaded executor can
    /// run it on any thread. In practice: don't hold an `Rc` or a `RefCell`
    /// borrow across an `.await`.
    fn setup(
        &self,
        ctx: &mut PluginContext<'_>,
    ) -> impl Future<Output = Result<(), BoxError>> + Send;
}

/// A [`Plugin`] of any type, so plugins of different types can share one list
/// as `Box<dyn DynPlugin>`.
///
/// Don't implement this yourself: every `Plugin` already is one.
///
/// It exists because a trait with an async method can't be used as `dyn
/// Trait`. Each plugin's `setup` returns a future of a different type, and a
/// `dyn` call needs one type. This trait boxes the future, so it always has the
/// same type: a pointer to some future.
pub trait DynPlugin: Send + Sync {
    /// See [`Plugin::name`].
    fn name(&self) -> &str;

    /// See [`Plugin::provides`].
    fn provides(&self) -> Vec<PortId>;

    /// See [`Plugin::requires`].
    fn requires(&self) -> Vec<PortId>;

    /// [`Plugin::setup`], with its future boxed.
    fn setup_boxed<'a>(
        &'a self,
        ctx: &'a mut PluginContext<'_>,
    ) -> Pin<Box<dyn Future<Output = Result<(), BoxError>> + Send + 'a>>;
}

impl<P: Plugin> DynPlugin for P {
    fn name(&self) -> &str {
        Plugin::name(self)
    }

    fn provides(&self) -> Vec<PortId> {
        Plugin::provides(self)
    }

    fn requires(&self) -> Vec<PortId> {
        Plugin::requires(self)
    }

    fn setup_boxed<'a>(
        &'a self,
        ctx: &'a mut PluginContext<'_>,
    ) -> Pin<Box<dyn Future<Output = Result<(), BoxError>> + Send + 'a>> {
        Box::pin(Plugin::setup(self, ctx))
    }
}

/// The values plugins have provided, at most one per port.
///
/// One map holds values of many types, so each `Arc<T>` is stored as
/// `Box<dyn Any>` and turned back into `Arc<T>` when read. Keying by the
/// port's type makes that conversion always succeed.
#[derive(Default)]
pub(crate) struct Ports {
    values: HashMap<PortId, Box<dyn Any + Send + Sync>>,
}

impl Ports {
    /// Stores the value for `T`'s port. Returns false, storing nothing, if that
    /// port already has a value.
    fn insert<T: ?Sized + Send + Sync + 'static>(&mut self, value: Arc<T>) -> bool {
        match self.values.entry(PortId::of::<T>()) {
            Entry::Occupied(_) => false,
            Entry::Vacant(slot) => {
                slot.insert(Box::new(value));
                true
            }
        }
    }

    pub(crate) fn get<T: ?Sized + Send + Sync + 'static>(&self) -> Option<Arc<T>> {
        let value = self.values.get(&PortId::of::<T>())?;
        let value = value
            .downcast_ref::<Arc<T>>()
            .expect("a port's value always has the port's type");
        Some(Arc::clone(value))
    }

    pub(crate) fn contains(&self, port: PortId) -> bool {
        self.values.contains_key(&port)
    }
}

/// What a plugin sees while it starts: the ports it may read and provide.
///
/// It borrows the runtime's ports for the length of one `setup` call, and holds
/// the plugin to what it declared in `provides` and `requires`.
pub struct PluginContext<'a> {
    plugin: &'a str,
    provides: &'a [PortId],
    requires: &'a [PortId],
    ports: &'a mut Ports,
}

impl<'a> PluginContext<'a> {
    pub(crate) fn new(
        plugin: &'a str,
        provides: &'a [PortId],
        requires: &'a [PortId],
        ports: &'a mut Ports,
    ) -> Self {
        Self {
            plugin,
            provides,
            requires,
            ports,
        }
    }

    /// Reads a port this plugin listed in `requires`.
    ///
    /// # Panics
    ///
    /// If the port isn't listed in `requires`. That is a bug in the plugin, not
    /// something to handle at run time: the runtime has already checked that
    /// every required port has a provider, and started that provider first.
    pub fn get<T: ?Sized + Send + Sync + 'static>(&self) -> Arc<T> {
        let port = PortId::of::<T>();
        assert!(
            self.requires.contains(&port),
            "plugin \"{}\" reads {port:?} without listing it in requires()",
            self.plugin,
        );
        self.ports.get::<T>().unwrap_or_else(|| {
            panic!(
                "{port:?} has no value, though plugin \"{}\" requires it; its provider should have started first",
                self.plugin,
            )
        })
    }

    /// Provides a port this plugin listed in `provides`.
    ///
    /// # Panics
    ///
    /// If the port isn't listed in `provides`, or this plugin already provided
    /// it.
    pub fn provide<T: ?Sized + Send + Sync + 'static>(&mut self, value: Arc<T>) {
        let port = PortId::of::<T>();
        assert!(
            self.provides.contains(&port),
            "plugin \"{}\" provides {port:?} without listing it in provides()",
            self.plugin,
        );
        assert!(
            self.ports.insert(value),
            "plugin \"{}\" provides {port:?} twice",
            self.plugin,
        );
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{PluginContext, Ports};
    use crate::PortId;

    trait Clock: Send + Sync {
        fn now(&self) -> u64;
    }

    struct FixedClock;

    impl Clock for FixedClock {
        fn now(&self) -> u64 {
            42
        }
    }

    #[test]
    fn a_provided_port_reads_back_in_another_plugin() {
        let clock = [PortId::of::<dyn Clock>()];
        let mut ports = Ports::default();

        PluginContext::new("clock", &clock, &[], &mut ports)
            .provide::<dyn Clock>(Arc::new(FixedClock));
        let greeter = PluginContext::new("greeter", &[], &clock, &mut ports);

        assert_eq!(greeter.get::<dyn Clock>().now(), 42);
    }

    #[test]
    #[should_panic(expected = "without listing it in requires()")]
    fn reading_an_undeclared_port_panics() {
        let mut ports = Ports::default();
        PluginContext::new("reader", &[], &[], &mut ports).get::<dyn Clock>();
    }

    #[test]
    #[should_panic(expected = "without listing it in provides()")]
    fn providing_an_undeclared_port_panics() {
        let mut ports = Ports::default();
        PluginContext::new("provider", &[], &[], &mut ports)
            .provide::<dyn Clock>(Arc::new(FixedClock));
    }

    #[test]
    #[should_panic(expected = "twice")]
    fn providing_a_port_twice_panics() {
        let clock = [PortId::of::<dyn Clock>()];
        let mut ports = Ports::default();
        let mut ctx = PluginContext::new("clock", &clock, &[], &mut ports);
        ctx.provide::<dyn Clock>(Arc::new(FixedClock));
        ctx.provide::<dyn Clock>(Arc::new(FixedClock));
    }
}
