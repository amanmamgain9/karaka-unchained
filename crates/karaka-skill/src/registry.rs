//! The registry: providers and runtime skills, layered by scope, merged
//! into one catalog per lookup, and cached. (dsh: `SkillRegistry` in
//! `skill/src/index.ts`.)
//!
//! # Layers
//!
//! A registration goes in the global layer, or in one agent's, by its
//! session's id: dsh files it in the layer of its calling context's scope,
//! where an agent preset's standing mount is a scope of its own. Karaka has
//! no presets, so a scope is a session and has no parent; a read merges the
//! global layer with the viewing scope's. Within a layer, the lowest rank
//! wins a duplicate name, then the earliest registration, then the
//! provider's own order. Across layers, the scope's entry replaces the
//! global one outright, whatever their ranks.
//!
//! # The cache
//!
//! A complete catalog is kept per `(cwd, scope, revision)`, at most
//! `collect_cache_max_entries` of them, oldest out first. Every change to
//! a layer, and a provider's own invalidation, bumps the revision. A read
//! whose revision changed while it ran tries once more, and if it changed
//! again, gives that result uncached. A provider that failed, or reported
//! incomplete discovery, makes the catalog incomplete and uncached.
//!
//! # Warnings
//!
//! dsh logs a warning when it ignores a duplicate or skips a failing
//! provider. Karaka has no logger, so the registry keeps its most recent
//! warnings, for [`SkillRegistry::warnings`].

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use karaka_fs::{AbortController, AbortSignal};
use karaka_session::SessionId;

use crate::error::SkillError;
use crate::types::{
    RUNTIME_PROVIDER, RUNTIME_RANK, SkillCandidate, SkillCatalogSnapshot, SkillDefinition,
    SkillInvocationPolicy, SkillLookupOptions, SkillProviderObservation, SkillRegistration,
    SkillSummary, is_skill_name,
};

/// The default of [`SkillConfig::collect_cache_max_entries`].
pub const DEFAULT_COLLECT_CACHE_ENTRIES: usize = 128;

/// How many times a read collects before it gives an uncached result.
const MAX_COLLECT_ATTEMPTS: u32 = 2;

/// How many warnings the registry keeps.
const KEPT_WARNINGS: usize = 256;

/// Why a provider failed.
pub type ProviderError = Box<dyn std::error::Error + Send + Sync>;

/// What a provider's `list` resolves to.
pub type ListFuture<'a> =
    Pin<Box<dyn Future<Output = Result<SkillProviderObservation, ProviderError>> + Send + 'a>>;

/// What a provider's `get` resolves to.
pub type GetFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Option<SkillDefinition>, ProviderError>> + Send + 'a>>;

/// One source of skills, such as local directories or a remote registry.
/// (dsh: `SkillProvider`.)
pub trait SkillProvider: Send + Sync {
    /// Unique in its layer; `runtime` is reserved.
    fn name(&self) -> &str;

    /// The candidates for this lookup. Remote setup and discovery happen
    /// here, and should end promptly when `options.signal` fires.
    fn list<'a>(&'a self, options: &'a SkillLookupOptions) -> ListFuture<'a>;

    /// The whole skill for a candidate this provider listed, or `None` if
    /// it no longer loads.
    fn get<'a>(
        &'a self,
        candidate: &'a SkillCandidate,
        options: &'a SkillLookupOptions,
    ) -> GetFuture<'a>;
}

/// What a provider registration lends its provider. (dsh:
/// `SkillProviderControl`.)
#[derive(Clone)]
pub struct SkillProviderControl {
    /// Fires when the registration is refused or removed.
    pub signal: AbortSignal,
    shared: Weak<Shared>,
    scope: Option<SessionId>,
    id: u64,
}

impl SkillProviderControl {
    /// Keeps `message` among the registry's warnings, where dsh's provider
    /// would log it: Karaka has no logger yet. (Karaka's own: dsh's
    /// providers log through their context.)
    pub fn warn(&self, message: impl Into<String>) {
        if let Some(shared) = self.shared.upgrade() {
            shared.warn(message.into());
        }
    }

    /// Drops the cached catalogs and tells the change listeners, while this
    /// exact registration is live; afterwards it does nothing.
    pub fn invalidate(&self) {
        if let Some(shared) = self.shared.upgrade()
            && shared.is_live(self.scope.as_ref(), self.id)
        {
            shared.invalidate();
        }
    }
}

impl std::fmt::Debug for SkillProviderControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SkillProviderControl")
            .field("scope", &self.scope)
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

/// The registry's settings. (dsh: `Config`.)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SkillConfig {
    /// The most complete catalogs kept. At least 1.
    pub collect_cache_max_entries: usize,
}

impl Default for SkillConfig {
    fn default() -> Self {
        Self {
            collect_cache_max_entries: DEFAULT_COLLECT_CACHE_ENTRIES,
        }
    }
}

/// A provider in the registry, which [`remove`](Self::remove) takes out.
/// Dropping it leaves the provider in.
#[must_use = "dropping a registration keeps the provider; call `remove` to take it out"]
pub struct ProviderRegistration {
    shared: Weak<Shared>,
    scope: Option<SessionId>,
    id: u64,
}

impl ProviderRegistration {
    /// Takes the provider out, fires its control's signal, and invalidates
    /// the catalogs. (dsh: the registration's disposer.)
    pub fn remove(self) {
        if let Some(shared) = self.shared.upgrade() {
            shared.remove_provider(self.scope.as_ref(), self.id);
        }
    }
}

/// A runtime skill in the registry, which [`remove`](Self::remove) takes
/// out. A duplicate's registration removes nothing.
#[must_use = "dropping a registration keeps the skill; call `remove` to take it out"]
pub struct SkillHandle {
    shared: Weak<Shared>,
    scope: Option<SessionId>,
    /// `None` for an ignored duplicate.
    entry: Option<(String, u64)>,
}

impl SkillHandle {
    /// Takes the skill out and invalidates the catalogs. (dsh: the
    /// registration's disposer.)
    pub fn remove(self) {
        if let (Some(shared), Some((name, id))) = (self.shared.upgrade(), self.entry) {
            shared.remove_runtime(self.scope.as_ref(), &name, id);
        }
    }
}

/// A listener of catalog changes, which [`remove`](Self::remove) takes out.
#[must_use = "dropping a registration keeps the listener; call `remove` to take it out"]
pub struct ChangeRegistration {
    shared: Weak<Shared>,
    id: u64,
}

impl ChangeRegistration {
    pub fn remove(self) {
        if let Some(shared) = self.shared.upgrade() {
            shared.lock().listeners.retain(|(id, _)| *id != self.id);
        }
    }
}

/// A catalog-change listener. (dsh: a `skills/change` listener.)
pub type ChangeListener = Arc<dyn Fn() + Send + Sync>;

/// The skill registry. (dsh: `ctx.skills`.)
pub struct SkillRegistry {
    shared: Arc<Shared>,
}

struct Shared {
    max_entries: usize,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    global: Layer,
    scoped: HashMap<SessionId, Layer>,
    revision: u64,
    next_id: u64,
    next_order: u64,
    cache: VecDeque<(CacheKey, Arc<Entries>)>,
    listeners: Vec<(u64, ChangeListener)>,
    warnings: VecDeque<String>,
}

#[derive(Clone, Default)]
struct Layer {
    /// In registration order.
    providers: Vec<RegisteredProvider>,
    /// By name, so they list in code-point order.
    runtime: BTreeMap<String, (u64, Arc<SkillDefinition>)>,
}

impl Layer {
    fn is_empty(&self) -> bool {
        self.providers.is_empty() && self.runtime.is_empty()
    }
}

#[derive(Clone)]
struct RegisteredProvider {
    id: u64,
    provider: Arc<dyn SkillProvider>,
    /// Registry-wide registration order, the tie-break after rank.
    order: u64,
    lifecycle: Arc<AbortController>,
}

#[derive(Clone, PartialEq, Eq)]
struct CacheKey {
    cwd: Option<String>,
    scope: Option<SessionId>,
    revision: u64,
}

/// The winning entries, by name.
type Entries = BTreeMap<String, Indexed>;

#[derive(Clone)]
struct Indexed {
    candidate: SkillCandidate,
    /// `None` for a runtime skill, whose body is its locator.
    provider: Option<(u64, Arc<dyn SkillProvider>)>,
    layer: Option<SessionId>,
    provider_order: i128,
    local_order: u64,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        // Every change is a push, removal or counter step, so the state is
        // whole even after a panic.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn warn(&self, message: String) {
        let mut state = self.lock();
        if state.warnings.len() == KEPT_WARNINGS {
            state.warnings.pop_front();
        }
        state.warnings.push_back(message);
    }

    fn is_live(&self, scope: Option<&SessionId>, id: u64) -> bool {
        let state = self.lock();
        layer(&state, scope).is_some_and(|layer| layer.providers.iter().any(|entry| entry.id == id))
    }

    /// Bumps the revision, drops the cache, and tells the listeners, with
    /// no lock held. (dsh: `invalidateCache`.)
    fn invalidate(&self) {
        let listeners: Vec<ChangeListener> = {
            let mut state = self.lock();
            state.revision += 1;
            state.cache.clear();
            state
                .listeners
                .iter()
                .map(|(_, listener)| Arc::clone(listener))
                .collect()
        };
        for listener in listeners {
            // A listener can't veto the change: its panic is contained.
            // (dsh: `notifyChange`.)
            if let Err(panic) = catch_unwind(AssertUnwindSafe(|| listener())) {
                self.warn(format!(
                    "skills/change listener threw: Error: {}",
                    panic_message(panic.as_ref())
                ));
            }
        }
    }

    fn remove_provider(&self, scope: Option<&SessionId>, id: u64) {
        let removed = {
            let mut state = self.lock();
            let Some(layer) = layer_mut(&mut state, scope) else {
                return;
            };
            let Some(index) = layer.providers.iter().position(|entry| entry.id == id) else {
                return;
            };
            let removed = layer.providers.remove(index);
            prune(&mut state, scope);
            removed
        };
        removed.lifecycle.abort();
        self.invalidate();
    }

    fn remove_runtime(&self, scope: Option<&SessionId>, name: &str, id: u64) {
        {
            let mut state = self.lock();
            let Some(layer) = layer_mut(&mut state, scope) else {
                return;
            };
            if layer.runtime.get(name).is_none_or(|(held, _)| *held != id) {
                return;
            }
            layer.runtime.remove(name);
            prune(&mut state, scope);
        }
        self.invalidate();
    }
}

fn layer<'a>(state: &'a State, scope: Option<&SessionId>) -> Option<&'a Layer> {
    match scope {
        None => Some(&state.global),
        Some(scope) => state.scoped.get(scope),
    }
}

fn layer_mut<'a>(state: &'a mut State, scope: Option<&SessionId>) -> Option<&'a mut Layer> {
    match scope {
        None => Some(&mut state.global),
        Some(scope) => state.scoped.get_mut(scope),
    }
}

/// Forgets an empty scoped layer. (dsh: `ScopedLayers` drops one.)
fn prune(state: &mut State, scope: Option<&SessionId>) {
    if let Some(scope) = scope
        && state.scoped.get(scope).is_some_and(Layer::is_empty)
    {
        state.scoped.remove(scope);
    }
}

fn panic_message(panic: &(dyn std::any::Any + Send)) -> String {
    panic
        .downcast_ref::<&str>()
        .map(|text| (*text).to_owned())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "[unrenderable thrown value]".to_owned())
}

/// Awaits `work`, unless `signal` fires first. (dsh: `waitWithAbort`.)
async fn with_abort<T>(
    work: impl Future<Output = T>,
    signal: Option<&AbortSignal>,
) -> Result<T, SkillError> {
    match signal {
        None => Ok(work.await),
        Some(signal) => {
            check_abort(Some(signal))?;
            tokio::select! {
                biased;
                () = signal.wait() => Err(SkillError::Aborted),
                value = work => Ok(value),
            }
        }
    }
}

fn check_abort(signal: Option<&AbortSignal>) -> Result<(), SkillError> {
    if signal.is_some_and(AbortSignal::is_aborted) {
        return Err(SkillError::Aborted);
    }
    Ok(())
}

impl SkillRegistry {
    /// An empty registry.
    ///
    /// # Errors
    ///
    /// A cache of no entries, with dsh's message.
    pub fn new(config: SkillConfig) -> Result<Self, SkillError> {
        if config.collect_cache_max_entries < 1 {
            return Err(SkillError::Config(
                "skill: collectCacheMaxEntries must be an integer greater than or equal to 1"
                    .into(),
            ));
        }
        Ok(Self {
            shared: Arc::new(Shared {
                max_entries: config.collect_cache_max_entries,
                state: Mutex::new(State::default()),
            }),
        })
    }

    /// Registers the provider `create` makes, given its control, in the
    /// global layer, or in the layer of the agent whose session is `scope`.
    /// (dsh: `registerProvider`, filed by the calling context's scope.)
    ///
    /// # Errors
    ///
    /// The reserved name `runtime`, or a name already registered in that
    /// layer; the control's signal fires either way.
    pub fn register_provider(
        &self,
        scope: Option<&SessionId>,
        create: impl FnOnce(SkillProviderControl) -> Arc<dyn SkillProvider>,
    ) -> Result<ProviderRegistration, SkillError> {
        let lifecycle = Arc::new(AbortController::new());
        let id = {
            let mut state = self.shared.lock();
            state.next_id += 1;
            state.next_id
        };
        let control = SkillProviderControl {
            signal: lifecycle.signal(),
            shared: Arc::downgrade(&self.shared),
            scope: scope.cloned(),
            id,
        };
        let provider = create(control);
        let name = provider.name().to_owned();
        if name == RUNTIME_PROVIDER {
            lifecycle.abort();
            return Err(SkillError::Provider(format!(
                "\"{RUNTIME_PROVIDER}\" is reserved for runtime skill registrations"
            )));
        }
        {
            let mut state = self.shared.lock();
            let order = state.next_order;
            let layer = match scope {
                None => &mut state.global,
                Some(scope) => state.scoped.entry(scope.clone()).or_default(),
            };
            if layer
                .providers
                .iter()
                .any(|entry| entry.provider.name() == name)
            {
                prune(&mut state, scope);
                drop(state);
                lifecycle.abort();
                return Err(SkillError::Provider(match scope {
                    None => format!("a skill provider named \"{name}\" is already registered"),
                    Some(_) => format!(
                        "a skill provider named \"{name}\" is already registered in this scope"
                    ),
                }));
            }
            layer.providers.push(RegisteredProvider {
                id,
                provider,
                order,
                lifecycle,
            });
            state.next_order += 1;
        }
        self.shared.invalidate();
        Ok(ProviderRegistration {
            shared: Arc::downgrade(&self.shared),
            scope: scope.cloned(),
            id,
        })
    }

    /// Registers a runtime skill, globally or for the agent whose session
    /// is `scope`. Within a layer, the first of a name wins: a duplicate is
    /// warned of, ignored, and gets a registration that removes nothing.
    /// (dsh: `register`.)
    ///
    /// # Errors
    ///
    /// An invalid name, or an empty description.
    pub fn register(
        &self,
        scope: Option<&SessionId>,
        skill: SkillRegistration,
    ) -> Result<SkillHandle, SkillError> {
        if !is_skill_name(&skill.name) {
            return Err(SkillError::Invalid(format!(
                "invalid skill name \"{}\"",
                skill.name
            )));
        }
        if skill.description.is_empty() {
            return Err(SkillError::Invalid(format!(
                "skill \"{}\" requires a description",
                skill.name
            )));
        }
        let definition = SkillDefinition {
            path: skill.path,
            name: skill.name,
            description: skill.description,
            when_to_use: skill.when_to_use,
            invocation: skill.invocation.unwrap_or(SkillInvocationPolicy::BOTH),
            source: skill.source,
            provider: skill
                .provider
                .unwrap_or_else(|| RUNTIME_PROVIDER.to_owned()),
            resource_base: skill.resource_base,
            content: skill.content,
            metadata: skill.metadata,
        };
        let name = definition.name.clone();
        let id = {
            let mut state = self.shared.lock();
            if layer(&state, scope).is_some_and(|layer| layer.runtime.contains_key(&name)) {
                drop(state);
                self.shared.warn(format!(
                    "runtime skill \"{name}\" ignored because it is already registered"
                ));
                return Ok(SkillHandle {
                    shared: Arc::downgrade(&self.shared),
                    scope: scope.cloned(),
                    entry: None,
                });
            }
            state.next_id += 1;
            let id = state.next_id;
            let layer = match scope {
                None => &mut state.global,
                Some(scope) => state.scoped.entry(scope.clone()).or_default(),
            };
            layer
                .runtime
                .insert(name.clone(), (id, Arc::new(definition)));
            id
        };
        self.shared.invalidate();
        Ok(SkillHandle {
            shared: Arc::downgrade(&self.shared),
            scope: scope.cloned(),
            entry: Some((name, id)),
        })
    }

    /// Calls `listener` after every change to the catalogs: a registration,
    /// a removal, or a provider's invalidation. A listener that panics is
    /// warned of, and can't stop the change. (dsh: `skills/change`.)
    pub fn on_change(&self, listener: ChangeListener) -> ChangeRegistration {
        let mut state = self.shared.lock();
        state.next_id += 1;
        let id = state.next_id;
        state.listeners.push((id, listener));
        ChangeRegistration {
            shared: Arc::downgrade(&self.shared),
            id,
        }
    }

    /// The winning summaries for this lookup, sorted by name. (dsh: `list`.)
    ///
    /// # Errors
    ///
    /// As [`snapshot`](Self::snapshot).
    pub async fn list(
        &self,
        options: &SkillLookupOptions,
    ) -> Result<Vec<SkillSummary>, SkillError> {
        Ok(self.snapshot(options).await?.skills)
    }

    /// The winning summaries for this lookup, and whether discovery
    /// completed with no change meanwhile. An incomplete catalog isn't
    /// cached. (dsh: `snapshot`.)
    ///
    /// # Errors
    ///
    /// The lookup's signal fired, or a provider listed a candidate that
    /// breaks the contract. A provider that fails is skipped, with a
    /// warning.
    pub async fn snapshot(
        &self,
        options: &SkillLookupOptions,
    ) -> Result<SkillCatalogSnapshot, SkillError> {
        let (entries, complete) = self.collect(options).await?;
        Ok(SkillCatalogSnapshot {
            skills: entries
                .values()
                .map(|entry| entry.candidate.summary())
                .collect(),
            complete,
        })
    }

    /// Loads the winning skill named `name`, from the provider that listed
    /// it, and checks it. `None` for an invalid name, a name nothing lists,
    /// a body that no longer loads, or one whose name changed, which also
    /// invalidates the catalogs. (dsh: `get`.)
    ///
    /// # Errors
    ///
    /// The lookup's signal fired, before or during loading; the provider
    /// failed to load it; or the loaded skill breaks the contract.
    pub async fn get(
        &self,
        name: &str,
        options: &SkillLookupOptions,
    ) -> Result<Option<SkillDefinition>, SkillError> {
        if !is_skill_name(name) {
            return Ok(None);
        }
        let (entries, _) = self.collect(options).await?;
        check_abort(options.signal.as_ref())?;
        let Some(found) = entries.get(name) else {
            return Ok(None);
        };
        let definition = match &found.provider {
            None => found
                .candidate
                .locator
                .downcast_ref::<SkillDefinition>()
                .cloned(),
            Some((_, provider)) => with_abort(
                provider.get(&found.candidate, options),
                options.signal.as_ref(),
            )
            .await?
            .map_err(|error| SkillError::Load(error.to_string()))?,
        };
        let Some(definition) = definition else {
            return Ok(None);
        };
        validate_definition(&definition)?;
        if definition.name != found.candidate.name {
            // A stale entry: only the exact registration that listed it may
            // invalidate.
            if let Some((id, _)) = &found.provider
                && self.shared.is_live(found.layer.as_ref(), *id)
            {
                self.shared.invalidate();
            }
            return Ok(None);
        }
        Ok(Some(definition))
    }

    /// The most recent warnings, oldest first: what dsh logs.
    pub fn warnings(&self) -> Vec<String> {
        self.shared.lock().warnings.iter().cloned().collect()
    }

    /// (dsh: `collect`.)
    async fn collect(
        &self,
        options: &SkillLookupOptions,
    ) -> Result<(Arc<Entries>, bool), SkillError> {
        check_abort(options.signal.as_ref())?;
        let mut attempt = 1;
        loop {
            let (revision, key, layers) = {
                let state = self.shared.lock();
                let key = CacheKey {
                    cwd: options.cwd.clone(),
                    scope: options.scope.clone(),
                    revision: state.revision,
                };
                if let Some((_, cached)) = state.cache.iter().find(|(held, _)| *held == key) {
                    return Ok((Arc::clone(cached), true));
                }
                // Global first, then the scope's, so its same-name entry
                // replaces the global one.
                let mut layers = vec![(None, state.global.clone())];
                if let Some(scope) = &options.scope
                    && let Some(layer) = state.scoped.get(scope)
                {
                    layers.push((Some(scope.clone()), layer.clone()));
                }
                (state.revision, key, layers)
            };
            let mut merged = Entries::new();
            let mut cacheable = true;
            for (scope, layer) in layers {
                let (entries, complete) = self.collect_layer(scope, layer, options).await?;
                cacheable &= complete;
                for entry in entries {
                    merged.insert(entry.candidate.name.clone(), entry);
                }
            }
            check_abort(options.signal.as_ref())?;
            let merged = Arc::new(merged);
            let mut state = self.shared.lock();
            if revision != state.revision {
                if attempt < MAX_COLLECT_ATTEMPTS {
                    attempt += 1;
                    continue;
                }
                return Ok((merged, false));
            }
            if cacheable {
                state.cache.push_back((key, Arc::clone(&merged)));
                if state.cache.len() > self.shared.max_entries {
                    state.cache.pop_front();
                }
            }
            return Ok((merged, cacheable));
        }
    }

    /// One layer's winners, and whether its discovery completed. (dsh:
    /// `collectLayer` and `listLayerCandidates`.)
    async fn collect_layer(
        &self,
        scope: Option<SessionId>,
        layer: Layer,
        options: &SkillLookupOptions,
    ) -> Result<(Vec<Indexed>, bool), SkillError> {
        check_abort(options.signal.as_ref())?;
        let mut candidates = Vec::new();
        let mut cacheable = true;
        for (local_order, (_, definition)) in layer.runtime.values().enumerate() {
            candidates.push(Indexed {
                candidate: runtime_candidate(definition),
                provider: None,
                layer: scope.clone(),
                provider_order: -1,
                local_order: local_order as u64,
            });
        }
        for registered in &layer.providers {
            let provider = &registered.provider;
            let observation =
                match with_abort(provider.list(options), options.signal.as_ref()).await? {
                    Ok(observation) => observation,
                    Err(error) => {
                        cacheable = false;
                        self.shared.warn(format!(
                            "skill provider \"{}\" skipped: Error: {error}",
                            provider.name()
                        ));
                        continue;
                    }
                };
            cacheable &= observation.complete;
            for (local_order, candidate) in observation.candidates.into_iter().enumerate() {
                validate_candidate(&candidate, provider.name())?;
                candidates.push(Indexed {
                    candidate,
                    provider: Some((registered.id, Arc::clone(provider))),
                    layer: scope.clone(),
                    provider_order: i128::from(registered.order),
                    local_order: local_order as u64,
                });
            }
        }
        // Stable: rank, then registration order, then the provider's own.
        candidates.sort_by(|left, right| {
            left.candidate
                .rank
                .total_cmp(&right.candidate.rank)
                .then(left.provider_order.cmp(&right.provider_order))
                .then(left.local_order.cmp(&right.local_order))
        });
        let mut seen = std::collections::HashSet::new();
        let mut winners = Vec::new();
        for entry in candidates {
            if !seen.insert(entry.candidate.name.clone()) {
                self.shared.warn(format!(
                    "skill \"{}\" from {} ignored because a higher-priority skill already exists",
                    entry.candidate.name, entry.candidate.source
                ));
                continue;
            }
            winners.push(entry);
        }
        Ok((winners, cacheable))
    }
}

impl std::fmt::Debug for SkillRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.shared.lock();
        f.debug_struct("SkillRegistry")
            .field("providers", &state.global.providers.len())
            .field("runtime", &state.global.runtime.len())
            .field("scoped", &state.scoped.len())
            .finish_non_exhaustive()
    }
}

/// A runtime skill as a candidate, its body its locator. (dsh:
/// `runtimeCandidate`.)
fn runtime_candidate(definition: &Arc<SkillDefinition>) -> SkillCandidate {
    SkillCandidate {
        path: definition.path.clone(),
        name: definition.name.clone(),
        description: definition.description.clone(),
        when_to_use: definition.when_to_use.clone(),
        invocation: definition.invocation,
        source: definition.source.clone(),
        provider: definition.provider.clone(),
        resource_base: definition.resource_base.clone(),
        rank: RUNTIME_RANK,
        locator: Arc::new(SkillDefinition::clone(definition)),
        metadata: definition.metadata.clone(),
    }
}

/// dsh's `validateCandidate`, for what a Rust candidate can get wrong; its
/// type checks are the types here.
fn validate_candidate(candidate: &SkillCandidate, provider: &str) -> Result<(), SkillError> {
    let name = &candidate.name;
    let invalid = |message: String| Err(SkillError::Invalid(message));
    if !is_skill_name(name) {
        return invalid(format!(
            "skill provider \"{provider}\" returned invalid skill name \"{name}\""
        ));
    }
    if candidate.description.is_empty() {
        return invalid(format!(
            "skill provider \"{provider}\" returned skill \"{name}\" without a description"
        ));
    }
    if !candidate.rank.is_finite() {
        return invalid(format!(
            "skill provider \"{provider}\" returned skill \"{name}\" with an invalid rank"
        ));
    }
    if candidate.provider != provider {
        return invalid(format!(
            "skill provider \"{provider}\" returned skill \"{name}\" for provider \"{}\"",
            candidate.provider
        ));
    }
    Ok(())
}

/// dsh's `validateDefinition`, for what a Rust definition can get wrong.
fn validate_definition(definition: &SkillDefinition) -> Result<(), SkillError> {
    let name = &definition.name;
    if !is_skill_name(name) {
        return Err(SkillError::Invalid(format!(
            "loaded skill has invalid name \"{name}\""
        )));
    }
    if definition.description.is_empty() {
        return Err(SkillError::Invalid(format!(
            "loaded skill \"{name}\" requires a description"
        )));
    }
    Ok(())
}
