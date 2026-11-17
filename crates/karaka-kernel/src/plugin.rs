use std::any::Any;
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::error::Error;
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
    /// it declared.
    fn setup(&self, ctx: &mut PluginContext<'_>) -> Result<(), BoxError>;
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
