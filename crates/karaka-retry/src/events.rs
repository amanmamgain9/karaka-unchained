//! The two events a retry logs, and reading the retry count back from the
//! log. (dsh: `LlmRetryEventData` and `LlmRetryStartedEventData` in
//! `llm-retry/src/types.ts`, and the `llmRetry` projection in `index.ts`.)
//!
//! They are a plugin's events, so they stay out of `karaka-session`'s typed
//! vocabulary, as dsh declares them in `llm-retry` rather than in its
//! session package. They are logged as passthrough events, in v4's envelope,
//! with the data dsh writes, field for field and in dsh's order.

use karaka_model::RetryPolicy;
use karaka_session::{Failure, Passthrough, Session, SessionEvent};
use serde_json::{Map, Number, Value, json};

/// dsh: `llm/retry`, logged before each wait.
pub const RETRY: &str = "llm/retry";

/// dsh: `llm/retry-started`, logged after the wait, as the next attempt
/// starts.
pub const RETRY_STARTED: &str = "llm/retry-started";

/// The `llm/retry` this step logged last for one provider and policy.
pub(crate) struct Previous {
    pub(crate) retry: u64,
    pub(crate) retry_id: String,
}

/// Finds the count so far, as dsh's `llmRetry` projection folds it: a
/// `step/start` or a `turn/end` clears it, and each `llm/retry` sets the
/// entry for its provider and policy key.
pub(crate) fn previous(session: &Session, provider: &str, policy_key: &str) -> Option<Previous> {
    for entry in session.events().iter().rev() {
        match &entry.event {
            SessionEvent::StepStart(_) | SessionEvent::TurnEnd(_) => return None,
            SessionEvent::Passthrough(event)
                if event.event_type == RETRY
                    && event.data["provider"] == provider
                    && event.data["policyKey"] == policy_key =>
            {
                return Some(Previous {
                    retry: event.data["retry"].as_u64()?,
                    retry_id: event.data["retryId"].as_str()?.to_owned(),
                });
            }
            _ => {}
        }
    }
    None
}

/// What one scheduled retry records. (dsh: `LlmRetryEventData`.)
pub(crate) struct Scheduled<'a> {
    pub(crate) retry_id: &'a str,
    pub(crate) turn: u64,
    pub(crate) step: u64,
    pub(crate) provider: &'a str,
    pub(crate) policy: &'a RetryPolicy,
    pub(crate) policy_key: &'a str,
    pub(crate) retry: u64,
    pub(crate) delay_ms: f64,
    pub(crate) failure: &'a Failure,
}

/// The `llm/retry` event: `{ retryId, turn, step, provider, mode,
/// policyKey, retry, maxRetries, delayMs, failure }`, without `maxRetries`
/// in always mode.
pub(crate) fn retry_event(scheduled: &Scheduled<'_>) -> Result<SessionEvent, serde_json::Error> {
    let mut data = Map::new();
    data.insert("retryId".into(), json!(scheduled.retry_id));
    data.insert("turn".into(), json!(scheduled.turn));
    data.insert("step".into(), json!(scheduled.step));
    data.insert("provider".into(), json!(scheduled.provider));
    data.insert("mode".into(), json!(mode(scheduled.policy)));
    data.insert("policyKey".into(), json!(scheduled.policy_key));
    data.insert("retry".into(), json!(scheduled.retry));
    if let RetryPolicy::Normal { max_retries, .. } = scheduled.policy {
        data.insert("maxRetries".into(), json!(max_retries));
    }
    data.insert("delayMs".into(), number(scheduled.delay_ms));
    data.insert("failure".into(), serde_json::to_value(scheduled.failure)?);
    Ok(plugin_event(RETRY, Value::Object(data)))
}

/// The `llm/retry-started` event: `{ retryId, turn, step, retry }`.
pub(crate) fn started_event(retry_id: &str, turn: u64, step: u64, retry: u64) -> SessionEvent {
    plugin_event(
        RETRY_STARTED,
        json!({ "retryId": retry_id, "turn": turn, "step": step, "retry": retry }),
    )
}

fn plugin_event(event_type: &str, data: Value) -> SessionEvent {
    SessionEvent::Passthrough(Passthrough {
        event_type: event_type.into(),
        data,
        surface_op: None,
        source_event_seqs: None,
    })
}

/// The text that tells one policy from another, so a count kept under one
/// policy isn't continued under another. dsh's own form, so a log keeps
/// counting across the two (`retryPolicyKey`, `index.ts:72-84`):
/// `["normal",5,["EMPTY_RESPONSE",...],500,10000,0.1]`, with the codes
/// sorted, or `["always",500,10000,0.1]`.
pub fn policy_key(policy: &RetryPolicy) -> String {
    let backoff = policy.backoff();
    let delays = [
        number(backoff.initial_delay_ms),
        number(backoff.max_delay_ms),
        number(backoff.jitter_ratio),
    ];
    let key = match policy {
        RetryPolicy::Always { .. } => {
            let mut key = vec![json!("always")];
            key.extend(delays);
            key
        }
        RetryPolicy::Normal {
            max_retries,
            retryable_codes,
            ..
        } => {
            let mut codes = retryable_codes.clone();
            codes.sort();
            let mut key = vec![json!("normal"), json!(max_retries), json!(codes)];
            key.extend(delays);
            key
        }
    };
    Value::Array(key).to_string()
}

fn mode(policy: &RetryPolicy) -> &'static str {
    match policy {
        RetryPolicy::Normal { .. } => "normal",
        RetryPolicy::Always { .. } => "always",
    }
}

/// `value` as JavaScript writes a number into JSON: a whole number with no
/// `.0`, as dsh's `500` and not `500.0`.
fn number(value: f64) -> Value {
    const SAFE: f64 = 9_007_199_254_740_991.0;
    if value.fract() == 0.0 && value.abs() <= SAFE {
        // In range and whole, so the cast is exact.
        return json!(value as i64);
    }
    Number::from_f64(value).map_or(Value::Null, Value::Number)
}

#[cfg(test)]
mod tests {
    use karaka_model::{Backoff, RetryPolicy};

    use super::{number, policy_key};

    /// dsh's key for the policy its "records the scheduled delay before
    /// retrying the request" case uses (`retry.spec.ts:229`).
    #[test]
    fn a_policy_key_is_the_one_dsh_writes() {
        let policy = RetryPolicy::Normal {
            max_retries: 2,
            retryable_codes: vec!["SERVER".into(), "RATE_LIMIT".into()],
            backoff: Backoff {
                jitter_ratio: 0.0,
                ..Backoff::default()
            },
        };
        assert_eq!(
            policy_key(&policy),
            r#"["normal",2,["RATE_LIMIT","SERVER"],500,10000,0]"#
        );
        // `persistence.spec.ts:42`.
        assert_eq!(
            policy_key(&RetryPolicy::Always {
                backoff: Backoff::default()
            }),
            r#"["always",500,10000,0.1]"#
        );
    }

    #[test]
    fn numbers_are_written_as_javascript_writes_them() {
        assert_eq!(number(500.0).to_string(), "500");
        assert_eq!(number(0.1).to_string(), "0.1");
        assert_eq!(number(1100.5).to_string(), "1100.5");
    }
}
