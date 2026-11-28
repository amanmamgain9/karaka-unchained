//! The registry of presets, and how a session runs one. (dsh:
//! `AgentPresetRegistry` in `agent-preset-registry/src/index.ts`, the
//! `agentPresets` service.)

use std::cmp::Ordering;
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};

use karaka_agent::{AgentComposer, ComposeError};
use karaka_scope::PresetId;
use karaka_session::{Session, SessionEvent, SessionHeader, SessionId};
use karaka_session_projection::SessionProjections;
use serde::Serialize;
use serde_json::{Value, json};

use crate::session::{AgentPresetProjection, selected};

/// The registry's settings. (dsh: `Config`, without `selectedDefault`,
/// which waits on a settings service.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentPresetsConfig {
    /// The preset a session that names none runs. (dsh: `default`,
    /// required.)
    pub default: String,
}

impl AgentPresetsConfig {
    pub fn new(default: impl Into<String>) -> Self {
        Self {
            default: default.into(),
        }
    }
}

/// One preset, as the registry lists it. (dsh: `AgentPreset`.)
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentPreset {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order: Option<f64>,
    /// Why the preset can't compose a session; `None` when it can.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub broken: Option<String>,
}

/// One preset as a client reads it. (dsh: `AgentPresetRow`.)
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentPresetRow {
    pub id: String,
    /// Whether a session naming no preset runs this one.
    pub is_default: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub broken: Option<String>,
}

/// Every preset the runtime has, broken ones included. (dsh:
/// `AgentPresetRoster`.)
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AgentPresetRoster {
    pub presets: Vec<AgentPresetRow>,
}

/// Why the registry refused. (dsh: the `RemoteError`s of
/// `RemoteErrorDetailsMap`, and `register`'s errors.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PresetError {
    /// No preset has the id. (dsh: `agent-preset/not-found`.)
    NotFound {
        agent_preset: String,
        /// Every preset's id, in the order they were registered.
        available: Vec<String>,
    },
    /// The preset can't compose a session. (dsh: `agent-preset/invalid`.)
    Invalid {
        agent_preset: String,
        reason: String,
    },
    /// The session's conversation has started, so its preset is fixed.
    /// (dsh: `agent-preset/locked`.)
    Locked {
        session_id: SessionId,
        agent_preset: String,
    },
    /// A definition with an empty id.
    EmptyId,
    /// Two definitions with one id.
    Duplicate(String),
    /// The session refused the selection event.
    Log(String),
}

impl PresetError {
    /// dsh's code, for the three errors a client sees.
    pub fn code(&self) -> Option<&'static str> {
        match self {
            Self::NotFound { .. } => Some("agent-preset/not-found"),
            Self::Invalid { .. } => Some("agent-preset/invalid"),
            Self::Locked { .. } => Some("agent-preset/locked"),
            _ => None,
        }
    }

    /// dsh's details, as their JSON, for the three errors a client sees.
    pub fn details(&self) -> Value {
        match self {
            Self::NotFound {
                agent_preset,
                available,
            } => json!({ "agentPreset": agent_preset, "available": available }),
            Self::Invalid {
                agent_preset,
                reason,
            } => json!({ "agentPreset": agent_preset, "reason": reason }),
            Self::Locked {
                session_id,
                agent_preset,
            } => json!({ "sessionId": session_id.as_str(), "agentPreset": agent_preset }),
            _ => Value::Null,
        }
    }
}

impl fmt::Display for PresetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound { agent_preset, .. } => {
                write!(f, "Unknown agent preset: {agent_preset}")
            }
            Self::Invalid { reason, .. } => f.write_str(reason),
            Self::Locked { .. } => f.write_str("This session has already started"),
            Self::EmptyId => f.write_str("Preset id must not be empty"),
            Self::Duplicate(id) => write!(f, "Duplicate agent preset: {id}"),
            Self::Log(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for PresetError {}

impl From<PresetError> for ComposeError {
    fn from(error: PresetError) -> Self {
        Self {
            code: error.code().unwrap_or_default().to_owned(),
            message: error.to_string(),
            details: error.details(),
        }
    }
}

/// One preset the plugin set up.
#[derive(Clone, Debug)]
pub(crate) struct Record {
    pub(crate) id: String,
    pub(crate) name: Option<String>,
    pub(crate) description: Option<String>,
    pub(crate) order: Option<f64>,
    /// Why its setup failed: one line per failed plugin.
    pub(crate) broken: Option<String>,
}

/// The presets of a runtime, set up when it started, and each session's
/// choice of one. (dsh: `AgentPresetRegistry`, `ctx.agentPresets`.)
///
/// It is also the [`AgentComposer`] the loop asks: a session it creates
/// names the default preset, a session naming a preset that is unknown or
/// broken refuses to open, and an agent's chain names its session's preset.
pub struct AgentPresets {
    config: AgentPresetsConfig,
    /// In the order they were registered.
    records: Vec<Record>,
    projections: Arc<SessionProjections>,
    /// One selection at a time, so its check and its event can't be split.
    /// (dsh: `switches`, a chain per agent.)
    selecting: Mutex<()>,
}

impl AgentPresets {
    pub(crate) fn new(
        config: AgentPresetsConfig,
        records: Vec<Record>,
        projections: Arc<SessionProjections>,
    ) -> Self {
        Self {
            config,
            records,
            projections,
            selecting: Mutex::new(()),
        }
    }

    /// The preset a session that names none runs. (dsh: `defaultId`,
    /// without the user's `selectedDefault`.)
    pub fn default_id(&self) -> &str {
        &self.config.default
    }

    /// Every preset, broken ones included, by order, then id. (dsh:
    /// `list`, which sorts ids by `localeCompare`; here by their bytes.)
    pub fn list(&self) -> Vec<AgentPreset> {
        let mut rows: Vec<AgentPreset> = self
            .records
            .iter()
            .map(|record| AgentPreset {
                id: record.id.clone(),
                name: record.name.clone(),
                description: record.description.clone(),
                order: record.order,
                broken: record.broken.clone(),
            })
            .collect();
        rows.sort_by(|a, b| {
            let order = |row: &AgentPreset| row.order.unwrap_or(f64::INFINITY);
            // dsh's `Infinity - Infinity` is NaN, which falls through to the
            // id, as an equal order does.
            order(a)
                .partial_cmp(&order(b))
                .unwrap_or(Ordering::Equal)
                .then_with(|| a.id.cmp(&b.id))
        });
        rows
    }

    /// [`list`](Self::list), each row marked when it is the default. (dsh:
    /// the `list` remote, `remoteExportList`.)
    pub fn roster(&self) -> AgentPresetRoster {
        AgentPresetRoster {
            presets: self
                .list()
                .into_iter()
                .map(|row| AgentPresetRow {
                    is_default: row.id == self.config.default,
                    id: row.id,
                    name: row.name,
                    description: row.description,
                    broken: row.broken,
                })
                .collect(),
        }
    }

    /// The preset `id`, or the default, with why it is broken, if it is.
    /// As dsh's, the result holds only the id and `broken`. (dsh:
    /// `resolve`.)
    ///
    /// # Errors
    ///
    /// `agent-preset/not-found`.
    pub fn resolve(&self, id: Option<&str>) -> Result<AgentPreset, PresetError> {
        let record = self.record(id.unwrap_or(&self.config.default))?;
        Ok(AgentPreset {
            id: record.id.clone(),
            name: None,
            description: None,
            order: None,
            broken: record.broken.clone(),
        })
    }

    /// The preset `id`, or the default, if a session can run it. (dsh:
    /// `retain`, without the revision it leases.)
    ///
    /// # Errors
    ///
    /// `agent-preset/not-found`, or `agent-preset/invalid` with why it is
    /// broken.
    pub fn usable(&self, id: Option<&str>) -> Result<&str, PresetError> {
        let record = self.record(id.unwrap_or(&self.config.default))?;
        match &record.broken {
            None => Ok(&record.id),
            Some(reason) => Err(PresetError::Invalid {
                agent_preset: record.id.clone(),
                reason: reason.clone(),
            }),
        }
    }

    /// The preset `session` records: its header's, or its latest
    /// selection; `None` when neither names one. (dsh: the `agentPreset`
    /// projection's state.)
    pub fn recorded(&self, session: &Session) -> Option<String> {
        self.projections
            .state_of::<AgentPresetProjection>(session)
            .and_then(|state| (*state).clone())
    }

    /// The preset `session` runs: the one it records, or the default.
    pub fn running(&self, session: &Session) -> String {
        self.recorded(session)
            .unwrap_or_else(|| self.config.default.clone())
    }

    /// Makes `session` run the preset `id`, while it is blank: before its
    /// first turn starts. A session that already records the preset logs
    /// nothing. (dsh: `select`.)
    ///
    /// Its agent's chain names the new preset at once: the loop reads the
    /// projection each time, where dsh rebinds the agent's scope
    /// (`recompose`).
    ///
    /// # Errors
    ///
    /// In dsh's order: `agent-preset/locked` once a turn has started; then
    /// `agent-preset/not-found` or `agent-preset/invalid`.
    pub fn select(&self, session: &Session, id: &str) -> Result<String, PresetError> {
        let _one = self
            .selecting
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        // dsh's `turnBoundary`: a turn is open, or one has run.
        let started = session
            .events()
            .iter()
            .any(|entry| matches!(entry.event, SessionEvent::TurnStart(_)));
        if started {
            return Err(PresetError::Locked {
                session_id: session.id().clone(),
                agent_preset: id.to_owned(),
            });
        }
        let usable = self
            .records
            .iter()
            .any(|record| record.id == id && record.broken.is_none());
        if usable && self.recorded(session).as_deref() == Some(id) {
            return Ok(id.to_owned());
        }
        let id = self.usable(Some(id))?.to_owned();
        session
            .append(selected(&id))
            .map_err(|error| PresetError::Log(error.to_string()))?;
        Ok(id)
    }

    fn record(&self, id: &str) -> Result<&Record, PresetError> {
        self.records
            .iter()
            .find(|record| record.id == id)
            .ok_or_else(|| PresetError::NotFound {
                agent_preset: id.to_owned(),
                available: self
                    .records
                    .iter()
                    .map(|record| record.id.clone())
                    .collect(),
            })
    }
}

impl AgentComposer for AgentPresets {
    /// A new session names the default preset, as dsh's session controller
    /// writes the resolved preset into a new session's header. A default no
    /// preset has is left out; the open then refuses.
    fn prepare(&self, mut header: SessionHeader) -> SessionHeader {
        if header.agent_preset.is_none() && self.record(&self.config.default).is_ok() {
            header.agent_preset = Some(self.config.default.clone());
        }
        header
    }

    fn check(&self, session: &Session) -> Result<(), ComposeError> {
        self.usable(Some(&self.running(session)))?;
        Ok(())
    }

    fn preset(&self, session: &Session) -> Option<PresetId> {
        Some(PresetId::new(self.running(session)))
    }
}

impl fmt::Debug for AgentPresets {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AgentPresets")
            .field("config", &self.config)
            .field("presets", &self.records)
            .finish_non_exhaustive()
    }
}
