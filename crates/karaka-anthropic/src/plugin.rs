//! [`AnthropicPlugin`], which registers an [`AnthropicModel`] in a Karaka
//! runtime's [`ModelRegistry`].

use std::fmt;
use std::sync::Arc;

use karaka_kernel::{BoxError, Plugin, PluginContext, PortId};
use karaka_model::{Effort, ModelRegistry, RetryPolicyConfig};
use serde::Deserialize;

use crate::{AnthropicModel, CacheMode, DeepSeekThinking, Profile, ThinkingDisplay, deepseek};

/// How the plugin reaches Anthropic, or DeepSeek.
///
/// Plugins will be configured from a YAML file read at startup, and each
/// plugin's part of that file will be read into its own typed config like
/// this one. That is why it derives `Deserialize`: a missing or misspelled
/// field is then reported when the file is read, before anything starts.
///
/// Play This Life's DeepSeek setup, in the shape this config reads:
///
/// ```yaml
/// route: deepseek
/// profile: deepseek
/// api_key: ...
/// thinking: enabled
/// default_effort: max
/// default_max_tokens: 393216
/// ```
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnthropicConfig {
    /// The name the model is registered under, and the plugin's name.
    /// "anthropic" unless set; a second Anthropic account in one runtime
    /// needs a route of its own, and the DeepSeek profile would normally
    /// use "deepseek".
    #[serde(default = "default_route")]
    pub route: String,
    pub api_key: String,
    /// Another server, such as a proxy. `None` means the profile's own:
    /// Anthropic's, or DeepSeek's `https://api.deepseek.com/anthropic`.
    pub base_url: Option<String>,
    /// The `max_tokens` for requests that don't set their own. `None` keeps
    /// the profile's default: 64,000 for Anthropic, 256,000 for DeepSeek.
    pub default_max_tokens: Option<u32>,
    /// The effort for requests that don't set their own. `None` sends none,
    /// so Anthropic uses the model's default; DeepSeek gets `high`. DeepSeek
    /// takes only low, high and max.
    pub default_effort: Option<Effort>,
    /// Anthropic only. Turns on adaptive thinking, shown as summaries or
    /// not at all. `None` sends no thinking setting, which keeps older
    /// models such as Haiku 4.5 working, but leaves current models at their
    /// default: thinking with empty text.
    pub thinking_display: Option<ThinkingDisplay>,
    /// Anthropic only. How long Anthropic caches the start of each
    /// conversation. Five minutes unless set. The DeepSeek profile sends no
    /// cache markers, whatever this says.
    #[serde(default)]
    pub cache: CacheMode,
    /// Whose Messages API to talk to. Anthropic unless set.
    #[serde(default)]
    pub profile: Profile,
    /// DeepSeek only. Turns thinking on or off. `None` leaves it on, as dsh
    /// does. With it off, no effort can be set.
    pub thinking: Option<DeepSeekThinking>,
    /// How failed requests to this provider should be retried, as dsh's
    /// provider configs hold a `retryPolicy`. dsh's default unless set:
    /// five retries of transient failures.
    #[serde(default)]
    pub retry_policy: Option<RetryPolicyConfig>,
}

/// The route when the config doesn't name one.
fn default_route() -> String {
    "anthropic".to_owned()
}

/// Leaves out the API key, so a config can be logged safely.
impl fmt::Debug for AnthropicConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AnthropicConfig")
            .field("route", &self.route)
            .field("base_url", &self.base_url)
            .field("default_max_tokens", &self.default_max_tokens)
            .field("default_effort", &self.default_effort)
            .field("thinking_display", &self.thinking_display)
            .field("cache", &self.cache)
            .field("profile", &self.profile)
            .field("thinking", &self.thinking)
            .field("retry_policy", &self.retry_policy)
            .finish_non_exhaustive()
    }
}

/// Registers a model backed by Anthropic's Messages API, or DeepSeek's, in
/// the [`ModelRegistry`], under the config's route.
///
/// The plugin is named after its route, so two Anthropic plugins with
/// different routes can run in one runtime, and so can an Anthropic one
/// beside a DeepSeek one. Two with the same route are stopped by the kernel
/// before either starts, because their names clash.
#[derive(Debug)]
pub struct AnthropicPlugin {
    config: AnthropicConfig,
}

impl AnthropicPlugin {
    pub fn new(config: AnthropicConfig) -> Self {
        Self { config }
    }

    /// The model for the Anthropic profile.
    fn anthropic(&self) -> Result<AnthropicModel, BoxError> {
        let config = &self.config;
        // Every call would be refused without a key. Failing here means a
        // missing key shows up when the runtime starts, not on the first call.
        if config.api_key.trim().is_empty() {
            return Err("the Anthropic API key is empty".into());
        }
        if config.thinking.is_some() {
            return Err(
                "`thinking` is for the deepseek profile; the anthropic profile uses `thinking_display`"
                    .into(),
            );
        }

        let mut model = AnthropicModel::new(config.api_key.clone());
        if let Some(url) = &config.base_url {
            model = model.with_base_url(url.clone());
        }
        if let Some(max_tokens) = config.default_max_tokens {
            model = model.with_default_max_tokens(max_tokens);
        }
        if let Some(effort) = config.default_effort {
            model = model.with_default_effort(effort);
        }
        if let Some(display) = config.thinking_display {
            model = model.with_thinking_display(display);
        }
        Ok(model.with_cache(config.cache))
    }

    /// The model for the DeepSeek profile. dsh checks its config when it
    /// loads (`config.ts:214-304`); the checks here are the ones that apply
    /// to the settings Karaka has.
    fn deepseek(&self) -> Result<AnthropicModel, BoxError> {
        let config = &self.config;
        if config.api_key.trim().is_empty() {
            return Err("the DeepSeek API key is empty".into());
        }
        if config.thinking_display.is_some() {
            return Err(
                "`thinking_display` is for the anthropic profile; the deepseek profile turns thinking on or off with `thinking`"
                    .into(),
            );
        }
        let thinking = config.thinking.unwrap_or_default();
        if let Some(effort) = config.default_effort {
            let name = deepseek::effort_label(effort);
            // dsh's config allows only these levels (`config.ts:94`).
            if deepseek::effort_name(effort).is_none() {
                return Err(format!(
                    "the default effort is \"{name}\", but DeepSeek takes only low, high or max"
                )
                .into());
            }
            // No request could work with both (`config.ts:219-223`).
            if thinking == DeepSeekThinking::Disabled {
                return Err(format!(
                    "the default effort is \"{name}\", but thinking is disabled, and DeepSeek takes no effort then"
                )
                .into());
            }
        }

        let mut model = AnthropicModel::deepseek(config.api_key.clone());
        if let Some(url) = &config.base_url {
            check_base_url(url)?;
            model = model.with_base_url(url.clone());
        }
        if let Some(max_tokens) = config.default_max_tokens {
            model = model.with_default_max_tokens(max_tokens);
        }
        if let Some(effort) = config.default_effort {
            model = model.with_default_effort(effort);
        }
        Ok(model.with_deepseek_thinking(thinking))
    }
}

/// Refuses a base URL dsh would refuse: one that isn't HTTP or HTTPS, or
/// that holds a user name, a password, a query or a fragment
/// (`config.ts:301-304`). A key in the URL would end up in logs.
fn check_base_url(url: &str) -> Result<(), BoxError> {
    let parsed = reqwest::Url::parse(url)
        .map_err(|error| format!("the base URL \"{url}\" isn't a URL: {error}"))?;
    let plain = matches!(parsed.scheme(), "http" | "https")
        && parsed.username().is_empty()
        && parsed.password().is_none()
        && parsed.query().is_none()
        && parsed.fragment().is_none();
    if plain {
        Ok(())
    } else {
        Err(format!(
            "the base URL \"{url}\" must be plain HTTP or HTTPS, with no user name, password, query or fragment"
        )
        .into())
    }
}

impl Plugin for AnthropicPlugin {
    fn name(&self) -> &str {
        &self.config.route
    }

    fn requires(&self) -> Vec<PortId> {
        vec![PortId::of::<ModelRegistry>()]
    }

    async fn setup(&self, ctx: &mut PluginContext<'_>) -> Result<(), BoxError> {
        let model = match self.config.profile {
            Profile::Anthropic => self.anthropic()?,
            Profile::DeepSeek => self.deepseek()?,
        };
        let model = match &self.config.retry_policy {
            Some(policy) => {
                let path = format!("{}.retry_policy", self.config.route);
                model.with_retry_policy(policy.resolve(&path)?)
            }
            None => model,
        };
        // An empty or taken route fails here. Its error becomes this
        // plugin's setup error, which the runtime reports by plugin name.
        ctx.get::<ModelRegistry>()
            .register(self.config.route.clone(), Arc::new(model))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use karaka_model::Effort;

    use super::AnthropicConfig;
    use crate::{CacheMode, DeepSeekThinking, Profile, ThinkingDisplay};

    #[test]
    fn the_config_reads_with_only_the_key_given() {
        let config: AnthropicConfig =
            serde_json::from_str(r#"{"api_key": "sk-secret"}"#).expect("the config reads");
        assert_eq!(config.api_key, "sk-secret");
        assert_eq!(config.route, "anthropic");
        assert_eq!(config.base_url, None);
        assert_eq!(config.default_max_tokens, None);
        assert_eq!(config.default_effort, None);
        assert_eq!(config.thinking_display, None);
        assert_eq!(config.cache, CacheMode::Short);
    }

    #[test]
    fn the_config_reads_effort_thinking_and_cache_by_name() {
        let config: AnthropicConfig = serde_json::from_str(
            r#"{"api_key": "k", "default_effort": "xhigh", "thinking_display": "summarized", "cache": "long"}"#,
        )
        .expect("the config reads");
        assert_eq!(config.default_effort, Some(Effort::XHigh));
        assert_eq!(config.thinking_display, Some(ThinkingDisplay::Summarized));
        assert_eq!(config.cache, CacheMode::Long);
    }

    #[test]
    fn the_config_reads_a_route_of_its_own() {
        let config: AnthropicConfig =
            serde_json::from_str(r#"{"route": "anthropic-team", "api_key": "k"}"#)
                .expect("the config reads");
        assert_eq!(config.route, "anthropic-team");
    }

    #[test]
    fn an_unknown_effort_is_rejected() {
        let error = serde_json::from_str::<AnthropicConfig>(
            r#"{"api_key": "k", "default_effort": "huge"}"#,
        )
        .expect_err("the config is rejected");
        assert!(error.to_string().contains("huge"), "{error}");
    }

    #[test]
    fn a_misspelled_field_is_rejected() {
        let error = serde_json::from_str::<AnthropicConfig>(r#"{"api_key": "k", "base_ulr": "x"}"#)
            .expect_err("the config is rejected");
        assert!(error.to_string().contains("base_ulr"), "{error}");
    }

    #[test]
    fn debug_output_leaves_out_the_api_key() {
        let config = AnthropicConfig {
            route: "anthropic".into(),
            api_key: "sk-secret".into(),
            base_url: None,
            default_max_tokens: None,
            default_effort: None,
            thinking_display: None,
            cache: CacheMode::Short,
            profile: Profile::Anthropic,
            thinking: None,
            retry_policy: None,
        };
        assert!(!format!("{config:?}").contains("sk-secret"));
    }

    #[test]
    fn the_profile_is_anthropic_unless_set() {
        let config: AnthropicConfig =
            serde_json::from_str(r#"{"api_key": "k"}"#).expect("the config reads");
        assert_eq!(config.profile, Profile::Anthropic);
        assert_eq!(config.thinking, None);
    }

    #[test]
    fn the_config_reads_a_deepseek_profile_by_name() {
        let config: AnthropicConfig = serde_json::from_str(
            r#"{"route": "deepseek", "profile": "deepseek", "api_key": "k", "thinking": "disabled", "default_max_tokens": 393216}"#,
        )
        .expect("the config reads");
        assert_eq!(config.route, "deepseek");
        assert_eq!(config.profile, Profile::DeepSeek);
        assert_eq!(config.thinking, Some(DeepSeekThinking::Disabled));
        assert_eq!(config.default_max_tokens, Some(393_216));
    }

    #[test]
    fn an_unknown_profile_is_rejected() {
        let error =
            serde_json::from_str::<AnthropicConfig>(r#"{"api_key": "k", "profile": "openai"}"#)
                .expect_err("the config is rejected");
        assert!(error.to_string().contains("openai"), "{error}");
    }
}
