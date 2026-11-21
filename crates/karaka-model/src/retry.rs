//! How a provider wants its failed requests retried.
//! (dsh: `packages/llm/llm/src/retry-policy.ts`.)
//!
//! The policy belongs to the provider, as in dsh, where each provider's
//! config holds a `retryPolicy`. A [`Model`](crate::Model) hands its policy
//! out with [`Model::retry_policy`](crate::Model::retry_policy), and
//! whatever retries requests, such as Karaka's retry plugin, follows it.
//! Nothing here retries anything itself.

use std::fmt;

/// The longest delay a timer may be set for: JavaScript's limit, which dsh
/// applies to every delay (`MAX_TIMER_DELAY_MS`, `util/timeout`).
pub const MAX_DELAY_MS: f64 = 2_147_483_647.0;

/// The failure codes a normal policy retries unless told otherwise
/// (`DEFAULT_RETRYABLE_CODES`, `retry-policy.ts:18-24`). They are the codes
/// of the [`ErrorKind`](crate::ErrorKind)s that are retryable, as
/// [`ErrorKind::code`](crate::ErrorKind::code) writes them, plus `TIMEOUT`,
/// which dsh has and Karaka files under `TRANSPORT`.
pub const DEFAULT_RETRYABLE_CODES: [&str; 5] = [
    "EMPTY_RESPONSE",
    "RATE_LIMIT",
    "SERVER",
    "TIMEOUT",
    "TRANSPORT",
];

const DEFAULT_MAX_RETRIES: u64 = 5;
const DEFAULT_INITIAL_DELAY_MS: f64 = 500.0;
const DEFAULT_MAX_DELAY_MS: f64 = 10_000.0;
const DEFAULT_JITTER_RATIO: f64 = 0.1;

/// A provider's retry policy, with every default filled in.
/// (dsh: `ResolvedRetryPolicy`.)
///
/// The default is dsh's: normal mode, five retries of the default codes,
/// starting at 500 ms and never waiting more than 10 s, with 10% jitter.
#[derive(Clone, Debug, PartialEq)]
pub enum RetryPolicy {
    /// Retry only failures with one of `retryable_codes`, at most
    /// `max_retries` times in one step.
    Normal {
        max_retries: u64,
        retryable_codes: Vec<String>,
        backoff: Backoff,
    },
    /// Retry every failure, with no limit, until the request works or the
    /// turn is cancelled.
    Always { backoff: Backoff },
}

/// Exponential backoff with symmetric jitter. (dsh: `ResolvedRetryBackoff`.)
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Backoff {
    /// The first wait, in milliseconds. Each retry doubles it.
    pub initial_delay_ms: f64,
    /// The longest wait, in milliseconds, whether worked out here or asked
    /// for by the provider.
    pub max_delay_ms: f64,
    /// How far each wait may stray from its exponential value, as a share
    /// of it: 0.1 means anywhere from 90% to 110%.
    pub jitter_ratio: f64,
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            initial_delay_ms: DEFAULT_INITIAL_DELAY_MS,
            max_delay_ms: DEFAULT_MAX_DELAY_MS,
            jitter_ratio: DEFAULT_JITTER_RATIO,
        }
    }
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self::Normal {
            max_retries: DEFAULT_MAX_RETRIES,
            retryable_codes: DEFAULT_RETRYABLE_CODES.map(str::to_owned).to_vec(),
            backoff: Backoff::default(),
        }
    }
}

impl RetryPolicy {
    /// The backoff, which both modes have.
    pub fn backoff(&self) -> &Backoff {
        match self {
            Self::Normal { backoff, .. } | Self::Always { backoff } => backoff,
        }
    }

    /// Whether this policy retries a failure with `code` at all, before any
    /// count or delay is considered.
    pub fn covers(&self, code: &str) -> bool {
        match self {
            Self::Normal {
                retryable_codes, ..
            } => retryable_codes.iter().any(|retryable| retryable == code),
            Self::Always { .. } => true,
        }
    }
}

/// A retry policy as a provider's config gives it, with anything left out
/// taking dsh's default. (dsh: `RetryPolicyConfig`.)
///
/// The fields are snake_case, like the rest of a Karaka provider config,
/// where dsh's are camelCase. An `always` policy accepts the normal-only
/// fields and ignores them, as dsh does, so switching modes in a layered
/// config doesn't fail; any other unknown field is refused.
///
/// ```yaml
/// retry_policy:
///   mode: normal
///   max_retries: 2
///   backoff: { initial_delay_ms: 250, max_delay_ms: 5000 }
/// ```
#[derive(Clone, Debug, Default, PartialEq)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Deserialize),
    serde(deny_unknown_fields)
)]
pub struct RetryPolicyConfig {
    pub mode: RetryMode,
    #[cfg_attr(feature = "serde", serde(default))]
    pub max_retries: Option<u64>,
    #[cfg_attr(feature = "serde", serde(default))]
    pub retryable_codes: Option<Vec<String>>,
    #[cfg_attr(feature = "serde", serde(default))]
    pub backoff: Option<BackoffConfig>,
}

/// dsh: `mode: 'normal' | 'always'`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Deserialize),
    serde(rename_all = "lowercase")
)]
pub enum RetryMode {
    #[default]
    Normal,
    Always,
}

/// dsh: `BackoffConfig`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Deserialize),
    serde(deny_unknown_fields)
)]
pub struct BackoffConfig {
    #[cfg_attr(feature = "serde", serde(default))]
    pub initial_delay_ms: Option<f64>,
    #[cfg_attr(feature = "serde", serde(default))]
    pub max_delay_ms: Option<f64>,
    #[cfg_attr(feature = "serde", serde(default))]
    pub jitter_ratio: Option<f64>,
}

/// Why a retry policy config was refused, in dsh's words.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetryPolicyError(String);

impl fmt::Display for RetryPolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RetryPolicyError {}

impl RetryPolicyConfig {
    /// The policy this config describes, checked as dsh checks it
    /// (`resolveRetryPolicy`, `retry-policy.ts:150-195`). `path` names the
    /// config in the error, such as `anthropic.retry_policy`.
    pub fn resolve(&self, path: &str) -> Result<RetryPolicy, RetryPolicyError> {
        let backoff = self
            .backoff
            .unwrap_or_default()
            .resolve(&format!("{path}.backoff"))?;
        match self.mode {
            RetryMode::Always => Ok(RetryPolicy::Always { backoff }),
            RetryMode::Normal => {
                let retryable_codes = self
                    .retryable_codes
                    .clone()
                    .unwrap_or_else(|| DEFAULT_RETRYABLE_CODES.map(str::to_owned).to_vec());
                if retryable_codes.is_empty() {
                    return Err(error(format!("{path}.retryable_codes must not be empty")));
                }
                if retryable_codes.iter().any(String::is_empty) {
                    return Err(error(format!(
                        "{path}.retryable_codes must contain only non-empty strings"
                    )));
                }
                let mut seen = Vec::new();
                for code in &retryable_codes {
                    if seen.contains(&code) {
                        return Err(error(format!(
                            "{path}.retryable_codes must not contain duplicates"
                        )));
                    }
                    seen.push(code);
                }
                Ok(RetryPolicy::Normal {
                    max_retries: self.max_retries.unwrap_or(DEFAULT_MAX_RETRIES),
                    retryable_codes,
                    backoff,
                })
            }
        }
    }
}

impl BackoffConfig {
    /// dsh: `resolveBackoff`, `retry-policy.ts:121-141`.
    fn resolve(&self, path: &str) -> Result<Backoff, RetryPolicyError> {
        let initial_delay_ms = self.initial_delay_ms.unwrap_or(DEFAULT_INITIAL_DELAY_MS);
        let max_delay_ms = self.max_delay_ms.unwrap_or(DEFAULT_MAX_DELAY_MS);
        let jitter_ratio = self.jitter_ratio.unwrap_or(DEFAULT_JITTER_RATIO);
        let delay = |value: f64| value.is_finite() && value > 0.0 && value <= MAX_DELAY_MS;
        if !delay(initial_delay_ms) {
            return Err(error(format!(
                "{path}.initial_delay_ms must be a positive finite number no greater than {MAX_DELAY_MS}"
            )));
        }
        if !delay(max_delay_ms) {
            return Err(error(format!(
                "{path}.max_delay_ms must be a positive finite number no greater than {MAX_DELAY_MS}"
            )));
        }
        if initial_delay_ms > max_delay_ms {
            return Err(error(format!(
                "{path}.initial_delay_ms must be less than or equal to max_delay_ms"
            )));
        }
        if !(0.0..=1.0).contains(&jitter_ratio) {
            return Err(error(format!(
                "{path}.jitter_ratio must be between 0 and 1"
            )));
        }
        Ok(Backoff {
            initial_delay_ms,
            max_delay_ms,
            jitter_ratio,
        })
    }
}

fn error(message: String) -> RetryPolicyError {
    RetryPolicyError(message)
}

#[cfg(test)]
mod tests {
    use super::{
        Backoff, BackoffConfig, DEFAULT_RETRYABLE_CODES, RetryMode, RetryPolicy, RetryPolicyConfig,
    };
    use crate::ErrorKind;

    const KINDS: [ErrorKind; 10] = [
        ErrorKind::Auth,
        ErrorKind::Quota,
        ErrorKind::InvalidRequest,
        ErrorKind::ContextWindowExceeded,
        ErrorKind::Refused,
        ErrorKind::Malformed,
        ErrorKind::EmptyResponse,
        ErrorKind::RateLimited,
        ErrorKind::Unavailable,
        ErrorKind::Network,
    ];

    /// The default policy retries exactly the kinds Karaka calls retryable,
    /// by the codes the loop writes for them. If the two drifted, a failure
    /// Karaka says may pass would never be retried, or the reverse.
    #[test]
    fn the_default_policy_retries_exactly_the_retryable_kinds() {
        let policy = RetryPolicy::default();
        for kind in KINDS {
            assert_eq!(policy.covers(kind.code()), kind.is_retryable(), "{kind:?}");
        }
        // Every default code but dsh's `TIMEOUT` is some kind's code.
        for code in DEFAULT_RETRYABLE_CODES {
            let used = KINDS.iter().any(|kind| kind.code() == code);
            assert_eq!(used, code != "TIMEOUT", "{code}");
        }
    }

    #[test]
    fn the_default_is_dshs() {
        let RetryPolicy::Normal {
            max_retries,
            retryable_codes,
            backoff,
        } = RetryPolicy::default()
        else {
            panic!("the default is normal mode");
        };
        assert_eq!(max_retries, 5);
        assert_eq!(retryable_codes, DEFAULT_RETRYABLE_CODES);
        assert_eq!(
            backoff,
            Backoff {
                initial_delay_ms: 500.0,
                max_delay_ms: 10_000.0,
                jitter_ratio: 0.1
            }
        );
        assert_eq!(
            RetryPolicyConfig::default().resolve("p"),
            Ok(RetryPolicy::default())
        );
    }

    #[test]
    fn an_always_policy_ignores_the_normal_only_fields() {
        let config = RetryPolicyConfig {
            mode: RetryMode::Always,
            max_retries: Some(1),
            retryable_codes: Some(Vec::new()),
            backoff: None,
        };
        assert_eq!(
            config.resolve("p"),
            Ok(RetryPolicy::Always {
                backoff: Backoff::default()
            })
        );
    }

    #[test]
    fn a_policy_dsh_would_refuse_is_refused() {
        let normal = |codes: Vec<&str>| RetryPolicyConfig {
            retryable_codes: Some(codes.into_iter().map(str::to_owned).collect()),
            ..RetryPolicyConfig::default()
        };
        let backoff = |backoff: BackoffConfig| RetryPolicyConfig {
            backoff: Some(backoff),
            ..RetryPolicyConfig::default()
        };
        let cases = [
            (normal(vec![]), "p.retryable_codes must not be empty"),
            (
                normal(vec![""]),
                "p.retryable_codes must contain only non-empty strings",
            ),
            (
                normal(vec!["A", "A"]),
                "p.retryable_codes must not contain duplicates",
            ),
            (
                backoff(BackoffConfig {
                    initial_delay_ms: Some(0.0),
                    ..BackoffConfig::default()
                }),
                "p.backoff.initial_delay_ms must be a positive finite number no greater than 2147483647",
            ),
            (
                backoff(BackoffConfig {
                    initial_delay_ms: Some(20_000.0),
                    ..BackoffConfig::default()
                }),
                "p.backoff.initial_delay_ms must be less than or equal to max_delay_ms",
            ),
            (
                backoff(BackoffConfig {
                    jitter_ratio: Some(1.5),
                    ..BackoffConfig::default()
                }),
                "p.backoff.jitter_ratio must be between 0 and 1",
            ),
        ];
        for (config, message) in cases {
            assert_eq!(
                config.resolve("p").expect_err("refused").to_string(),
                message
            );
        }
    }
}
