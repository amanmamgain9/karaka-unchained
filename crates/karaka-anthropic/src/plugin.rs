//! [`AnthropicPlugin`], which registers an [`AnthropicModel`] in a Karaka
//! runtime's [`ModelRegistry`].

use std::fmt;
use std::sync::Arc;

use karaka_kernel::{BoxError, Plugin, PluginContext, PortId};
use karaka_model::{Effort, ModelRegistry};
use serde::Deserialize;

use crate::{AnthropicModel, CacheMode, ThinkingDisplay};

/// How the plugin reaches Anthropic.
///
/// Plugins will be configured from a YAML file read at startup, and each
/// plugin's part of that file will be read into its own typed config like
/// this one. That is why it derives `Deserialize`: a missing or misspelled
/// field is then reported when the file is read, before anything starts.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnthropicConfig {
    /// The name the model is registered under, and the plugin's name.
    /// "anthropic" unless set; a second Anthropic account in one runtime
    /// needs a route of its own.
    #[serde(default = "default_route")]
    pub route: String,
    pub api_key: String,
    /// Another server, such as a proxy. `None` means Anthropic's own.
    pub base_url: Option<String>,
    /// The `max_tokens` for requests that don't set their own. `None` keeps
    /// the model's default.
    pub default_max_tokens: Option<u32>,
    /// The effort for requests that don't set their own. `None` sends none,
    /// so Anthropic uses the model's default.
    pub default_effort: Option<Effort>,
    /// Turns on adaptive thinking, shown as summaries or not at all. `None`
    /// sends no thinking setting, which keeps older models such as Haiku 4.5
    /// working, but leaves current models at their default: thinking with
    /// empty text.
    pub thinking_display: Option<ThinkingDisplay>,
    /// How long Anthropic caches the start of each conversation. Five
    /// minutes unless set.
    #[serde(default)]
    pub cache: CacheMode,
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
            .finish_non_exhaustive()
    }
}

/// Registers a model backed by Anthropic's Messages API in the
/// [`ModelRegistry`], under the config's route.
///
/// The plugin is named after its route, so two Anthropic plugins with
/// different routes can run in one runtime. Two with the same route are
/// stopped by the kernel before either starts, because their names clash.
#[derive(Debug)]
pub struct AnthropicPlugin {
    config: AnthropicConfig,
}

impl AnthropicPlugin {
    pub fn new(config: AnthropicConfig) -> Self {
        Self { config }
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
        // Every call would be refused without a key. Failing here means a
        // missing key shows up when the runtime starts, not on the first call.
        if self.config.api_key.trim().is_empty() {
            return Err("the Anthropic API key is empty".into());
        }

        let mut model = AnthropicModel::new(self.config.api_key.clone());
        if let Some(url) = &self.config.base_url {
            model = model.with_base_url(url.clone());
        }
        if let Some(max_tokens) = self.config.default_max_tokens {
            model = model.with_default_max_tokens(max_tokens);
        }
        if let Some(effort) = self.config.default_effort {
            model = model.with_default_effort(effort);
        }
        if let Some(display) = self.config.thinking_display {
            model = model.with_thinking_display(display);
        }
        model = model.with_cache(self.config.cache);
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
    use crate::{CacheMode, ThinkingDisplay};

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
        };
        assert!(!format!("{config:?}").contains("sk-secret"));
    }
}
