use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fmt;
use std::sync::Arc;

use crate::PortId;
use crate::plugin::{BoxError, Plugin, PluginContext, Ports};

/// A started set of plugins and the ports they provide.
///
/// The plugin list is fixed when the runtime starts: nothing is added,
/// removed or swapped while it runs.
pub struct Runtime {
    plugins: Vec<String>,
    ports: Ports,
}

impl Runtime {
    /// Checks the whole plugin list, then starts each plugin after the
    /// providers of every port it requires.
    ///
    /// Plugins that don't depend on each other start in the order listed. If
    /// the list has any problem, no plugin starts.
    pub fn start(plugins: Vec<Box<dyn Plugin>>) -> Result<Self, StartError> {
        let declared: Vec<Declared> = plugins.into_iter().map(Declared::new).collect();
        let order = start_order(&declared)?;

        let mut ports = Ports::default();
        for &index in &order {
            let plugin = &declared[index];
            let mut ctx =
                PluginContext::new(&plugin.name, &plugin.provides, &plugin.requires, &mut ports);
            plugin
                .plugin
                .setup(&mut ctx)
                .map_err(|source| StartError::Failed {
                    plugin: plugin.name.clone(),
                    source,
                })?;

            if let Some(&port) = plugin.provides.iter().find(|&&port| !ports.contains(port)) {
                return Err(StartError::NotProvided {
                    plugin: plugin.name.clone(),
                    port,
                });
            }
        }

        Ok(Self {
            plugins: order
                .iter()
                .map(|&index| declared[index].name.clone())
                .collect(),
            ports,
        })
    }

    /// The plugins' names, in the order they started.
    pub fn plugins(&self) -> Vec<&str> {
        self.plugins.iter().map(String::as_str).collect()
    }

    /// Reads a port.
    ///
    /// # Panics
    ///
    /// If no plugin provides it.
    pub fn get<T: ?Sized + Send + Sync + 'static>(&self) -> Arc<T> {
        self.ports
            .get::<T>()
            .unwrap_or_else(|| panic!("no plugin provides {:?}", PortId::of::<T>()))
    }
}

/// Why a runtime didn't start.
#[derive(Debug)]
pub enum StartError {
    /// The plugin list doesn't fit together. Every problem is listed, and no
    /// plugin was started.
    Invalid(Vec<String>),
    /// A plugin's setup returned an error.
    Failed { plugin: String, source: BoxError },
    /// A plugin's setup finished without providing a port it declared.
    NotProvided { plugin: String, port: PortId },
}

impl fmt::Display for StartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(problems) => {
                write!(f, "the plugin list doesn't fit together:")?;
                for problem in problems {
                    write!(f, "\n- {problem}")?;
                }
                Ok(())
            }
            Self::Failed { plugin, source } => {
                write!(f, "plugin \"{plugin}\" failed to start: {source}")
            }
            Self::NotProvided { plugin, port } => {
                write!(
                    f,
                    "plugin \"{plugin}\" finished setup without providing {port:?}"
                )
            }
        }
    }
}

impl Error for StartError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Failed { source, .. } => Some(source.as_ref()),
            _ => None,
        }
    }
}

/// A plugin with its declarations read once, so ordering and the checks in
/// `PluginContext` all see the same lists.
struct Declared {
    plugin: Box<dyn Plugin>,
    name: String,
    provides: Vec<PortId>,
    requires: Vec<PortId>,
}

impl Declared {
    fn new(plugin: Box<dyn Plugin>) -> Self {
        Self {
            name: plugin.name().to_owned(),
            provides: plugin.provides(),
            requires: plugin.requires(),
            plugin,
        }
    }
}

/// Indexes into `declared`, in an order where every plugin comes after the
/// providers of the ports it requires.
fn start_order(declared: &[Declared]) -> Result<Vec<usize>, StartError> {
    let mut problems = Vec::new();

    let mut names = HashSet::new();
    for plugin in declared {
        if !names.insert(plugin.name.as_str()) {
            problems.push(format!(
                "plugin \"{}\" is listed more than once",
                plugin.name
            ));
        }
    }

    let mut provider: HashMap<PortId, usize> = HashMap::new();
    for (index, plugin) in declared.iter().enumerate() {
        for &port in &plugin.provides {
            match provider.get(&port) {
                Some(&first) => problems.push(format!(
                    "{port:?} is provided by both \"{}\" and \"{}\"",
                    declared[first].name, plugin.name,
                )),
                None => {
                    provider.insert(port, index);
                }
            }
        }
    }

    for plugin in declared {
        for port in &plugin.requires {
            if !provider.contains_key(port) {
                problems.push(format!(
                    "plugin \"{}\" requires {port:?}, which no plugin provides",
                    plugin.name,
                ));
            }
        }
    }

    if !problems.is_empty() {
        return Err(StartError::Invalid(problems));
    }

    // Repeatedly take the first listed plugin whose providers have all been
    // placed. That keeps the listed order wherever dependencies allow it.
    let mut placed = vec![false; declared.len()];
    let mut order = Vec::with_capacity(declared.len());
    while order.len() < declared.len() {
        let ready = (0..declared.len()).find(|&index| {
            !placed[index]
                && declared[index]
                    .requires
                    .iter()
                    .all(|port| placed[provider[port]])
        });
        let Some(index) = ready else {
            let waiting: Vec<String> = declared
                .iter()
                .zip(&placed)
                .filter(|&(_, &placed)| !placed)
                .map(|(plugin, _)| format!("\"{}\"", plugin.name))
                .collect();
            return Err(StartError::Invalid(vec![format!(
                "these plugins depend on each other in a cycle, or on a plugin that does: {}",
                waiting.join(", "),
            )]));
        };
        placed[index] = true;
        order.push(index);
    }
    Ok(order)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{Runtime, StartError};
    use crate::{BoxError, Plugin, PluginContext, PortId};

    trait Alpha: Send + Sync {}
    trait Beta: Send + Sync {}

    struct Value;
    impl Alpha for Value {}
    impl Beta for Value {}

    type Setup = fn(&mut PluginContext<'_>) -> Result<(), BoxError>;

    /// A plugin built from its parts, so each test can declare just what it needs.
    struct TestPlugin {
        name: &'static str,
        provides: Vec<PortId>,
        requires: Vec<PortId>,
        setup: Setup,
    }

    impl Plugin for TestPlugin {
        fn name(&self) -> &str {
            self.name
        }
        fn provides(&self) -> Vec<PortId> {
            self.provides.clone()
        }
        fn requires(&self) -> Vec<PortId> {
            self.requires.clone()
        }
        fn setup(&self, ctx: &mut PluginContext<'_>) -> Result<(), BoxError> {
            (self.setup)(ctx)
        }
    }

    fn plugin(
        name: &'static str,
        provides: Vec<PortId>,
        requires: Vec<PortId>,
        setup: Setup,
    ) -> Box<dyn Plugin> {
        Box::new(TestPlugin {
            name,
            provides,
            requires,
            setup,
        })
    }

    fn alpha() -> PortId {
        PortId::of::<dyn Alpha>()
    }

    fn beta() -> PortId {
        PortId::of::<dyn Beta>()
    }

    fn must_not_start(_: &mut PluginContext<'_>) -> Result<(), BoxError> {
        panic!("a plugin started although the list was invalid")
    }

    fn provide_alpha(ctx: &mut PluginContext<'_>) -> Result<(), BoxError> {
        ctx.provide::<dyn Alpha>(Arc::new(Value));
        Ok(())
    }

    fn do_nothing(_: &mut PluginContext<'_>) -> Result<(), BoxError> {
        Ok(())
    }

    #[test]
    fn keeps_the_listed_order_between_independent_plugins() {
        let runtime = Runtime::start(vec![
            plugin("b", vec![], vec![], do_nothing),
            plugin("a", vec![], vec![], do_nothing),
            plugin("c", vec![], vec![], do_nothing),
        ])
        .expect("the runtime starts");
        assert_eq!(runtime.plugins(), ["b", "a", "c"]);
    }

    #[test]
    fn reports_every_problem_before_starting_any_plugin() {
        let Err(StartError::Invalid(problems)) = Runtime::start(vec![
            plugin("same", vec![alpha()], vec![], must_not_start),
            plugin("same", vec![alpha()], vec![], must_not_start),
            plugin("needy", vec![], vec![beta()], must_not_start),
        ]) else {
            panic!("expected an invalid plugin list");
        };
        assert_eq!(problems.len(), 3, "{problems:#?}");
        assert!(problems[0].contains("\"same\" is listed more than once"));
        assert!(problems[1].contains("is provided by both \"same\" and \"same\""));
        assert!(problems[2].contains("\"needy\" requires port"));
    }

    #[test]
    fn names_the_plugins_caught_in_a_cycle() {
        let Err(StartError::Invalid(problems)) = Runtime::start(vec![
            plugin("first", vec![alpha()], vec![beta()], must_not_start),
            plugin("second", vec![beta()], vec![alpha()], must_not_start),
        ]) else {
            panic!("expected an invalid plugin list");
        };
        assert!(problems[0].contains("cycle"), "{problems:#?}");
        assert!(
            problems[0].ends_with("\"first\", \"second\""),
            "{problems:#?}"
        );
    }

    #[test]
    fn reports_which_plugin_failed_to_start() {
        let Err(error) = Runtime::start(vec![plugin("broken", vec![], vec![], |_| {
            Err("no database".into())
        })]) else {
            panic!("expected a failed start");
        };
        assert_eq!(
            error.to_string(),
            "plugin \"broken\" failed to start: no database"
        );
    }

    #[test]
    fn rejects_a_plugin_that_forgets_a_declared_port() {
        let Err(StartError::NotProvided { plugin: name, port }) =
            Runtime::start(vec![plugin("forgetful", vec![alpha()], vec![], do_nothing)])
        else {
            panic!("expected a missing port");
        };
        assert_eq!((name.as_str(), port), ("forgetful", alpha()));
    }

    #[test]
    fn a_started_runtime_hands_out_provided_ports() {
        let runtime = Runtime::start(vec![plugin("alpha", vec![alpha()], vec![], provide_alpha)])
            .expect("the runtime starts");
        let _value: Arc<dyn Alpha> = runtime.get::<dyn Alpha>();
    }
}
