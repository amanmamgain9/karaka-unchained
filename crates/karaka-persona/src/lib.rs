//! A preset's persona: its own identity, shadowing the deployment persona
//! for its agents. (dsh: `@deepseek-ai/dsh-persona`,
//! `preset/persona/src/index.ts`.)
//!
//! The system prompt registers the deployment persona itself, from its
//! config, in the global layer: the [`PERSONA_PREFIX_SECTION`] and the
//! [`PERSONA_SUFFIX_SECTION`]. A [`Persona`] registers sections of the same
//! names in a scope, so they shadow the deployment's for the agents whose
//! chain holds it. Mounted globally, it collides with the system prompt's
//! own registration and fails loud, as dsh's does. A preset can't mount the
//! system prompt itself, so without this plugin a preset could change an
//! agent's tools but never its identity.
//!
//! Its texts are templates: a `{{name}}` interpolates strictly against the
//! prompt's variables when the prompt renders, like any other section's.

use karaka_agent_presets::{PresetContext, PresetFuture, PresetPlugin};
use karaka_kernel::PortId;
use karaka_scope::ScopeKey;
pub use karaka_system_prompt::{PERSONA_PREFIX_SECTION, PERSONA_SUFFIX_SECTION};
use karaka_system_prompt::{PromptError, Section, SectionOrder, SystemPrompt};

/// The persona a composition contributes. (dsh: `Config`.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Persona {
    /// The persona prefix's text. Empty text drops the section when the
    /// prompt renders, so the deployment persona is gone with nothing in
    /// its place. (dsh: `prefix`, required.)
    pub prefix: String,
    /// The persona suffix's text, after first-party guidance. Empty text,
    /// the default, shadows the deployment suffix away. (dsh: `suffix`.)
    pub suffix: String,
    /// The prefix is the whole system prompt: every other section, the
    /// suffix included, is left out. (dsh: `complete`.)
    pub complete: bool,
    /// Whether this scope's agents see runtime context. `false` suppresses
    /// the optional context for the scope. (dsh: `includeRuntimeContext`,
    /// `true` by default.)
    pub include_runtime_context: bool,
}

impl Persona {
    /// A persona of `prefix`, with dsh's defaults: an empty suffix, not
    /// complete, and runtime context kept.
    pub fn new(prefix: impl Into<String>) -> Self {
        Self {
            prefix: prefix.into(),
            suffix: String::new(),
            complete: false,
            include_runtime_context: true,
        }
    }

    /// Registers the persona's sections in `scope`, and suppresses runtime
    /// context there when asked. (dsh: `apply`.)
    ///
    /// The registrations stay for the prompt's life, as preset plugins'
    /// do.
    ///
    /// # Errors
    ///
    /// The scope has a persona already; and globally, always, since the
    /// system prompt registered the deployment persona there: `prompt
    /// section "deployment:persona-prefix" is already registered`.
    pub fn apply(
        &self,
        prompt: &SystemPrompt,
        scope: Option<&ScopeKey>,
    ) -> Result<(), PromptError> {
        let mut prefix = Section::new(
            PERSONA_PREFIX_SECTION,
            SectionOrder::DeploymentPersonaPrefix.order(),
            self.prefix.as_str(),
        );
        if self.complete {
            prefix = prefix.complete();
        }
        let _ = prompt.section(scope, prefix)?;
        let _ = prompt.section(
            scope,
            Section::new(
                PERSONA_SUFFIX_SECTION,
                SectionOrder::DeploymentPersonaSuffix.order(),
                self.suffix.as_str(),
            ),
        )?;
        if !self.include_runtime_context {
            let _ = prompt.suppress_runtime_context(scope);
        }
        Ok(())
    }
}

impl PresetPlugin for Persona {
    fn name(&self) -> &str {
        "persona"
    }

    fn requires(&self) -> Vec<PortId> {
        vec![PortId::of::<SystemPrompt>()]
    }

    fn setup<'a, 'p>(&'a self, ctx: &'a PresetContext<'a, 'p>) -> PresetFuture<'a> {
        Box::pin(async move {
            self.apply(&ctx.get::<SystemPrompt>(), Some(ctx.scope()))?;
            Ok(())
        })
    }
}
