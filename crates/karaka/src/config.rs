//! Which plugin to start and how to ask the model, read from environment
//! variables.
//!
//! These variables are a stopgap. They will move to the YAML config file,
//! whose sections fill the plugins' typed configs directly.

use karaka_anthropic::{AnthropicConfig, AnthropicPlugin};
use karaka_kernel::DynPlugin;
use karaka_openai::{OpenAiConfig, OpenAiPlugin};

/// Used when `KARAKA_MODEL` isn't set and the provider is Anthropic.
const DEFAULT_ANTHROPIC_MODEL: &str = "claude-opus-5-5";

/// Everything `karaka ask` needs besides the question.
#[derive(Debug)]
pub struct Config {
    pub provider: Provider,
    /// The model's id at its provider.
    pub model: String,
    pub system: Option<String>,
    pub max_tokens: Option<u32>,
}

/// The model provider, with its plugin's config. Kept apart from the plugin
/// itself so tests can look inside it.
#[derive(Debug)]
pub enum Provider {
    Anthropic(AnthropicConfig),
    OpenAi(OpenAiConfig),
}

impl Provider {
    /// The plugins to start: for now, just the provider's.
    pub fn plugins(self) -> Vec<Box<dyn DynPlugin>> {
        match self {
            Self::Anthropic(config) => vec![Box::new(AnthropicPlugin::new(config))],
            Self::OpenAi(config) => vec![Box::new(OpenAiPlugin::new(config))],
        }
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
        let (provider, model) = match provider.as_deref().unwrap_or("anthropic") {
            "anthropic" => {
                let api_key = var("ANTHROPIC_API_KEY").ok_or(
                    "ANTHROPIC_API_KEY is not set. The anthropic provider needs it to call the API.",
                )?;
                let config = AnthropicConfig {
                    api_key,
                    base_url: var("ANTHROPIC_BASE_URL"),
                    // The request carries `KARAKA_MAX_TOKENS` itself.
                    default_max_tokens: None,
                };
                let model = model.unwrap_or_else(|| DEFAULT_ANTHROPIC_MODEL.to_owned());
                (Provider::Anthropic(config), model)
            }
            "openai" => {
                let base_url = var("OPENAI_BASE_URL").ok_or(
                    "OPENAI_BASE_URL is not set. It is the server's URL before /chat/completions, for example:\n  \
                     OpenAI:   https://api.openai.com/v1\n  \
                     DeepSeek: https://api.deepseek.com\n  \
                     Ollama:   http://localhost:11434/v1",
                )?;
                let config = OpenAiConfig {
                    base_url,
                    api_key: var("OPENAI_API_KEY"),
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
                    "KARAKA_PROVIDER is \"{other}\", but it must be \"anthropic\" or \"openai\""
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

        Ok(Self {
            provider,
            model,
            system: var("KARAKA_SYSTEM"),
            max_tokens,
        })
    }
}

#[cfg(test)]
mod tests {
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
        assert_eq!(config.model, "claude-opus-5-5");
        assert_eq!(config.system, None);
        assert_eq!(config.max_tokens, None);
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
        ])
        .expect("the config reads");
        let Provider::Anthropic(anthropic) = &config.provider else {
            panic!("expected anthropic, got {config:?}");
        };
        assert_eq!(anthropic.base_url.as_deref(), Some("http://localhost:8080"));
        assert_eq!(config.model, "claude-test");
        assert_eq!(config.system.as_deref(), Some("Be brief."));
        assert_eq!(config.max_tokens, Some(4096));
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
            "KARAKA_PROVIDER is \"gemini\", but it must be \"anthropic\" or \"openai\""
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
}
