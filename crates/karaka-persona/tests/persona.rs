//! The persona row. Ported from dsh's `preset/persona/tests/persona.spec.ts`,
//! with a session scope standing in for dsh's minted agent scope.
//!
//! Not ported: "restores the shadowed default when its fiber unloads", and
//! the disposal half of the first case and of the runtime-context case,
//! since the kernel unloads nothing; "keeps runtime context by default when
//! apply bypasses schema defaults", since a Rust `Persona` has no schema to
//! bypass: `Persona::new` sets the default.

use std::sync::Arc;

use karaka_persona::{PERSONA_PREFIX_SECTION, PERSONA_SUFFIX_SECTION, Persona};
use karaka_scope::{ScopeChain, ScopeKey};
use karaka_session::SessionId;
use karaka_system_prompt::{
    AssembleContext, AssembleFuture, AssembleListener, AssembleNext, AssembledSection,
    PromptAssembly, PromptContext, Section, SystemPrompt, SystemPromptConfig, render_prompt,
};

fn harness(prefix: &str, suffix: &str) -> SystemPrompt {
    SystemPrompt::new(SystemPromptConfig {
        persona_prefix: prefix.into(),
        persona_suffix: suffix.into(),
        ..SystemPromptConfig::default()
    })
    .unwrap()
}

fn key(id: &str) -> ScopeKey {
    ScopeKey::Session(SessionId::new(id))
}

fn seen_by(id: &str) -> AssembleContext {
    AssembleContext::scoped(ScopeChain::session(SessionId::new(id)))
}

async fn section(prompt: &SystemPrompt, context: &AssembleContext, name: &str) -> Option<String> {
    prompt
        .assemble(context)
        .await
        .unwrap()
        .sections
        .into_iter()
        .find(|section| section.name == name)
        .map(|section| section.text)
}

async fn rendered(prompt: &SystemPrompt, context: &AssembleContext) -> String {
    render_prompt(&prompt.assemble(context).await.unwrap()).unwrap()
}

// "shadows and interpolates the environment per scope", without its
// disposal half.
#[tokio::test]
async fn it_shadows_and_interpolates_per_scope() {
    let prompt = harness("Deployment.", "Global workspace.");
    let _ = prompt
        .variable(None, "cwd", |_| Some("/local".into()))
        .unwrap();
    let _ = prompt
        .section(None, Section::new("guidance", 100.0, "Use tools."))
        .unwrap();
    let mut persona = Persona::new("Preset.");
    persona.suffix = "Workspace {{cwd}}.".into();
    persona.apply(&prompt, Some(&key("environment"))).unwrap();

    let context = seen_by("environment");
    assert_eq!(
        section(&prompt, &context, PERSONA_SUFFIX_SECTION)
            .await
            .as_deref(),
        Some("Workspace {{cwd}}.")
    );
    assert_eq!(
        rendered(&prompt, &context).await,
        "You are an AI agent powered by Karaka.\n\nPreset.\n\nUse tools.\n\nWorkspace /local."
    );
    assert!(
        rendered(&prompt, &AssembleContext::default())
            .await
            .contains("Global workspace.")
    );
}

// "shadows the default environment with an omitted or empty value"
#[tokio::test]
async fn an_omitted_or_empty_suffix_shadows_the_deployments_away() {
    for suffix in [None, Some("")] {
        let prompt = harness("", "Global workspace.");
        let mut persona = Persona::new("Preset.");
        if let Some(suffix) = suffix {
            persona.suffix = suffix.into();
        }
        persona.apply(&prompt, Some(&key("empty"))).unwrap();
        assert!(
            !rendered(&prompt, &seen_by("empty"))
                .await
                .contains("Global workspace.")
        );
        assert_eq!(
            section(&prompt, &seen_by("empty"), PERSONA_SUFFIX_SECTION)
                .await
                .as_deref(),
            Some("")
        );
    }
}

// "rejects an unscoped mount, which would collide with the registry
// default"
#[test]
fn a_global_mount_collides_with_the_deployment_persona() {
    let prompt = harness("deployment identity", "");
    let error = Persona::new("composition identity")
        .apply(&prompt, None)
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("\"deployment:persona-prefix\" is already registered"),
        "{error}"
    );
}

// "shadows the deployment default for one scope only"
#[tokio::test]
async fn it_shadows_the_deployment_persona_for_one_scope_only() {
    let prompt = harness("deployment identity", "");
    Persona::new("preset identity")
        .apply(&prompt, Some(&key("a1")))
        .unwrap();
    assert_eq!(
        section(&prompt, &seen_by("a1"), PERSONA_PREFIX_SECTION)
            .await
            .as_deref(),
        Some("preset identity")
    );
    assert_eq!(
        section(&prompt, &AssembleContext::default(), PERSONA_PREFIX_SECTION)
            .await
            .as_deref(),
        Some("deployment identity")
    );
}

// "gives two scopes independent personas"
#[tokio::test]
async fn two_scopes_get_independent_personas() {
    let prompt = harness("", "");
    Persona::new("first identity")
        .apply(&prompt, Some(&key("a1")))
        .unwrap();
    Persona::new("second identity")
        .apply(&prompt, Some(&key("a2")))
        .unwrap();
    for (id, text) in [("a1", "first identity"), ("a2", "second identity")] {
        assert_eq!(
            section(&prompt, &seen_by(id), PERSONA_PREFIX_SECTION)
                .await
                .as_deref(),
            Some(text)
        );
    }
}

// "shadows the deployment persona away entirely when its text is empty"
#[tokio::test]
async fn an_empty_persona_shadows_the_deployments_away() {
    let prompt = harness("deployment identity", "");
    Persona::new("").apply(&prompt, Some(&key("a1"))).unwrap();
    assert_eq!(
        section(&prompt, &seen_by("a1"), PERSONA_PREFIX_SECTION)
            .await
            .as_deref(),
        Some("")
    );
    assert!(
        !rendered(&prompt, &seen_by("a1"))
            .await
            .contains("deployment identity")
    );
    assert_eq!(
        section(&prompt, &AssembleContext::default(), PERSONA_PREFIX_SECTION)
            .await
            .as_deref(),
        Some("deployment identity")
    );
}

// "interpolates prompt variables strictly, like any other section"
#[tokio::test]
async fn it_interpolates_strictly() {
    let prompt = harness("", "");
    let _ = prompt
        .variable(None, "model", |_| Some("deepseek-v4-pro".into()))
        .unwrap();
    Persona::new("You run on {{model}}.")
        .apply(&prompt, Some(&key("a1")))
        .unwrap();
    assert_eq!(
        section(&prompt, &seen_by("a1"), PERSONA_PREFIX_SECTION)
            .await
            .as_deref(),
        Some("You run on {{model}}.")
    );
    assert!(
        rendered(&prompt, &seen_by("a1"))
            .await
            .contains("You run on deepseek-v4-pro.")
    );
    Persona::new("{{unknown}}")
        .apply(&prompt, Some(&key("a2")))
        .unwrap();
    let assembly = prompt.assemble(&seen_by("a2")).await.unwrap();
    assert!(render_prompt(&assembly).is_err());
}

/// Adds a section before the rest of the chain runs.
struct Late;

impl AssembleListener for Late {
    fn assemble<'a>(
        &'a self,
        mut assembly: PromptAssembly,
        _: &'a AssembleContext,
        next: AssembleNext<'a>,
    ) -> AssembleFuture<'a> {
        assembly.sections.push(AssembledSection {
            name: "late:extra".into(),
            text: "late guidance".into(),
            interpolate: true,
        });
        next.run(assembly)
    }
}

// "makes a complete persona the exact prompt after every other
// contribution"
#[tokio::test]
async fn a_complete_persona_is_the_exact_prompt() {
    let prompt = harness("deployment identity", "");
    let _ = prompt
        .section(None, Section::new("global:extra", 100.0, "global guidance"))
        .unwrap();
    let mut persona = Persona::new("Only this.");
    persona.suffix = "{{unknown}}".into();
    persona.complete = true;
    persona.apply(&prompt, Some(&key("a1"))).unwrap();
    let _ = prompt.on_assemble_first(Some(&key("a1")), Arc::new(Late));

    let assembly = prompt.assemble(&seen_by("a1")).await.unwrap();
    let names: Vec<&str> = assembly.sections.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, [PERSONA_PREFIX_SECTION]);
    assert_eq!(assembly.sections[0].text, "Only this.");
    assert_eq!(render_prompt(&assembly).unwrap(), "Only this.");
}

// "can suppress runtime context for its scope without changing the global
// assembly", without its disposal half.
#[tokio::test]
async fn it_suppresses_runtime_context_for_its_scope_only() {
    let prompt = harness("deployment identity", "");
    let _ = prompt
        .context(None, PromptContext::new("policy", 1.0, "global policy"))
        .unwrap();
    let mut persona = Persona::new("Only this.");
    persona.include_runtime_context = false;
    persona.apply(&prompt, Some(&key("a1"))).unwrap();

    assert!(
        prompt
            .assemble(&seen_by("a1"))
            .await
            .unwrap()
            .contexts
            .is_empty()
    );
    let global = prompt.assemble(&AssembleContext::default()).await.unwrap();
    let names: Vec<&str> = global.contexts.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["policy"]);
    // By default the scope keeps it.
    Persona::new("Kept.")
        .apply(&prompt, Some(&key("a2")))
        .unwrap();
    assert_eq!(
        prompt
            .assemble(&seen_by("a2"))
            .await
            .unwrap()
            .contexts
            .len(),
        1
    );
}
