//! Contributions scoped to one agent. Ported from dsh's
//! `core/system-prompt/tests/scoped.spec.ts`.
//!
//! dsh scopes nest to any depth, and a scope disposes all its contributions
//! at once. Karaka's chain is fixed at global, preset, then session, and
//! each contribution is removed by its own registration.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use karaka_model::ToolSchema;
use karaka_scope::{PresetId, ScopeChain, ScopeKey};
use karaka_session::SessionId;
use karaka_system_prompt::{
    AssembleContext, AssembleFuture, AssembleListener, AssembleNext, AssembledContext,
    AssembledSection, PromptAssembly, PromptContext, Registration, Section, SystemPrompt,
    SystemPromptConfig, TOOL_ORDER_REST, Text, ToolProviderResult, render_context_snapshot,
    render_prompt,
};

fn mount(config: SystemPromptConfig) -> Arc<SystemPrompt> {
    Arc::new(SystemPrompt::new(config).unwrap())
}

fn persona(prefix: &str) -> SystemPromptConfig {
    SystemPromptConfig {
        persona_prefix: prefix.into(),
        ..SystemPromptConfig::default()
    }
}

fn child() -> ScopeKey {
    ScopeKey::Session(SessionId::new("child"))
}

fn scoped() -> AssembleContext {
    AssembleContext::scoped(ScopeChain::session(SessionId::new("child")))
}

fn coach() -> ScopeKey {
    ScopeKey::Preset(PresetId::new("coach"))
}

/// The child agent, in the preset `coach`.
fn coached() -> AssembleContext {
    AssembleContext::scoped(ScopeChain::new(
        Some(PresetId::new("coach")),
        Some(SessionId::new("child")),
    ))
}

fn global() -> AssembleContext {
    AssembleContext::default()
}

fn schema(name: &str) -> ToolSchema {
    ToolSchema {
        name: name.into(),
        description: format!("tool {name}"),
        parameters: "{}".into(),
    }
}

async fn rendered(prompt: &SystemPrompt, context: &AssembleContext) -> String {
    render_prompt(&prompt.assemble(context).await.unwrap()).unwrap()
}

async fn snapshot(prompt: &SystemPrompt, context: &AssembleContext) -> String {
    render_context_snapshot(&prompt.assemble(context).await.unwrap()).unwrap()
}

async fn tools(prompt: &SystemPrompt, context: &AssembleContext) -> Vec<String> {
    prompt
        .assemble(context)
        .await
        .unwrap()
        .tools
        .into_iter()
        .map(|tool| tool.name)
        .collect()
}

// --- sections ---

#[tokio::test]
async fn a_scoped_persona_shadows_the_deployments_for_that_scope_only() {
    let prompt = mount(persona("You are the deployment."));
    let _ = prompt
        .section(
            Some(&child()),
            Section::new("deployment:persona-prefix", 0.0, "You run tests."),
        )
        .unwrap();
    let scoped = rendered(&prompt, &scoped()).await;
    let global = rendered(&prompt, &global()).await;
    assert!(scoped.contains("You run tests.") && !scoped.contains("You are the deployment."));
    assert!(global.contains("You are the deployment.") && !global.contains("You run tests."));
}

#[tokio::test]
async fn a_scoped_section_joins_that_scope_alone_until_removed() {
    let prompt = mount(SystemPromptConfig::default());
    let registration = prompt
        .section(
            Some(&child()),
            Section::new("child:extra", 50.0, "Extra guidance."),
        )
        .unwrap();
    assert!(
        rendered(&prompt, &scoped())
            .await
            .contains("Extra guidance.")
    );
    assert!(
        !rendered(&prompt, &global())
            .await
            .contains("Extra guidance.")
    );
    registration.remove();
    assert!(
        !rendered(&prompt, &scoped())
            .await
            .contains("Extra guidance.")
    );
}

#[tokio::test]
async fn duplicates_are_refused_per_layer() {
    let prompt = mount(SystemPromptConfig::default());
    let _ = prompt.section(None, Section::new("x", 1.0, "a")).unwrap();
    let global = prompt
        .section(None, Section::new("x", 1.0, "b"))
        .unwrap_err();
    assert!(
        global.to_string().contains("that agent's scope instead"),
        "{global}"
    );
    // A scoped twin of a global name is a shadow, not a duplicate.
    let _ = prompt
        .section(Some(&child()), Section::new("x", 1.0, "c"))
        .unwrap();
    let _ = prompt
        .section(Some(&child()), Section::new("y", 1.0, "a"))
        .unwrap();
    let scoped = prompt
        .section(Some(&child()), Section::new("y", 1.0, "b"))
        .unwrap_err();
    assert!(
        scoped
            .to_string()
            .contains("already registered in this scope"),
        "{scoped}"
    );
}

#[tokio::test]
async fn the_shadowed_global_text_is_never_worked_out() {
    let prompt = mount(SystemPromptConfig::default());
    let (global_calls, scoped_calls) =
        (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let counted = Arc::clone(&global_calls);
    let _ = prompt
        .section(
            None,
            Section::new(
                "shared",
                1.0,
                Text::dynamic(move |_| {
                    counted.fetch_add(1, Ordering::SeqCst);
                    "global text".into()
                }),
            ),
        )
        .unwrap();
    let counted = Arc::clone(&scoped_calls);
    let _ = prompt
        .section(
            Some(&child()),
            Section::new(
                "shared",
                1.0,
                Text::dynamic(move |_| {
                    counted.fetch_add(1, Ordering::SeqCst);
                    "scoped text".into()
                }),
            ),
        )
        .unwrap();
    let assembly = prompt.assemble(&scoped()).await.unwrap();
    let shared = assembly
        .sections
        .iter()
        .find(|section| section.name == "shared")
        .unwrap();
    assert_eq!(shared.text, "scoped text");
    assert_eq!(global_calls.load(Ordering::SeqCst), 0);
    assert_eq!(scoped_calls.load(Ordering::SeqCst), 1);
}

// --- variables ---

#[tokio::test]
async fn a_scoped_variable_shadows_its_global_twin() {
    let prompt = mount(persona("Mode: {{mode}}."));
    let _ = prompt
        .variable(None, "mode", |_| Some("normal".into()))
        .unwrap();
    let _ = prompt
        .variable(Some(&child()), "mode", |_| Some("strict".into()))
        .unwrap();
    assert!(rendered(&prompt, &scoped()).await.contains("Mode: strict."));
    assert!(rendered(&prompt, &global()).await.contains("Mode: normal."));
}

#[tokio::test]
async fn a_scoped_duplicate_is_refused_and_a_removed_one_frees_the_name() {
    let prompt = mount(SystemPromptConfig::default());
    let first = prompt
        .variable(Some(&child()), "v", |_| Some("1".into()))
        .unwrap();
    let refusal = prompt
        .variable(Some(&child()), "v", |_| Some("2".into()))
        .unwrap_err();
    assert!(
        refusal
            .to_string()
            .contains("already registered in this scope")
    );
    first.remove();
    let _ = prompt
        .variable(Some(&child()), "v", |_| Some("3".into()))
        .unwrap();
}

#[tokio::test]
async fn a_variable_replaced_during_an_assembly_waits_for_the_next() {
    let prompt = mount(persona("Mode: {{mode}}."));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let _ = prompt
        .section(
            Some(&child()),
            Section::new("scope:sibling", 1.0, "Scoped."),
        )
        .unwrap();
    let own: Arc<Mutex<Option<Registration>>> = Arc::default();
    let (registry, recorded, handle) = (Arc::clone(&prompt), Arc::clone(&calls), Arc::clone(&own));
    let registration = prompt
        .variable(Some(&child()), "mode", move |_| {
            recorded.lock().unwrap().push("first");
            if let Some(registration) = handle.lock().unwrap().take() {
                registration.remove();
            }
            let recorded = Arc::clone(&recorded);
            let _ = registry.variable(Some(&child()), "mode", move |_| {
                recorded.lock().unwrap().push("replacement");
                Some("replacement".into())
            });
            Some("first".into())
        })
        .unwrap();
    *own.lock().unwrap() = Some(registration);
    assert!(rendered(&prompt, &scoped()).await.contains("Mode: first."));
    assert_eq!(*calls.lock().unwrap(), ["first"]);
    assert!(
        rendered(&prompt, &scoped())
            .await
            .contains("Mode: replacement.")
    );
    assert_eq!(*calls.lock().unwrap(), ["first", "replacement"]);
}

// --- context ---

#[tokio::test]
async fn a_scoped_context_shadows_the_global_one_until_removed() {
    let prompt = mount(SystemPromptConfig::default());
    let _ = prompt
        .context(None, PromptContext::new("policy", 1.0, "global policy"))
        .unwrap();
    let registration = prompt
        .context(
            Some(&child()),
            PromptContext::new("policy", 1.0, "scoped policy"),
        )
        .unwrap();
    let refusal = prompt
        .context(
            Some(&child()),
            PromptContext::new("policy", 2.0, "duplicate"),
        )
        .unwrap_err();
    assert_eq!(
        refusal.to_string(),
        "prompt context \"policy\" is already registered in this scope"
    );
    assert!(snapshot(&prompt, &scoped()).await.contains("scoped policy"));
    assert!(snapshot(&prompt, &global()).await.contains("global policy"));
    registration.remove();
    assert!(snapshot(&prompt, &scoped()).await.contains("global policy"));
}

#[tokio::test]
async fn one_scope_can_suppress_its_context_until_removed() {
    let prompt = mount(SystemPromptConfig::default());
    let _ = prompt
        .context(None, PromptContext::new("policy", 1.0, "global policy"))
        .unwrap();
    let suppressor = prompt.suppress_runtime_context(Some(&child()));
    assert!(
        prompt
            .assemble(&scoped())
            .await
            .unwrap()
            .contexts
            .is_empty()
    );
    assert!(snapshot(&prompt, &global()).await.contains("global policy"));
    suppressor.remove();
    assert!(snapshot(&prompt, &scoped()).await.contains("global policy"));
}

// --- tools ---

#[tokio::test]
async fn a_scoped_tool_source_is_asked_for_its_scope_only() {
    let prompt = mount(SystemPromptConfig::default());
    let _ = prompt.tools(None, |_| ToolProviderResult {
        schemas: vec![schema("global_tool")],
        known_names: None,
    });
    let registration = prompt.tools(Some(&child()), |_| ToolProviderResult {
        schemas: vec![schema("scoped_tool")],
        known_names: None,
    });
    assert_eq!(
        tools(&prompt, &scoped()).await,
        ["global_tool", "scoped_tool"]
    );
    assert_eq!(tools(&prompt, &global()).await, ["global_tool"]);
    registration.remove();
    assert_eq!(tools(&prompt, &scoped()).await, ["global_tool"]);
}

#[tokio::test]
async fn an_ordered_tool_hidden_from_a_scope_is_absent_but_a_typo_fails() {
    let with_order = |order: &str| {
        let prompt = mount(SystemPromptConfig {
            tool_order: Some(vec![order.into(), TOOL_ORDER_REST.into()]),
            ..SystemPromptConfig::default()
        });
        // A source that knows bash but hides it in this assembly.
        let _ = prompt.tools(None, |_| ToolProviderResult {
            schemas: vec![schema("read")],
            known_names: Some(vec!["read".into(), "bash".into()]),
        });
        prompt
    };
    assert_eq!(tools(&with_order("bash"), &global()).await, ["read"]);
    assert_eq!(
        with_order("basj")
            .assemble(&global())
            .await
            .unwrap_err()
            .to_string(),
        "toolOrder lists unregistered tool \"basj\"; known tools: bash, read"
    );
}

// --- refresh and listeners ---

#[tokio::test]
async fn refreshing_uses_the_scopes_sources_and_suppression() {
    let prompt = mount(SystemPromptConfig::default());
    let current = Arc::new(Mutex::new("before".to_owned()));
    let _ = prompt
        .context(None, PromptContext::new("fact", 0.0, "global"))
        .unwrap();
    let live = Arc::clone(&current);
    let _ = prompt
        .context(
            Some(&child()),
            PromptContext::new(
                "fact",
                0.0,
                Text::dynamic(move |_| live.lock().unwrap().clone()),
            )
            .required(),
        )
        .unwrap();
    let _ = prompt
        .context(
            Some(&child()),
            PromptContext::new("optional", 10.0, "optional"),
        )
        .unwrap();
    let accepted = prompt.assemble(&scoped()).await.unwrap();
    *current.lock().unwrap() = "after".into();
    let suppressor = prompt.suppress_runtime_context(Some(&child()));
    assert_eq!(
        prompt.refresh_context(&accepted, &scoped()).contexts,
        [AssembledContext::new("fact", "after")]
    );
    suppressor.remove();
    let global_assembly = prompt.assemble(&global()).await.unwrap();
    assert_eq!(
        prompt.refresh_context(&global_assembly, &global()).contexts,
        [AssembledContext::new("fact", "global")]
    );
}

/// Adds a section after the rest of the chain, and counts its calls.
struct Extra(AtomicUsize);

impl AssembleListener for Extra {
    fn assemble<'a>(
        &'a self,
        assembly: PromptAssembly,
        _context: &'a AssembleContext,
        next: AssembleNext<'a>,
    ) -> AssembleFuture<'a> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            let mut result = next.run(assembly).await?;
            result
                .sections
                .push(AssembledSection::new("listener:extra", "listener text"));
            Ok(result)
        })
    }
}

#[tokio::test]
async fn a_scoped_listener_shapes_its_scopes_assemblies_only() {
    let prompt = mount(SystemPromptConfig::default());
    let listener = Arc::new(Extra(AtomicUsize::new(0)));
    let _ = prompt.on_assemble(
        Some(&child()),
        Arc::clone(&listener) as Arc<dyn AssembleListener>,
    );
    let has = |assembly: &PromptAssembly| {
        assembly
            .sections
            .iter()
            .any(|section| section.name == "listener:extra")
    };
    assert!(has(&prompt.assemble(&scoped()).await.unwrap()));
    assert!(!has(&prompt.assemble(&global()).await.unwrap()));
    assert_eq!(listener.0.load(Ordering::SeqCst), 1);
}

// A preset's contributions reach every agent whose chain names the preset,
// under the session's own, and no other agent. (dsh: a section a preset's
// scope registers, seen through an agent scope bound under it.)
#[tokio::test]
async fn a_presets_contributions_are_seen_through_chains_naming_it() {
    let prompt = mount(SystemPromptConfig::default());
    let _global = prompt
        .section(None, Section::new("tone", 1.0, "Global tone."))
        .unwrap();
    let _preset = prompt
        .section(Some(&coach()), Section::new("tone", 1.0, "Coach tone."))
        .unwrap();
    let _rules = prompt
        .section(Some(&coach()), Section::new("rules", 2.0, "Coach rules."))
        .unwrap();
    let _variable = prompt
        .variable(Some(&coach()), "mode", |_| Some("coach".into()))
        .unwrap();
    let _session = prompt
        .variable(Some(&child()), "mode", |_| Some("child".into()))
        .unwrap();
    let _uses = prompt
        .section(None, Section::new("uses", 3.0, "Mode: {{mode}}."))
        .unwrap();

    let seen = rendered(&prompt, &coached()).await;
    assert!(seen.contains("Coach tone."), "{seen}");
    assert!(!seen.contains("Global tone."), "{seen}");
    assert!(seen.contains("Coach rules."), "{seen}");
    // The session's variable is nearer than the preset's.
    assert!(seen.contains("Mode: child."), "{seen}");

    let preset_only = AssembleContext::scoped(ScopeChain::new(Some(PresetId::new("coach")), None));
    assert!(
        rendered(&prompt, &preset_only)
            .await
            .contains("Mode: coach.")
    );

    let _fallback = prompt
        .variable(None, "mode", |_| Some("global".into()))
        .unwrap();
    for other in [
        scoped(),
        global(),
        AssembleContext::scoped(ScopeChain::new(Some(PresetId::new("other")), None)),
    ] {
        let seen = rendered(&prompt, &other).await;
        assert!(!seen.contains("Coach"), "{seen}");
    }
    // A session id that happens to equal the preset's name isn't the preset.
    let lookalike = AssembleContext::scoped(ScopeChain::session(SessionId::new("coach")));
    assert!(!rendered(&prompt, &lookalike).await.contains("Coach"));
}
