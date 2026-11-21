//! Retries a failed model request as its provider's retry policy says: a
//! port of dsh's `llm-retry` (`packages/llm/llm-retry/src/index.ts`).
//!
//! [`RetryPlugin`] registers a [`RetryHandler`] in the loop's request-error
//! hook. The policy is the provider's own
//! ([`Model::retry_policy`](karaka_model::Model::retry_policy)); this crate
//! has no config of its own, as dsh's plugin has none.
//!
//! # What a retry does
//!
//! - **Normal mode** retries only failures whose code the policy lists, at
//!   most `max_retries` times in one step. Anything else is handed on to
//!   the rest of the chain.
//! - **Always mode** first hands the failure on, so a handler after it that
//!   knows the failure better can recover it. If none asks for a retry, it
//!   retries anyway, with no limit.
//! - **The wait** is the provider's `Retry-After`, when it is more than
//!   nothing and no more than the policy's longest wait. A longer one makes
//!   normal mode give up, handing on, and always mode back off on its own.
//!   Otherwise the wait is exponential backoff with jitter.
//! - **The log.** An `llm/retry` event is appended before the wait, so the
//!   log says a retry was planned even if the process dies during the wait,
//!   and `llm/retry-started` after it. A cancel during the wait ends it, and
//!   no `llm/retry-started` follows.
//!
//! # The count comes from the log
//!
//! How many times a step has retried is read from the session, not kept in
//! memory: the latest `llm/retry` for the same provider and policy since the
//! step began (`retry.rs`, as dsh's `llmRetry` projection). So anything that
//! reads the log back mid-step carries on counting. A step that starts, or
//! a turn that ends, starts the count again.

mod events;
mod plugin;
mod retry;

pub use events::{RETRY, RETRY_STARTED, policy_key};
pub use plugin::RetryPlugin;
pub use retry::{Random, RetryHandler, local_delay};
