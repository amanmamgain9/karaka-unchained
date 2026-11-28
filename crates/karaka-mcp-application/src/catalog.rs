//! The application catalog: which endpoints are expected, which tool names
//! they contribute, and whether every one has settled its first discovery.
//! (Karaka: `mcp-application/src/catalog.ts`, `ApplicationCatalog`.)

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

/// Owns the endpoints' contributions, and says once every expected endpoint
/// has settled. (Karaka: `karakaApplicationCatalog`; the same object
/// published again as `karakaApplicationTools` is [`tools`](Self::tools).)
pub struct ApplicationCatalog {
    servers: Vec<String>,
    state: Mutex<State>,
    me: Weak<ApplicationCatalog>,
}

#[derive(Default)]
struct State {
    /// Each owned endpoint's record, and whether it has settled.
    endpoints: HashMap<String, (u64, bool)>,
    /// Each contributed name, and how many contribute it.
    tools: BTreeMap<String, usize>,
    /// Latched: readiness persists while the catalog lives.
    published: bool,
    next: u64,
}

impl ApplicationCatalog {
    /// A catalog expecting `servers`; with none it is ready at once.
    ///
    /// # Errors
    ///
    /// A server named twice, in Karaka's words.
    pub fn new(servers: Vec<String>) -> Result<Arc<Self>, String> {
        let unique: BTreeSet<&String> = servers.iter().collect();
        if unique.len() != servers.len() {
            return Err("Application catalog server names must be unique".into());
        }
        let catalog = Arc::new_cyclic(|me| Self {
            servers,
            state: Mutex::new(State::default()),
            me: me.clone(),
        });
        catalog.publish(&mut catalog.lock());
        Ok(catalog)
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The servers it expects.
    pub fn servers(&self) -> &[String] {
        &self.servers
    }

    /// Every name contributed now, detached from the catalog. (Karaka:
    /// `names`.)
    pub fn names(&self) -> BTreeSet<String> {
        self.lock().tools.keys().cloned().collect()
    }

    /// The names, once every expected endpoint has settled; `None` before.
    /// What a preset's policy reads, as Karaka's waits on
    /// `karakaApplicationTools`.
    pub fn tools(&self) -> Option<BTreeSet<String>> {
        let state = self.lock();
        state
            .published
            .then(|| state.tools.keys().cloned().collect())
    }

    /// The expected endpoints not settled yet.
    pub fn pending(&self) -> Vec<String> {
        let state = self.lock();
        self.servers
            .iter()
            .filter(|server| {
                !state
                    .endpoints
                    .get(*server)
                    .is_some_and(|(_, ready)| *ready)
            })
            .cloned()
            .collect()
    }

    /// Contributes `name`; contributions of one name are counted. (Karaka:
    /// `registerTool`.)
    pub fn register_tool(&self, name: &str) -> ToolContribution {
        *self.lock().tools.entry(name.to_owned()).or_default() += 1;
        ToolContribution {
            catalog: self.me.clone(),
            name: name.to_owned(),
        }
    }

    /// Claims one expected endpoint's discovery. (Karaka: `endpoint`.)
    ///
    /// # Errors
    ///
    /// In Karaka's words: a server the catalog doesn't expect, or one
    /// already owned.
    pub fn endpoint(&self, server: &str) -> Result<CatalogEndpoint, String> {
        if !self.servers.iter().any(|known| known == server) {
            return Err(format!(
                "Application catalog does not configure server \"{server}\""
            ));
        }
        let mut state = self.lock();
        if state.endpoints.contains_key(server) {
            return Err(format!(
                "Application catalog server \"{server}\" already has an owner"
            ));
        }
        state.next += 1;
        let record = state.next;
        state.endpoints.insert(server.to_owned(), (record, false));
        Ok(CatalogEndpoint {
            catalog: self.me.clone(),
            server: server.to_owned(),
            record,
        })
    }

    fn publish(&self, state: &mut State) {
        if state.published || state.endpoints.len() != self.servers.len() {
            return;
        }
        if state.endpoints.values().any(|(_, ready)| !ready) {
            return;
        }
        state.published = true;
    }
}

impl fmt::Debug for ApplicationCatalog {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.lock();
        f.debug_struct("ApplicationCatalog")
            .field("servers", &self.servers)
            .field("tools", &state.tools)
            .field("published", &state.published)
            .finish()
    }
}

/// One contribution of a name, until removed.
#[derive(Debug)]
#[must_use = "the name stays contributed until removed"]
pub struct ToolContribution {
    catalog: Weak<ApplicationCatalog>,
    name: String,
}

impl ToolContribution {
    pub fn remove(self) {
        let Some(catalog) = self.catalog.upgrade() else {
            return;
        };
        let mut state = catalog.lock();
        if let Some(count) = state.tools.get_mut(&self.name) {
            *count -= 1;
            if *count == 0 {
                state.tools.remove(&self.name);
            }
        }
    }
}

/// One endpoint's claim on its server. A stale claim's calls do nothing.
#[derive(Debug)]
pub struct CatalogEndpoint {
    catalog: Weak<ApplicationCatalog>,
    server: String,
    record: u64,
}

impl CatalogEndpoint {
    /// Its first discovery has settled. (Karaka: `settle`.)
    pub fn settle(&self) {
        let Some(catalog) = self.catalog.upgrade() else {
            return;
        };
        let mut state = catalog.lock();
        match state.endpoints.get_mut(&self.server) {
            Some((record, ready)) if *record == self.record => *ready = true,
            _ => return,
        }
        catalog.publish(&mut state);
    }

    /// Gives the server up, so another endpoint can own it. Readiness
    /// already published stays. (Karaka: `dispose`.)
    pub fn dispose(&self) {
        let Some(catalog) = self.catalog.upgrade() else {
            return;
        };
        let mut state = catalog.lock();
        if state
            .endpoints
            .get(&self.server)
            .is_some_and(|(record, _)| *record == self.record)
        {
            state.endpoints.remove(&self.server);
        }
    }
}
