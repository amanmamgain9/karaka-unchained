//! Which plugin to start and how to ask the model, read from environment
//! variables.
//!
//! These variables are a stopgap. They will move to the YAML config file,
//! whose sections fill the plugins' typed configs directly.

use std::path::PathBuf;

use karaka_anthropic::{
    AnthropicConfig, AnthropicPlugin, CacheMode, DeepSeekThinking, Profile, ThinkingDisplay,
};
use karaka_kernel::DynPlugin;
use karaka_model::{Effort, ModelsPlugin, RetryPolicyConfig};
use karaka_openai::{OpenAiConfig, OpenAiPlugin};

/// Used when `KARAKA_MODEL` isn't set and the provider is Anthropic.
const DEFAULT_ANTHROPIC_MODEL: &str = "claude-opus-5-5";

/// Used when `KARAKA_MODEL` isn't set and the provider is DeepSeek. dsh's
/// default (`bundle/base/cordis.patch.yml:94`), the first model in
/// llm-deepseek's catalog (`models.ts:8`).
const DEFAULT_DEEPSEEK_MODEL: &str = "deepseek-flash";

/// Everything `karaka ask` needs besides the question, and everything
/// `karaka chat` needs.
#[derive(Debug)]
pub struct Config {
    pub provider: Provider,
    /// The model's id at its provider.
    pub model: String,
    /// The deployment's persona prefix: the system prompt after the
    /// identity line.
    pub system: Option<String>,
    pub max_tokens: Option<u32>,
    pub effort: Option<Effort>,
    /// Where `chat` keeps its sessions, so a later chat carries on. Without
    /// it, a chat lives only as long as the process.
    pub sessions: Option<PathBuf>,
    /// The session `chat` runs in.
    pub session: Option<String>,
    /// A YAML file of agent presets (`presets.rs`). Without it, there are
    /// none.
    pub presets: Option<PathBuf>,
    /// The default preset; the file's first when unset.
    pub preset: Option<String>,
    /// A YAML file of global MCP servers (`presets.rs`).
    pub mcp_servers: Option<PathBuf>,
}

/// The model provider, with its plugin's config. Kept apart from the plugin
/// itself so tests can look inside it.
#[derive(Debug)]
pub enum Provider {
    Anthropic(AnthropicConfig),
    /// DeepSeek, through the Anthropic crate's DeepSeek profile, as dsh v4
    /// talks to it.
    DeepSeek(AnthropicConfig),
    OpenAi(OpenAiConfig),
}

impl Provider {
    /// The route the provider's model is registered under.
    pub fn route(&self) -> &str {
        match self {
            Self::Anthropic(config) | Self::DeepSeek(config) => &config.route,
            Self::OpenAi(config) => &config.route,
        }
    }

    /// The plugins to start: the model registry, and the provider's plugin,
    /// which registers its model there.
    pub fn plugins(self) -> Vec<Box<dyn DynPlugin>> {
        let provider: Box<dyn DynPlugin> = match self {
            Self::Anthropic(config) | Self::DeepSeek(config) => {
                Box::new(AnthropicPlugin::new(config))
            }
            Self::OpenAi(config) => Box::new(OpenAiPlugin::new(config)),
        };
        vec![Box::new(ModelsPlugin), provider]
    }
}

impl Config {
    /// Reads every setting through `lookup`, which finds one environment
    /// variable. `main` passes the real environment; tests pass a fixed list,
    /// so they never change the process's own.
    ///
    /// A variable that is empty, or only spaces, counts as not set, as it
    /// would in a shell line like `OPENAI_API_KEY= karaka ask ...`.
    pub fn from_env(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let var = |name: &str| lookup(name).filter(|value| !value.trim().is_empty());

        let model = var("KARAKA_MODEL");
        let provider = var("KARAKA_PROVIDER");
        let provider = provider.as_deref().unwrap_or("anthropic");
        // The provider's own name unless set, so `KARAKA_ROUTE=deepseek`
        // only needs saying when it helps someone read the setup.
        let route = var("KARAKA_ROUTE").unwrap_or_else(|| provider.to_owned());
        // The provider's retry policy: dsh's default, or as many retries of
        // transient failures as this says. 0 turns retries off.
        let retry_policy = match var("KARAKA_MAX_RETRIES") {
            None => None,
            Some(text) => Some(RetryPolicyConfig {
                max_retries: Some(text.trim().parse::<u64>().map_err(|_| {
                    format!(
                        "KARAKA_MAX_RETRIES is \"{text}\", but it must be a whole number, such as 0 or 5"
                    )
                })?),
                ..RetryPolicyConfig::default()
            }),
        };
        let (provider, model) = match provider {
            "anthropic" => {
                let api_key = var("ANTHROPIC_API_KEY").ok_or(
                    "ANTHROPIC_API_KEY is not set. The anthropic provider needs it to call the API.",
                )?;
                let thinking_display = match var("ANTHROPIC_THINKING_DISPLAY") {
                    Some(value) => choice(
                        "ANTHROPIC_THINKING_DISPLAY",
                        &value,
                        &[
                            ("summarized", ThinkingDisplay::Summarized),
                            ("omitted", ThinkingDisplay::Omitted),
                        ],
                    )?,
                    // The answer printer shows reasoning, dimmed, so ask for
                    // reasoning it can show. Anthropic's own default sends
                    // it empty.
                    None => ThinkingDisplay::Summarized,
                };
                let cache = match var("ANTHROPIC_CACHE") {
                    Some(value) => choice(
                        "ANTHROPIC_CACHE",
                        &value,
                        &[
                            ("short", CacheMode::Short),
                            ("long", CacheMode::Long),
                            ("off", CacheMode::Off),
                        ],
                    )?,
                    None => CacheMode::default(),
                };
                let config = AnthropicConfig {
                    route,
                    api_key,
                    base_url: var("ANTHROPIC_BASE_URL"),
                    // The request carries `KARAKA_MAX_TOKENS` and
                    // `KARAKA_EFFORT` itself.
                    default_max_tokens: None,
                    default_effort: None,
                    thinking_display: Some(thinking_display),
                    cache,
                    profile: Profile::Anthropic,
                    thinking: None,
                    retry_policy: retry_policy.clone(),
                };
                let model = model.unwrap_or_else(|| DEFAULT_ANTHROPIC_MODEL.to_owned());
                (Provider::Anthropic(config), model)
            }
            "deepseek" => {
                let api_key = var("DEEPSEEK_API_KEY").ok_or(
                    "DEEPSEEK_API_KEY is not set. The deepseek provider needs it to call the API.",
                )?;
                // Unset leaves thinking on, as in dsh.
                let thinking = match var("DEEPSEEK_THINKING") {
                    Some(value) => Some(choice(
                        "DEEPSEEK_THINKING",
                        &value,
                        &[
                            ("enabled", DeepSeekThinking::Enabled),
                            ("disabled", DeepSeekThinking::Disabled),
                        ],
                    )?),
                    None => None,
                };
                let config = AnthropicConfig {
                    route,
                    api_key,
                    // dsh reads the same variable (`config.ts:118`).
                    base_url: var("DEEPSEEK_BASE_URL"),
                    // The request carries `KARAKA_MAX_TOKENS` and
                    // `KARAKA_EFFORT` itself. Without them, DeepSeek's
                    // profile sends dsh's defaults: 256,000 and high.
                    default_max_tokens: None,
                    default_effort: None,
                    thinking_display: None,
                    cache: CacheMode::default(),
                    profile: Profile::DeepSeek,
                    thinking,
                    retry_policy: retry_policy.clone(),
                };
                let model = model.unwrap_or_else(|| DEFAULT_DEEPSEEK_MODEL.to_owned());
                (Provider::DeepSeek(config), model)
            }
            "openai" => {
                let base_url = var("OPENAI_BASE_URL").ok_or(
                    "OPENAI_BASE_URL is not set. It is the server's URL before /chat/completions, for example:\n  \
                     OpenAI:   https://api.openai.com/v1\n  \
                     DeepSeek: https://api.deepseek.com\n  \
                     Ollama:   http://localhost:11434/v1",
                )?;
                let config = OpenAiConfig {
                    route,
                    base_url,
                    api_key: var("OPENAI_API_KEY"),
                    default_effort: None,
                    thinking: None,
                    max_tokens_field: Default::default(),
                    system_messages_in_history: false,
                    retry_policy: retry_policy.clone(),
                };
                // Each OpenAI-compatible server has its own models, so there
                // is no default that would work everywhere.
                let model = model.ok_or(
                    "KARAKA_MODEL is not set. The openai provider has no default model, so name the one the server should run.",
                )?;
                (Provider::OpenAi(config), model)
            }
            other => {
                return Err(format!(
                    "KARAKA_PROVIDER is \"{other}\", but it must be \"anthropic\", \"deepseek\" or \"openai\""
                ));
            }
        };

        let max_tokens = match var("KARAKA_MAX_TOKENS") {
            None => None,
            Some(text) => Some(
                text.trim()
                    .parse::<u32>()
                    .ok()
                    .filter(|&tokens| tokens > 0)
                    .ok_or_else(|| {
                        format!(
                            "KARAKA_MAX_TOKENS is \"{text}\", but it must be a whole number above 0, such as 4096"
                        )
                    })?,
            ),
        };

        let effort = match var("KARAKA_EFFORT") {
            None => None,
            Some(value) => Some(choice(
                "KARAKA_EFFORT",
                &value,
                &[
                    ("low", Effort::Low),
                    ("medium", Effort::Medium),
                    ("high", Effort::High),
                    ("xhigh", Effort::XHigh),
                    ("max", Effort::Max),
                ],
            )?),
        };

        Ok(Self {
            provider,
            model,
            system: var("KARAKA_SYSTEM"),
            max_tokens,
            effort,
            sessions: var("KARAKA_SESSIONS").map(PathBuf::from),
            session: var("KARAKA_SESSION"),
            presets: var("KARAKA_PRESETS").map(PathBuf::from),
            preset: var("KARAKA_PRESET"),
            mcp_servers: var("KARAKA_MCP_SERVERS").map(PathBuf::from),
        })
    }
}

/// The value matching `text` among `options`, for a variable that takes one
/// of a few names. The error lists them all.
fn choice<T: Copy>(variable: &str, text: &str, options: &[(&str, T)]) -> Result<T, String> {
    let wanted = text.trim();
    options
        .iter()
        .find(|(name, _)| *name == wanted)
        .map(|&(_, value)| value)
        .ok_or_else(|| {
            let names: Vec<&str> = options.iter().map(|&(name, _)| name).collect();
            let (last, rest) = names.split_last().expect("there is always an option");
            format!(
                "{variable} is \"{text}\", but it must be {} or {last}",
                rest.join(", ")
            )
        })
}

#[cfg(test)]
mod tests {
    use karaka_anthropic::{CacheMode, DeepSeekThinking, Profile, ThinkingDisplay};
    use karaka_model::Effort;

    use super::{Config, Provider};

    /// Reads the config from `vars` alone, as if they were the whole
    /// environment.
    fn config(vars: &[(&str, &str)]) -> Result<Config, String> {
        Config::from_env(|name| {
            vars.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.to_string())
        })
    }

    #[test]
    fn anthropic_is_the_default_and_needs_only_a_key() {
        let config = config(&[("ANTHROPIC_API_KEY", "sk-test")]).expect("the config reads");
        let Provider::Anthropic(anthropic) = &config.provider else {
            panic!("expected anthropic, got {config:?}");
        };
        assert_eq!(anthropic.api_key, "sk-test");
        assert_eq!(anthropic.base_url, None);
        assert_eq!(config.provider.route(), "anthropic");
        assert_eq!(config.model, "claude-opus-5-5");
        assert_eq!(config.system, None);
        assert_eq!(config.max_tokens, None);
        assert_eq!(config.effort, None);
        // Summaries by default, so the dimmed reasoning has text in it.
        assert_eq!(
            anthropic.thinking_display,
            Some(ThinkingDisplay::Summarized)
        );
        assert_eq!(anthropic.cache, CacheMode::Short);
    }

    #[test]
    fn every_anthropic_setting_is_read() {
        let config = config(&[
            ("KARAKA_PROVIDER", "anthropic"),
            ("KARAKA_MODEL", "claude-test"),
            ("KARAKA_SYSTEM", "Be brief."),
            ("KARAKA_MAX_TOKENS", "4096"),
            ("ANTHROPIC_API_KEY", "sk-test"),
            ("ANTHROPIC_BASE_URL", "http://localhost:8080"),
            ("KARAKA_EFFORT", "xhigh"),
            ("ANTHROPIC_THINKING_DISPLAY", "omitted"),
            ("ANTHROPIC_CACHE", "long"),
        ])
        .expect("the config reads");
        let Provider::Anthropic(anthropic) = &config.provider else {
            panic!("expected anthropic, got {config:?}");
        };
        assert_eq!(anthropic.base_url.as_deref(), Some("http://localhost:8080"));
        assert_eq!(anthropic.thinking_display, Some(ThinkingDisplay::Omitted));
        assert_eq!(anthropic.cache, CacheMode::Long);
        assert_eq!(config.model, "claude-test");
        assert_eq!(config.system.as_deref(), Some("Be brief."));
        assert_eq!(config.max_tokens, Some(4096));
        assert_eq!(config.effort, Some(Effort::XHigh));
    }

    #[test]
    fn every_effort_level_is_read_by_name() {
        let levels = [
            ("low", Effort::Low),
            ("medium", Effort::Medium),
            ("high", Effort::High),
            ("xhigh", Effort::XHigh),
            ("max", Effort::Max),
        ];
        for (name, effort) in levels {
            let config = config(&[("ANTHROPIC_API_KEY", "k"), ("KARAKA_EFFORT", name)])
                .expect("the config reads");
            assert_eq!(config.effort, Some(effort), "{name}");
        }
    }

    #[test]
    fn the_effort_applies_to_openai_too() {
        let config = config(&[
            ("KARAKA_PROVIDER", "openai"),
            ("KARAKA_MODEL", "deepseek-test"),
            ("OPENAI_BASE_URL", "https://api.deepseek.com"),
            ("KARAKA_EFFORT", "max"),
        ])
        .expect("the config reads");
        assert_eq!(config.effort, Some(Effort::Max));
    }

    #[test]
    fn an_unknown_effort_lists_the_levels() {
        let error = config(&[("ANTHROPIC_API_KEY", "k"), ("KARAKA_EFFORT", "huge")])
            .expect_err("the config is rejected");
        assert_eq!(
            error,
            "KARAKA_EFFORT is \"huge\", but it must be low, medium, high, xhigh or max"
        );
    }

    #[test]
    fn caching_can_be_turned_off() {
        let config = config(&[("ANTHROPIC_API_KEY", "k"), ("ANTHROPIC_CACHE", "off")])
            .expect("the config reads");
        let Provider::Anthropic(anthropic) = &config.provider else {
            panic!("expected anthropic, got {config:?}");
        };
        assert_eq!(anthropic.cache, CacheMode::Off);
    }

    #[test]
    fn unknown_anthropic_choices_are_rejected() {
        let error = config(&[
            ("ANTHROPIC_API_KEY", "k"),
            ("ANTHROPIC_THINKING_DISPLAY", "full"),
        ])
        .expect_err("the config is rejected");
        assert_eq!(
            error,
            "ANTHROPIC_THINKING_DISPLAY is \"full\", but it must be summarized or omitted"
        );

        let error = config(&[("ANTHROPIC_API_KEY", "k"), ("ANTHROPIC_CACHE", "forever")])
            .expect_err("the config is rejected");
        assert_eq!(
            error,
            "ANTHROPIC_CACHE is \"forever\", but it must be short, long or off"
        );
    }

    #[test]
    fn a_missing_anthropic_key_is_named() {
        let error = config(&[]).expect_err("the config is rejected");
        assert!(error.starts_with("ANTHROPIC_API_KEY is not set"), "{error}");
    }

    #[test]
    fn an_empty_variable_counts_as_not_set() {
        let error = config(&[("ANTHROPIC_API_KEY", "")]).expect_err("the config is rejected");
        assert!(error.starts_with("ANTHROPIC_API_KEY is not set"), "{error}");
    }

    #[test]
    fn openai_needs_a_base_url_and_a_model_but_no_key() {
        let config = config(&[
            ("KARAKA_PROVIDER", "openai"),
            ("KARAKA_MODEL", "llama3"),
            ("OPENAI_BASE_URL", "http://localhost:11434/v1"),
        ])
        .expect("the config reads");
        let Provider::OpenAi(openai) = &config.provider else {
            panic!("expected openai, got {config:?}");
        };
        assert_eq!(openai.base_url, "http://localhost:11434/v1");
        assert_eq!(openai.api_key, None);
        assert_eq!(config.model, "llama3");
        // With no route set, the route is the provider's name.
        assert_eq!(config.provider.route(), "openai");
    }

    #[test]
    fn a_route_names_the_provider_it_is_set_for() {
        let deepseek = config(&[
            ("KARAKA_PROVIDER", "openai"),
            ("KARAKA_ROUTE", "deepseek"),
            ("KARAKA_MODEL", "deepseek-chat"),
            ("OPENAI_BASE_URL", "https://api.deepseek.com"),
        ])
        .expect("the config reads");
        assert_eq!(deepseek.provider.route(), "deepseek");

        let team = config(&[("ANTHROPIC_API_KEY", "k"), ("KARAKA_ROUTE", "team")])
            .expect("the config reads");
        assert_eq!(team.provider.route(), "team");
    }

    #[test]
    fn the_plugins_are_the_registry_and_the_provider() {
        let config = config(&[("ANTHROPIC_API_KEY", "k"), ("KARAKA_ROUTE", "team")])
            .expect("the config reads");
        let names: Vec<String> = config
            .provider
            .plugins()
            .iter()
            .map(|plugin| plugin.name().to_owned())
            .collect();
        assert_eq!(names, ["models", "team"]);
    }

    #[test]
    fn openai_reads_its_key_when_set() {
        let config = config(&[
            ("KARAKA_PROVIDER", "openai"),
            ("KARAKA_MODEL", "gpt-test"),
            ("OPENAI_BASE_URL", "https://api.openai.com/v1"),
            ("OPENAI_API_KEY", "sk-test"),
        ])
        .expect("the config reads");
        let Provider::OpenAi(openai) = &config.provider else {
            panic!("expected openai, got {config:?}");
        };
        assert_eq!(openai.api_key.as_deref(), Some("sk-test"));
    }

    #[test]
    fn a_missing_openai_base_url_shows_examples() {
        let error = config(&[("KARAKA_PROVIDER", "openai"), ("KARAKA_MODEL", "m")])
            .expect_err("the config is rejected");
        assert!(error.starts_with("OPENAI_BASE_URL is not set"), "{error}");
        assert!(error.contains("https://api.openai.com/v1"), "{error}");
        assert!(error.contains("https://api.deepseek.com"), "{error}");
        assert!(error.contains("http://localhost:11434/v1"), "{error}");
    }

    #[test]
    fn openai_has_no_default_model() {
        let error = config(&[
            ("KARAKA_PROVIDER", "openai"),
            ("OPENAI_BASE_URL", "http://localhost:11434/v1"),
        ])
        .expect_err("the config is rejected");
        assert!(error.starts_with("KARAKA_MODEL is not set"), "{error}");
    }

    #[test]
    fn an_unknown_provider_is_rejected() {
        let error = config(&[("KARAKA_PROVIDER", "gemini")]).expect_err("the config is rejected");
        assert_eq!(
            error,
            "KARAKA_PROVIDER is \"gemini\", but it must be \"anthropic\", \"deepseek\" or \"openai\""
        );
    }

    #[test]
    fn max_retries_sets_the_providers_retry_policy() {
        let limited = config(&[("ANTHROPIC_API_KEY", "k"), ("KARAKA_MAX_RETRIES", "0")])
            .expect("the config reads");
        let Provider::Anthropic(anthropic) = limited.provider else {
            panic!("expected anthropic");
        };
        assert_eq!(
            anthropic.retry_policy.and_then(|policy| policy.max_retries),
            Some(0)
        );
        let unset = Config::from_env(|name| (name == "ANTHROPIC_API_KEY").then(|| "k".into()))
            .expect("the config reads");
        let Provider::Anthropic(anthropic) = unset.provider else {
            panic!("expected anthropic");
        };
        assert_eq!(anthropic.retry_policy, None);
        let error = config(&[("ANTHROPIC_API_KEY", "k"), ("KARAKA_MAX_RETRIES", "lots")])
            .expect_err("not a number");
        assert!(
            error.starts_with("KARAKA_MAX_RETRIES is \"lots\""),
            "{error}"
        );
    }

    #[test]
    fn max_tokens_must_be_a_positive_number() {
        for bad in ["lots", "-1", "0", "1.5"] {
            let error = config(&[("ANTHROPIC_API_KEY", "k"), ("KARAKA_MAX_TOKENS", bad)])
                .expect_err("the config is rejected");
            assert!(error.starts_with("KARAKA_MAX_TOKENS is"), "{bad}: {error}");
        }
    }

    #[test]
    fn debug_output_leaves_out_the_keys() {
        let config = config(&[("ANTHROPIC_API_KEY", "sk-secret")]).expect("the config reads");
        assert!(!format!("{config:?}").contains("sk-secret"));
    }

    #[test]
    fn deepseek_needs_only_a_key_and_speaks_the_messages_format() {
        let config = config(&[
            ("KARAKA_PROVIDER", "deepseek"),
            ("DEEPSEEK_API_KEY", "sk-test"),
        ])
        .expect("the config reads");
        let Provider::DeepSeek(deepseek) = &config.provider else {
            panic!("expected deepseek, got {config:?}");
        };
        assert_eq!(deepseek.profile, Profile::DeepSeek);
        assert_eq!(deepseek.api_key, "sk-test");
        assert_eq!(deepseek.base_url, None);
        assert_eq!(deepseek.thinking, None);
        assert_eq!(deepseek.thinking_display, None);
        assert_eq!(deepseek.default_max_tokens, None);
        assert_eq!(config.provider.route(), "deepseek");
        assert_eq!(config.model, "deepseek-flash");
    }

    #[test]
    fn every_deepseek_setting_is_read() {
        let config = config(&[
            ("KARAKA_PROVIDER", "deepseek"),
            ("KARAKA_ROUTE", "deepseek-team"),
            ("KARAKA_MODEL", "deepseek-v4-pro"),
            ("KARAKA_MAX_TOKENS", "393216"),
            ("KARAKA_EFFORT", "max"),
            ("DEEPSEEK_API_KEY", "sk-test"),
            ("DEEPSEEK_BASE_URL", "http://localhost:8080/anthropic"),
            ("DEEPSEEK_THINKING", "disabled"),
        ])
        .expect("the config reads");
        let Provider::DeepSeek(deepseek) = &config.provider else {
            panic!("expected deepseek, got {config:?}");
        };
        assert_eq!(
            deepseek.base_url.as_deref(),
            Some("http://localhost:8080/anthropic")
        );
        assert_eq!(deepseek.thinking, Some(DeepSeekThinking::Disabled));
        assert_eq!(config.provider.route(), "deepseek-team");
        assert_eq!(config.model, "deepseek-v4-pro");
        assert_eq!(config.max_tokens, Some(393_216));
        assert_eq!(config.effort, Some(Effort::Max));
    }

    #[test]
    fn a_missing_deepseek_key_is_named() {
        let error = config(&[("KARAKA_PROVIDER", "deepseek")]).expect_err("the config is rejected");
        assert!(error.starts_with("DEEPSEEK_API_KEY is not set"), "{error}");
    }

    #[test]
    fn an_unknown_deepseek_thinking_setting_is_rejected() {
        let error = config(&[
            ("KARAKA_PROVIDER", "deepseek"),
            ("DEEPSEEK_API_KEY", "k"),
            ("DEEPSEEK_THINKING", "on"),
        ])
        .expect_err("the config is rejected");
        assert_eq!(
            error,
            "DEEPSEEK_THINKING is \"on\", but it must be enabled or disabled"
        );
    }

    #[test]
    fn the_deepseek_plugin_is_named_after_its_route() {
        let config = config(&[("KARAKA_PROVIDER", "deepseek"), ("DEEPSEEK_API_KEY", "k")])
            .expect("the config reads");
        let names: Vec<String> = config
            .provider
            .plugins()
            .iter()
            .map(|plugin| plugin.name().to_owned())
            .collect();
        assert_eq!(names, ["models", "deepseek"]);
    }

    #[test]
    fn debug_output_leaves_out_the_deepseek_key() {
        let config = config(&[
            ("KARAKA_PROVIDER", "deepseek"),
            ("DEEPSEEK_API_KEY", "sk-secret"),
        ])
        .expect("the config reads");
        assert!(!format!("{config:?}").contains("sk-secret"));
    }
}
