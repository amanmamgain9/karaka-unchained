//! Scopes: whose a registration is, and which scopes a lookup sees. (dsh:
//! `@deepseek-ai/dsh-scope`, `core/scope/src/index.ts` and `store.ts`.)
//!
//! Every registry that keeps per-agent contributions (prompt sections and
//! contexts, answerers, pre-execute and pre-step listeners, skills, tools)
//! files each one under a [`ScopeKey`], or globally under `None`. A lookup
//! is made for a [`ScopeChain`], the viewer's scopes, and sees the global
//! layer and the layer of every scope on the chain.
//!
//! # The chain
//!
//! dsh gives a scope key a parent (`bindScopeParent`), so chains may be any
//! length, and a parent may be moved while a session is still blank
//! (`rebind`). An agent preset mounts its plugins into a scope of its own,
//! and each agent's scope is bound under its preset's. Karaka's plugins are
//! fixed when the runtime starts, so the chain is fixed at three levels:
//!
//! 1. global, which every chain sees;
//! 2. the agent's preset, if it has one;
//! 3. the agent's session.
//!
//! A child agent joins its parent's preset, as dsh's `composeFrom` does, so
//! three levels still cover it.
//!
//! # Precedence
//!
//! Read farthest first, the global layer, then the preset's, then the
//! session's, a nearer layer has the last word: its entry of a name shadows
//! a farther one (dsh: `ScopedLayers.merge`). A listener filed in a scope
//! hears its own scope and every scope nested inside it, never one outside
//! (dsh: `scopeTarget`); listeners keep the order they were added in.

use std::collections::HashMap;
use std::fmt;

use karaka_session::SessionId;

/// An agent preset's name, the key of its scope. (dsh: the preset row's
/// scope key.)
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PresetId(String);

impl PresetId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PresetId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The scope a registration is filed under. A registration with no key is
/// global. (dsh: `ScopeKey`, an opaque object compared by identity; here
/// a preset's or a session's id.)
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ScopeKey {
    /// Everything an agent preset mounts, which every agent of the preset
    /// sees.
    Preset(PresetId),
    /// One agent's own, by its session's id.
    Session(SessionId),
}

impl From<SessionId> for ScopeKey {
    fn from(id: SessionId) -> Self {
        Self::Session(id)
    }
}

impl From<PresetId> for ScopeKey {
    fn from(id: PresetId) -> Self {
        Self::Preset(id)
    }
}

impl fmt::Display for ScopeKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Preset(id) => write!(f, "preset \"{id}\""),
            Self::Session(id) => write!(f, "session \"{id}\""),
        }
    }
}

/// The scopes a viewer sees, besides the global layer: its preset's, then
/// its session's. (dsh: `scopeChainOf`, fixed at three levels.)
///
/// The empty chain, [`ScopeChain::global`], sees the global layer alone, as
/// a dsh lookup with no scope does.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct ScopeChain {
    pub preset: Option<PresetId>,
    pub session: Option<SessionId>,
}

impl ScopeChain {
    pub fn new(preset: Option<PresetId>, session: Option<SessionId>) -> Self {
        Self { preset, session }
    }

    /// No scopes: the global layer alone.
    pub fn global() -> Self {
        Self::default()
    }

    /// A session with no preset, as every agent is until presets exist.
    pub fn session(id: SessionId) -> Self {
        Self {
            preset: None,
            session: Some(id),
        }
    }

    /// Whether the chain holds no scope.
    pub fn is_global(&self) -> bool {
        self.preset.is_none() && self.session.is_none()
    }

    /// The scopes, the farthest first: the order a merge reads them in, so
    /// the nearest has the last word. (dsh: `ScopedLayers.chainLayers`.)
    pub fn farthest_first(&self) -> Vec<ScopeKey> {
        let mut keys = self.nearest_first();
        keys.reverse();
        keys
    }

    /// The scopes, the nearest first. (dsh: `scopeChainOf`.)
    pub fn nearest_first(&self) -> Vec<ScopeKey> {
        self.session
            .iter()
            .cloned()
            .map(ScopeKey::Session)
            .chain(self.preset.iter().cloned().map(ScopeKey::Preset))
            .collect()
    }

    /// The nearest scope: the viewer's own, whose registrations it owns
    /// rather than inherits. `None` for the empty chain.
    pub fn nearest(&self) -> Option<ScopeKey> {
        self.nearest_first().into_iter().next()
    }

    /// Whether `key` is on the chain.
    pub fn contains(&self, key: &ScopeKey) -> bool {
        match key {
            ScopeKey::Preset(id) => self.preset.as_ref() == Some(id),
            ScopeKey::Session(id) => self.session.as_ref() == Some(id),
        }
    }

    /// Whether a registration filed under `scope` is seen through this
    /// chain: a global one always, a scoped one when its scope is on the
    /// chain. (dsh: `scopeTarget`'s filter.)
    pub fn admits(&self, scope: Option<&ScopeKey>) -> bool {
        scope.is_none_or(|key| self.contains(key))
    }
}

/// A registry's global layer and its scopes' layers. (dsh: `ScopedLayers`.)
///
/// The global layer always exists. A scoped one is made by the first
/// registration in it, never by a read, and [`prune`](Self::prune) forgets
/// it once it is empty.
#[derive(Debug)]
pub struct ScopedLayers<L> {
    global: L,
    scoped: HashMap<ScopeKey, L>,
}

impl<L: Default> Default for ScopedLayers<L> {
    fn default() -> Self {
        Self {
            global: L::default(),
            scoped: HashMap::new(),
        }
    }
}

impl<L: Default> ScopedLayers<L> {
    /// The layer to register in: the global one for `None`, otherwise the
    /// scope's, made if it is new.
    pub fn layer_mut(&mut self, scope: Option<&ScopeKey>) -> &mut L {
        match scope {
            None => &mut self.global,
            Some(key) => self.scoped.entry(key.clone()).or_default(),
        }
    }
}

impl<L> ScopedLayers<L> {
    /// The global layer.
    pub fn global(&self) -> &L {
        &self.global
    }

    /// The global layer for `None`, otherwise the scope's, if it exists.
    pub fn get(&self, scope: Option<&ScopeKey>) -> Option<&L> {
        match scope {
            None => Some(&self.global),
            Some(key) => self.scoped.get(key),
        }
    }

    /// As [`get`](Self::get), to change it.
    pub fn get_mut(&mut self, scope: Option<&ScopeKey>) -> Option<&mut L> {
        match scope {
            None => Some(&mut self.global),
            Some(key) => self.scoped.get_mut(key),
        }
    }

    /// One scope's own layer, if it exists, and none of its chain's. (dsh:
    /// `peek`, chain-blind on purpose.)
    pub fn peek(&self, key: &ScopeKey) -> Option<&L> {
        self.scoped.get(key)
    }

    /// The layers of the chain's scopes that exist, the farthest first,
    /// each with its key; not the global layer. (dsh: `chainLayers`.)
    pub fn chain_layers(&self, chain: &ScopeChain) -> Vec<(ScopeKey, &L)> {
        chain
            .farthest_first()
            .into_iter()
            .filter_map(|key| self.scoped.get(&key).map(|layer| (key, layer)))
            .collect()
    }

    /// Forgets the scope's layer when `is_empty` says it is. The global
    /// layer stays.
    pub fn prune(&mut self, scope: Option<&ScopeKey>, is_empty: impl FnOnce(&L) -> bool) {
        if let Some(key) = scope
            && self.scoped.get(key).is_some_and(is_empty)
        {
            self.scoped.remove(key);
        }
    }

    /// How many scoped layers exist.
    pub fn scoped_len(&self) -> usize {
        self.scoped.len()
    }
}
