//! What composes an agent from its session: the preset its scope chain
//! names. (dsh: the `setup` callback an agent is created with, in which the
//! session controller calls `agentPresets.mount`, and `composeAgent` in
//! `api/session-controller/src/agent.ts`.)
//!
//! The loop provides [`AgentComposition`], empty. A plugin that composes
//! agents, such as the agent preset registry, sets its [`AgentComposer`]
//! there during setup. With no composer, an agent's chain is its session
//! alone.
//!
//! The loop asks the composer three things:
//!
//! - what header a session it creates starts with, so a new session names
//!   the default preset, as dsh's session controller writes the resolved
//!   preset into a new session's `meta`;
//! - whether a session can be composed at all, before its agent opens, so a
//!   session naming a preset that doesn't exist, or is broken, refuses to
//!   open, as dsh's `mount` throws;
//! - which preset the agent runs, each time its chain is read, so a
//!   selection logged while the session is blank takes effect at once, as
//!   dsh's `recompose` rebinds the agent's scope.

use std::fmt;
use std::sync::{Arc, PoisonError, RwLock};

use karaka_scope::PresetId;
use karaka_session::{Session, SessionHeader};
use serde_json::Value;

/// Why a session can't be composed. (dsh: the `RemoteError` `mount`
/// throws, with its code and details.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ComposeError {
    /// dsh's error code, such as `agent-preset/not-found`.
    pub code: String,
    pub message: String,
    /// dsh's details, as their JSON.
    pub details: Value,
}

impl fmt::Display for ComposeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ComposeError {}

/// Composes agents from their sessions. See the module docs.
pub trait AgentComposer: Send + Sync {
    /// The header a session the loop creates starts with, given the fresh
    /// one. The default keeps it.
    fn prepare(&self, header: SessionHeader) -> SessionHeader {
        header
    }

    /// Whether `session` can be composed. An error refuses its agent's
    /// open.
    ///
    /// # Errors
    ///
    /// Why the session can't be composed.
    fn check(&self, session: &Session) -> Result<(), ComposeError>;

    /// The preset an agent of `session` runs, if any.
    fn preset(&self, session: &Session) -> Option<PresetId>;
}

/// The port a composer is set in. The loop provides it, empty, and reads
/// it whenever it opens an agent or reads an agent's chain.
#[derive(Default)]
pub struct AgentComposition {
    composer: RwLock<Option<Arc<dyn AgentComposer>>>,
}

impl AgentComposition {
    pub fn new() -> Self {
        Self::default()
    }

    /// Makes `composer` the one agents are composed by, for the runtime's
    /// life.
    ///
    /// # Errors
    ///
    /// A composer is set already: a runtime composes agents one way.
    pub fn set(&self, composer: Arc<dyn AgentComposer>) -> Result<(), CompositionTaken> {
        let mut slot = self
            .composer
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        if slot.is_some() {
            return Err(CompositionTaken);
        }
        *slot = Some(composer);
        Ok(())
    }

    /// The composer, if one is set.
    pub fn composer(&self) -> Option<Arc<dyn AgentComposer>> {
        self.composer
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The preset an agent of `session` runs: none without a composer.
    pub fn preset(&self, session: &Session) -> Option<PresetId> {
        self.composer()
            .and_then(|composer| composer.preset(session))
    }
}

impl fmt::Debug for AgentComposition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AgentComposition")
            .field("composer", &self.composer().is_some())
            .finish()
    }
}

/// [`AgentComposition::set`] was called twice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompositionTaken;

impl fmt::Display for CompositionTaken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an agent composer is already set")
    }
}

impl std::error::Error for CompositionTaken {}
