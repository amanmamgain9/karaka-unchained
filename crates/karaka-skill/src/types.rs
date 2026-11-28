//! Skill names, summaries, candidates, definitions and registrations, and
//! the model-facing rendering of a loaded skill. (dsh: the types and
//! helpers of `skill/src/index.ts`.)

use std::any::Any;
use std::fmt;
use std::sync::Arc;

use karaka_fs::AbortSignal;
use karaka_scope::ScopeChain;
use karaka_session::UserSource;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

/// The standard rank of packaged skill providers and bundled local roots.
/// Lower ranks win. (dsh: `BUNDLED_SKILL_RANK`.)
pub const BUNDLED_SKILL_RANK: f64 = 600.0;

/// The rank of runtime registrations. (dsh: `RUNTIME_RANK`.)
pub const RUNTIME_RANK: f64 = 250.0;

/// The provider name runtime registrations carry by default, which no
/// provider may take. (dsh: `RUNTIME_PROVIDER`.)
pub const RUNTIME_PROVIDER: &str = "runtime";

/// The `kind` of a user-explicit skill invocation's message source. (dsh:
/// the `skill-invocation` entry of `MessageSourceMap`.)
pub const SKILL_INVOCATION_KIND: &str = "skill-invocation";

/// Whether `name` is a kebab-case skill name: lowercase letters and digits
/// in runs joined by single hyphens. (dsh: `isSkillName`, the pattern
/// `^[a-z0-9]+(?:-[a-z0-9]+)*$`.)
pub fn is_skill_name(name: &str) -> bool {
    name.split('-').all(|run| {
        !run.is_empty()
            && run
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
    })
}

/// Where a skill came from. Prompt-visible metadata, not precedence by
/// itself; any other string is allowed. (dsh: `SkillSource`.)
pub mod source {
    pub const PROJECT_DSH: &str = "project-dsh";
    pub const PROJECT_AGENTS: &str = "project-agents";
    pub const RUNTIME: &str = "runtime";
    pub const USER_DSH: &str = "user-dsh";
    pub const USER_AGENTS: &str = "user-agents";
    pub const CUSTOM: &str = "custom";
    pub const BUNDLED: &str = "bundled";
}

/// What a loaded skill's relative resources resolve against. (dsh:
/// `SkillResourceBase`.)
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum SkillResourceBase {
    Directory { path: String },
    Url { url: String },
    Opaque { description: String },
}

/// Whether models and people may invoke a skill. (dsh:
/// `SkillInvocationPolicy`.)
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillInvocationPolicy {
    /// Model-facing catalogs and loaders include it.
    pub model_invocable: bool,
    /// Human-facing command catalogs and loaders include it.
    pub user_invocable: bool,
}

impl SkillInvocationPolicy {
    /// Both surfaces, the default of a runtime registration.
    pub const BOTH: Self = Self {
        model_invocable: true,
        user_invocable: true,
    };
}

/// What carries an invocation policy. (dsh: `isModelInvocable` and
/// `isUserInvocable`, over `Pick<SkillSummary, 'invocation'>`.)
pub trait Invocable {
    fn invocation(&self) -> SkillInvocationPolicy;

    /// Whether model-facing catalogs and loaders may include it.
    fn is_model_invocable(&self) -> bool {
        self.invocation().model_invocable
    }

    /// Whether human-facing commands may include it.
    fn is_user_invocable(&self) -> bool {
        self.invocation().user_invocable
    }
}

/// Invocation-neutral metadata of a winning skill, as `list` gives it.
/// (dsh: `SkillSummary`.)
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillSummary {
    /// The instruction file's absolute path, when the provider has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub name: String,
    /// A short routing description.
    pub description: String,
    /// More routing guidance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when_to_use: Option<String>,
    pub invocation: SkillInvocationPolicy,
    pub source: String,
    /// The provider that owns the body.
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_base: Option<SkillResourceBase>,
}

impl Invocable for SkillSummary {
    fn invocation(&self) -> SkillInvocationPolicy {
        self.invocation
    }
}

/// A provider's opaque handle to a skill, given back to its `get`. (dsh:
/// `locator: unknown`.)
pub type SkillLocator = Arc<dyn Any + Send + Sync>;

/// One entry of a provider's catalog. (dsh: `SkillCandidate`.)
#[derive(Clone)]
pub struct SkillCandidate {
    pub path: Option<String>,
    pub name: String,
    pub description: String,
    pub when_to_use: Option<String>,
    pub invocation: SkillInvocationPolicy,
    pub source: String,
    pub provider: String,
    pub resource_base: Option<SkillResourceBase>,
    /// Lower ranks win a duplicate name, before registration order.
    pub rank: f64,
    pub locator: SkillLocator,
    /// The provider's parsed frontmatter metadata.
    pub metadata: Option<Map<String, Value>>,
}

impl SkillCandidate {
    /// The candidate's summary. (dsh: `toSummary`.)
    pub fn summary(&self) -> SkillSummary {
        SkillSummary {
            path: self.path.clone(),
            name: self.name.clone(),
            description: self.description.clone(),
            when_to_use: self.when_to_use.clone(),
            invocation: self.invocation,
            source: self.source.clone(),
            provider: self.provider.clone(),
            resource_base: self.resource_base.clone(),
        }
    }
}

impl Invocable for SkillCandidate {
    fn invocation(&self) -> SkillInvocationPolicy {
        self.invocation
    }
}

impl fmt::Debug for SkillCandidate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SkillCandidate")
            .field("name", &self.name)
            .field("provider", &self.provider)
            .field("source", &self.source)
            .field("rank", &self.rank)
            .finish_non_exhaustive()
    }
}

/// A whole skill, its body included, as `get` gives it. (dsh:
/// `SkillDefinition`.)
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillDefinition {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub name: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when_to_use: Option<String>,
    pub invocation: SkillInvocationPolicy,
    pub source: String,
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_base: Option<SkillResourceBase>,
    /// The Markdown instructions, without the provider's metadata.
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Map<String, Value>>,
}

impl SkillDefinition {
    /// The definition's summary. (dsh: `toSummary`.)
    pub fn summary(&self) -> SkillSummary {
        SkillSummary {
            path: self.path.clone(),
            name: self.name.clone(),
            description: self.description.clone(),
            when_to_use: self.when_to_use.clone(),
            invocation: self.invocation,
            source: self.source.clone(),
            provider: self.provider.clone(),
            resource_base: self.resource_base.clone(),
        }
    }

    /// The model-facing `<skill_content>` block. (dsh: `renderSkillContent`.)
    pub fn render(&self) -> String {
        render_skill_content(
            &self.name,
            &self.provider,
            self.resource_base.as_ref(),
            &self.content,
        )
    }
}

impl Invocable for SkillDefinition {
    fn invocation(&self) -> SkillInvocationPolicy {
        self.invocation
    }
}

/// A skill registered at runtime. Without an invocation policy it is open
/// to both surfaces, and without a provider it is the registry's own
/// `runtime`. (dsh: `SkillRegistration`.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillRegistration {
    pub path: Option<String>,
    pub name: String,
    pub description: String,
    pub when_to_use: Option<String>,
    pub invocation: Option<SkillInvocationPolicy>,
    pub source: String,
    pub provider: Option<String>,
    pub resource_base: Option<SkillResourceBase>,
    pub content: String,
    pub metadata: Option<Map<String, Value>>,
}

impl SkillRegistration {
    /// A registration with only a name, description, source and body.
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        source: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self {
            path: None,
            name: name.into(),
            description: description.into(),
            when_to_use: None,
            invocation: None,
            source: source.into(),
            provider: None,
            resource_base: None,
            content: content.into(),
            metadata: None,
        }
    }
}

/// What a read of the registry is for. (dsh: `SkillViewOptions`, with its
/// `SkillLookupOptions`.) Providers get the same options, and read only
/// `cwd` and `signal`.
#[derive(Clone, Debug, Default)]
pub struct SkillLookupOptions {
    /// The workspace the lookup is for.
    pub cwd: Option<String>,
    /// Cancels discovery or loading.
    pub signal: Option<AbortSignal>,
    /// The viewing agent's chain; the empty chain reads the global layer
    /// alone. (dsh: `scope`, the agent as its scope key.)
    pub scope: ScopeChain,
}

/// One observation of the catalog. (dsh: `SkillCatalogSnapshot`.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillCatalogSnapshot {
    /// The winning summaries, sorted by name.
    pub skills: Vec<SkillSummary>,
    /// Whether every provider completed with no change to the registry
    /// meanwhile.
    pub complete: bool,
}

/// A provider's candidates, and whether its discovery completed, so they
/// may be cached. (dsh: `SkillProviderObservation`; dsh's array shorthand
/// is [`complete`](Self::complete).)
#[derive(Clone, Debug)]
pub struct SkillProviderObservation {
    pub candidates: Vec<SkillCandidate>,
    pub complete: bool,
}

impl SkillProviderObservation {
    /// A complete discovery.
    pub fn complete(candidates: Vec<SkillCandidate>) -> Self {
        Self {
            candidates,
            complete: true,
        }
    }

    /// Usable candidates from a discovery that didn't complete: they are
    /// listed and loadable, but not cached.
    pub fn incomplete(candidates: Vec<SkillCandidate>) -> Self {
        Self {
            candidates,
            complete: false,
        }
    }
}

/// The source of the context message a user-explicit invocation of `name`
/// injects: `{ kind: "skill-invocation", name, form: "instructions" }`.
/// (dsh: `SkillInvocationSource`.)
pub fn skill_invocation_source(name: &str) -> UserSource {
    UserSource::Other(json!({
        "kind": SKILL_INVOCATION_KIND,
        "name": name,
        "form": "instructions",
    }))
}

/// One loaded skill for the model: the `<skill_content>` block the `skill`
/// tool and a user-explicit invocation share. The name is an escaped
/// attribute; the body is verbatim. (dsh: `renderSkillContent`.)
pub fn render_skill_content(
    name: &str,
    provider: &str,
    resource_base: Option<&SkillResourceBase>,
    content: &str,
) -> String {
    let hint = match resource_base {
        None => [
            format!(
                "Resources for this skill are managed by provider \"{}\".",
                escape_text(provider)
            ),
            "Load referenced resources only as needed.".to_owned(),
        ],
        Some(SkillResourceBase::Directory { path }) => [
            format!("Base directory for this skill: {}", escape_text(path)),
            "Resolve relative paths mentioned by this skill against the base directory before \
             using them. Load referenced resources only as needed."
                .to_owned(),
        ],
        Some(SkillResourceBase::Url { url }) => [
            format!("Base URL for this skill: {}", escape_text(url)),
            "Resolve relative URLs mentioned by this skill against the base URL before using \
             them. Load referenced resources only as needed."
                .to_owned(),
        ],
        Some(SkillResourceBase::Opaque { description }) => [
            format!("Resources for this skill: {}", escape_text(description)),
            "Load referenced resources only as needed.".to_owned(),
        ],
    };
    [
        format!("<skill_content name=\"{}\">", escape_attr(name)),
        "<skill_resources>".to_owned(),
        hint[0].clone(),
        hint[1].clone(),
        "</skill_resources>".to_owned(),
        String::new(),
        "<skill_instructions>".to_owned(),
        content.to_owned(),
        "</skill_instructions>".to_owned(),
        "</skill_content>".to_owned(),
    ]
    .join("\n")
}

/// dsh's `escapeAttr`: `&`, `"` and `<`.
fn escape_attr(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
}

/// Escapes prose embedded in skill markup, so it can't open or close a
/// tag: `&`, `<` and `>`. (dsh: `escapeText`.)
pub fn escape_text(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
