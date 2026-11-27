//! Approval outcomes, ids, policies and requests, and the events the
//! service logs, in dsh's JSON. (dsh: `user-approval/src/types.ts`, and
//! the policy types in `index.ts`.)

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use karaka_agent::Agent;
use karaka_fs::AbortSignal;
use serde::{Deserialize, Serialize};

/// The `approval/asked` event type.
pub const ASKED_EVENT: &str = "approval/asked";
/// The `approval/decided` event type.
pub const DECIDED_EVENT: &str = "approval/decided";
/// The `approval/policy` event type.
pub const POLICY_EVENT: &str = "approval/policy";

/// How a request ended: a one-shot grant, an explicit rejection, a
/// withdrawn request, or no answerer to decide. Callers fail closed on
/// `Unavailable`; `AllowedOnce` is the only grant. (dsh:
/// `ApprovalOutcome`.)
///
/// dsh normalizes an answerer's value outside this vocabulary to
/// `unavailable`. A Rust answerer can't return one: the enum is closed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalOutcome {
    AllowedOnce,
    Rejected,
    Cancelled,
    Unavailable,
}

impl ApprovalOutcome {
    /// Every outcome. (dsh: `OUTCOMES`.)
    pub const ALL: [Self; 4] = [
        Self::AllowedOnce,
        Self::Rejected,
        Self::Cancelled,
        Self::Unavailable,
    ];

    /// dsh's word for it, such as `allowed-once`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AllowedOnce => "allowed-once",
            Self::Rejected => "rejected",
            Self::Cancelled => "cancelled",
            Self::Unavailable => "unavailable",
        }
    }
}

impl fmt::Display for ApprovalOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Pairs one `approval/asked` with its `approval/decided`. The service
/// issues a fresh one per request. (dsh: `ApprovalRequestId`.)
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ApprovalRequestId(pub String);

impl ApprovalRequestId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ApprovalRequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What happens to a request before any answerer sees it. (dsh:
/// `ApprovalPolicy`.)
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase", try_from = "String")]
pub enum ApprovalPolicy {
    /// Ask the answerers; with none, the request fails closed as
    /// `unavailable`. The default.
    #[default]
    Ask,
    /// Ask no one: every request is `rejected`. The headless stance, for CI
    /// and unattended runs.
    Never,
}

impl ApprovalPolicy {
    /// Every policy. (dsh: `APPROVAL_POLICIES`.)
    pub const ALL: [Self; 2] = [Self::Ask, Self::Never];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::Never => "never",
        }
    }
}

impl fmt::Display for ApprovalPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A policy string outside the vocabulary. (dsh: the `TypeError`
/// `setApprovalPolicy` throws.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PolicyError;

impl fmt::Display for PolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("approval policy must be one of \"ask\" or \"never\"")
    }
}

impl std::error::Error for PolicyError {}

impl FromStr for ApprovalPolicy {
    type Err = PolicyError;

    fn from_str(text: &str) -> Result<Self, PolicyError> {
        Self::ALL
            .into_iter()
            .find(|policy| policy.as_str() == text)
            .ok_or(PolicyError)
    }
}

impl TryFrom<String> for ApprovalPolicy {
    type Error = PolicyError;

    fn try_from(text: String) -> Result<Self, PolicyError> {
        text.parse()
    }
}

/// The reason, localized for display only: never logged. (dsh:
/// `displayReason`, `{ en, [locale]: string }`.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DisplayReason {
    pub en: String,
    /// Other locales, by their tag.
    pub others: BTreeMap<String, String>,
}

impl DisplayReason {
    /// A reason in English only.
    pub fn en(text: impl Into<String>) -> Self {
        Self {
            en: text.into(),
            others: BTreeMap::new(),
        }
    }
}

/// One question for the answerers: may `agent` go ahead with `tool_name`?
/// (dsh: `ApprovalRequest`, and `ApprovalRequestEvent`.)
#[derive(Clone)]
pub struct ApprovalRequest {
    /// Whose action it is. It routes the request to that agent's answerers,
    /// and its session gets the audit events.
    pub agent: Arc<dyn Agent>,
    /// The tool the question is about.
    pub tool_name: String,
    /// The exact tool call being decided, when the asker has one.
    pub call_id: Option<String>,
    /// Why the asker asks.
    pub reason: Option<String>,
    /// For display only; never logged.
    pub display_reason: Option<DisplayReason>,
    /// Withdraws the request: it settles `cancelled` at once, and a late
    /// answer is dropped. (dsh: `signal`.)
    pub signal: Option<AbortSignal>,
}

impl ApprovalRequest {
    /// A request about `tool_name`, with nothing else.
    pub fn new(agent: Arc<dyn Agent>, tool_name: impl Into<String>) -> Self {
        Self {
            agent,
            tool_name: tool_name.into(),
            call_id: None,
            reason: None,
            display_reason: None,
            signal: None,
        }
    }
}

impl fmt::Debug for ApprovalRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApprovalRequest")
            .field("agent", self.agent.id())
            .field("tool_name", &self.tool_name)
            .field("call_id", &self.call_id)
            .field("reason", &self.reason)
            .field("display_reason", &self.display_reason)
            .field("signal", &self.signal.as_ref().map(AbortSignal::is_aborted))
            .finish()
    }
}

/// `approval/asked`: a question was put to the answerers. Log-only audit,
/// no surface op. (dsh: `SessionEventMap['approval/asked']`.)
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalAsked {
    pub id: ApprovalRequestId,
    pub tool_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// `approval/decided`: how the question with the same `id` ended.
/// Exactly one follows each ask. (dsh: `SessionEventMap['approval/decided']`.)
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalDecided {
    pub id: ApprovalRequestId,
    pub outcome: ApprovalOutcome,
}

/// `approval/policy`: the session's policy was switched. The last one is
/// the session's override. (dsh: `SessionEventMap['approval/policy']`.)
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalPolicyChange {
    pub policy: ApprovalPolicy,
    /// `delegation` for an override seeded into a child; absent for a
    /// runtime switch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<PolicySource>,
}

/// Where an `approval/policy` came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PolicySource {
    /// Seeded into a child when it was delegated to.
    Delegation,
}
