//! Approvals: whether an action may go ahead, decided by a chain of
//! answerers under each session's policy, with an audit pair in the
//! session's log. (dsh: `packages/interaction/user-approval`,
//! `dsh-user-approval`.)
//!
//! [`ApprovalService::request`] asks; it never grants by default: a
//! request nobody answers is `unavailable`, and callers fail closed. A
//! session's policy, `ask` or `never`, is decided before any answerer.
//! The plugin also tells the model the policy, as the `approval:policy`
//! runtime context.
//!
//! - `types.rs`: outcomes, ids, policies, requests, and the events.
//! - `service.rs`: the service, its answerer chain, and the projection it
//!   reads a session's state from.
//! - `plugin.rs`: the plugin, and the runtime context.
//!
//! The events, `approval/asked`, `approval/decided` and `approval/policy`,
//! are v4 types the session crate knows but doesn't model, so they are
//! stored as passthroughs, their shapes [`ApprovalAsked`],
//! [`ApprovalDecided`] and [`ApprovalPolicyChange`].

mod error;
mod plugin;
mod service;
mod types;

pub use error::{ApprovalError, OUTSIDE_TURN};
pub use plugin::{ApprovalConfig, ApprovalPlugin, CONTEXT_NAME};
pub use service::{
    ASK_SENTENCE, AnswererError, AnswererRegistration, ApprovalAnswerer, ApprovalFuture,
    ApprovalNext, ApprovalService, ApprovalState, ApprovalStateProjection, NEVER_SENTENCE,
    set_approval_policy,
};
pub use types::{
    ASKED_EVENT, ApprovalAsked, ApprovalDecided, ApprovalOutcome, ApprovalPolicy,
    ApprovalPolicyChange, ApprovalRequest, ApprovalRequestId, DECIDED_EVENT, DisplayReason,
    POLICY_EVENT, PolicyError, PolicySource,
};
