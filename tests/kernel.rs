use std::sync::Arc;

use karaka::{BoxError, Plugin, PluginContext, PortId, Runtime};

// Two ports. A port is a trait; plugins share implementations of it as
// `Arc<dyn Trait>`. `Send + Sync` lets one value serve many connections on
// many threads at once.
trait Clock: Send + Sync {
    fn now(&self) -> u64;
}

trait Greeter: Send + Sync {
    fn greet(&self) -> String;
}

struct FixedClock;

impl Clock for FixedClock {
    fn now(&self) -> u64 {
        42
    }
}

struct ClockGreeter {
    clock: Arc<dyn Clock>,
}

impl Greeter for ClockGreeter {
    fn greet(&self) -> String {
        format!("hello at {}", self.clock.now())
    }
}

/// Provides the clock.
struct ClockPlugin;

impl Plugin for ClockPlugin {
    fn name(&self) -> &str {
        "clock"
    }

    fn provides(&self) -> Vec<PortId> {
        vec![PortId::of::<dyn Clock>()]
    }

    fn setup(&self, ctx: &mut PluginContext) -> Result<(), BoxError> {
        ctx.provide::<dyn Clock>(Arc::new(FixedClock));
        Ok(())
    }
}

/// Provides the greeter, and needs the clock to do it.
struct GreeterPlugin;

impl Plugin for GreeterPlugin {
    fn name(&self) -> &str {
        "greeter"
    }

    fn provides(&self) -> Vec<PortId> {
        vec![PortId::of::<dyn Greeter>()]
    }

    fn requires(&self) -> Vec<PortId> {
        vec![PortId::of::<dyn Clock>()]
    }

    fn setup(&self, ctx: &mut PluginContext) -> Result<(), BoxError> {
        // No `?` here: the runtime checked before starting anything that a
        // plugin provides the clock, so this cannot fail.
        let clock = ctx.get::<dyn Clock>();
        ctx.provide::<dyn Greeter>(Arc::new(ClockGreeter { clock }));
        Ok(())
    }
}

#[test]
fn starts_providers_before_the_plugins_that_need_them() {
    // Listed in the wrong order on purpose.
    let runtime = Runtime::start(vec![Box::new(GreeterPlugin), Box::new(ClockPlugin)])
        .expect("the runtime starts");

    assert_eq!(runtime.plugins(), ["clock", "greeter"]);
    assert_eq!(runtime.get::<dyn Greeter>().greet(), "hello at 42");
}
