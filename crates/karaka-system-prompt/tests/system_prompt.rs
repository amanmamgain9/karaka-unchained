//! The registry and its assembly. Ported from dsh's
//! `core/system-prompt/tests/system-prompt.spec.ts`, where its cases apply.
//!
//! Left out: `system-prompt/change` and its rollbacks (Karaka has no event
//! bus), fiber disposal (a `Registration` is removed by hand), the order
//! tables of contributors Karaka doesn't have, and a context smuggled
//! through extra assemble fields (Karaka's context is only the scope).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use karaka_model::ToolSchema;
use karaka_system_prompt::{
    AssembleContext, AssembleFuture, AssembleListener, AssembleNext, AssembledContext,
    AssembledSection, ContextSection, HARNESS_IDENTITY, PromptAssembly, PromptContext, Scope,
    Section, SectionOrder, SystemPrompt, SystemPromptConfig, Text, ToolProviderResult,
    render_context_sections, render_context_snapshot, render_prompt,
};

const BUILT_IN: [&str; 3] = [
    "harness:identity",
    "deployment:persona-prefix",
    "deployment:persona-suffix",
];
const SNAPSHOT: &str =
    "Current runtime context. This snapshot supersedes earlier runtime-context snapshots.";

fn prompt(config: SystemPromptConfig) -> Arc<SystemPrompt> {
    Arc::new(SystemPrompt::new(config).expect("the config is valid"))
}

fn plain() -> Arc<SystemPrompt> {
    prompt(SystemPromptConfig::default())
}

fn persona(prefix: &str) -> SystemPromptConfig {
    SystemPromptConfig {
        persona_prefix: prefix.into(),
        ..SystemPromptConfig::default()
    }
}

fn global() -> AssembleContext {
    AssembleContext::default()
}

fn tool(name: &str, description: &str) -> ToolSchema {
    ToolSchema {
        name: name.into(),
        description: description.into(),
        parameters: "{}".into(),
    }
}

fn names(assembly: &PromptAssembly) -> Vec<&str> {
    assembly
        .sections
        .iter()
        .map(|section| section.name.as_str())
        .collect()
}

/// The sections a test contributed, without the plugin's own.
fn contributed(assembly: &PromptAssembly) -> Vec<&AssembledSection> {
    assembly
        .sections
        .iter()
        .filter(|section| !BUILT_IN.contains(&section.name.as_str()))
        .collect()
}

/// A listener made of a closure that edits the assembly before the rest
/// of the chain runs.
struct Before<F>(F);

impl<F: Fn(&mut PromptAssembly, &AssembleContext) + Send + Sync> AssembleListener for Before<F> {
    fn assemble<'a>(
        &'a self,
        mut assembly: PromptAssembly,
        context: &'a AssembleContext,
        next: AssembleNext<'a>,
    ) -> AssembleFuture<'a> {
        (self.0)(&mut assembly, context);
        next.run(assembly)
    }
}

/// A listener that edits what the rest of the chain gave back.
struct After<F>(F);

impl<F: Fn(&mut PromptAssembly) + Send + Sync> AssembleListener for After<F> {
    fn assemble<'a>(
        &'a self,
        assembly: PromptAssembly,
        _context: &'a AssembleContext,
        next: AssembleNext<'a>,
    ) -> AssembleFuture<'a> {
        Box::pin(async move {
            let mut accepted = next.run(assembly).await?;
            (self.0)(&mut accepted);
            Ok(accepted)
        })
    }
}

#[tokio::test]
async fn optional_context_suppressed_keeps_the_required_context() {
    let prompt = prompt(SystemPromptConfig {
        include_runtime_context: false,
        ..SystemPromptConfig::default()
    });
    let _ = prompt
        .context(
            None,
            PromptContext::new("directory", 0.0, "Directory: /project").required(),
        )
        .unwrap();
    let _ = prompt
        .context(
            None,
            PromptContext::new(
                "optional",
                1.0,
                Text::dynamic(|_| panic!("suppressed provider ran")),
            ),
        )
        .unwrap();
    let _ = prompt.on_assemble(
        None,
        Arc::new(Before(
            |assembly: &mut PromptAssembly, _: &AssembleContext| {
                assembly.contexts.push(AssembledContext::new(
                    "unregistered",
                    "should stay suppressed",
                ));
            },
        )),
    );
    let assembly = prompt.assemble(&global()).await.unwrap();
    assert!(
        render_context_snapshot(&assembly)
            .unwrap()
            .contains("Directory: /project")
    );
    assert_eq!(
        assembly.contexts,
        [AssembledContext::new("directory", "Directory: /project")]
    );
    assert!(
        !render_prompt(&assembly)
            .unwrap()
            .contains("Directory: /project")
    );
}

#[tokio::test]
async fn refreshing_reads_the_context_again_and_nothing_else() {
    let prompt = plain();
    let current = Arc::new(Mutex::new("before".to_owned()));
    let (tools, variables, assemblies) = (
        Arc::new(AtomicUsize::new(0)),
        Arc::new(AtomicUsize::new(0)),
        Arc::new(AtomicUsize::new(0)),
    );
    let live = Arc::clone(&current);
    let _ = prompt
        .context(
            None,
            PromptContext::new(
                "live",
                0.0,
                Text::dynamic(move |_| live.lock().unwrap().clone()),
            )
            .literal(),
        )
        .unwrap();
    let (counted, value) = (Arc::clone(&variables), Arc::clone(&current));
    let _ = prompt
        .variable(None, "fixed", move |_| {
            counted.fetch_add(1, Ordering::SeqCst);
            Some(value.lock().unwrap().clone())
        })
        .unwrap();
    let (counted, value) = (Arc::clone(&tools), Arc::clone(&current));
    let _ = prompt.tools(None, move |_| {
        counted.fetch_add(1, Ordering::SeqCst);
        ToolProviderResult {
            schemas: vec![tool("accepted_tool", &value.lock().unwrap())],
            known_names: None,
        }
    });
    let counted = Arc::clone(&assemblies);
    let _ = prompt.on_assemble(
        None,
        Arc::new(After(move |accepted: &mut PromptAssembly| {
            counted.fetch_add(1, Ordering::SeqCst);
            accepted
                .contexts
                .push(AssembledContext::new("expert", "accepted only"));
        })),
    );

    let accepted = prompt.assemble(&global()).await.unwrap();
    *current.lock().unwrap() = "after".into();
    let refreshed = prompt.refresh_context(&accepted, &global());
    let mut live = AssembledContext::new("live", "after");
    live.interpolate = false;
    assert_eq!(
        refreshed.contexts,
        [live, AssembledContext::new("expert", "accepted only")]
    );
    assert_eq!(accepted.contexts[0].text, "before");
    assert_eq!(refreshed.sections, accepted.sections);
    assert_eq!(refreshed.tools[0].description, "before");
    assert_eq!(refreshed.variables, accepted.variables);
    let counts = [&assemblies, &tools, &variables].map(|count| count.load(Ordering::SeqCst));
    assert_eq!(counts, [1, 1, 1]);
}

#[tokio::test]
async fn refreshing_restores_missing_required_context_in_order() {
    for later in [false, true] {
        let prompt = plain();
        let _ = prompt
            .context(
                None,
                PromptContext::new("required", 0.0, "fresh").required(),
            )
            .unwrap();
        let _ = prompt
            .context(None, PromptContext::new("later", 10.0, "last"))
            .unwrap();
        let mut accepted = prompt.assemble(&global()).await.unwrap();
        accepted.contexts = if later {
            vec![
                AssembledContext::new("expert", "extra"),
                accepted.contexts[1].clone(),
            ]
        } else {
            Vec::new()
        };
        let expected = if later {
            vec![
                AssembledContext::new("expert", "extra"),
                AssembledContext::new("required", "fresh"),
                AssembledContext::new("later", "last"),
            ]
        } else {
            vec![AssembledContext::new("required", "fresh")]
        };
        assert_eq!(
            prompt.refresh_context(&accepted, &global()).contexts,
            expected,
            "later={later}"
        );
    }
}

#[tokio::test]
async fn refreshing_applies_current_suppression_without_reading_optional_context() {
    let prompt = plain();
    let suppressed = Arc::new(Mutex::new(false));
    let _ = prompt
        .context(
            None,
            PromptContext::new("required", 0.0, Text::dynamic(|_| "current".into())).required(),
        )
        .unwrap();
    let flag = Arc::clone(&suppressed);
    let _ = prompt
        .context(
            None,
            PromptContext::new(
                "optional",
                10.0,
                Text::dynamic(move |_| {
                    assert!(!*flag.lock().unwrap(), "suppressed context was evaluated");
                    "optional".into()
                }),
            ),
        )
        .unwrap();
    let mut accepted = prompt.assemble(&global()).await.unwrap();
    accepted
        .contexts
        .push(AssembledContext::new("expert", "accepted optional"));
    let restore = prompt.suppress_runtime_context(None);
    *suppressed.lock().unwrap() = true;
    assert_eq!(
        prompt.refresh_context(&accepted, &global()).contexts,
        [AssembledContext::new("required", "current")]
    );
    restore.remove();
}

#[tokio::test]
async fn literal_context_stays_literal_in_the_snapshot_and_its_sections() {
    let prompt = plain();
    let _ = prompt
        .variable(None, "project", |_| Some("expanded".into()))
        .unwrap();
    let literal = "/workspace/{{project}}/{{unregistered}}/{{malformed-name}}";
    let _ = prompt
        .context(None, PromptContext::new("literal", 0.0, literal).literal())
        .unwrap();
    let _ = prompt
        .context(None, PromptContext::new("template", 1.0, "{{project}}"))
        .unwrap();
    let assembly = prompt.assemble(&global()).await.unwrap();
    assert_eq!(
        render_context_sections(&assembly).unwrap(),
        [
            ContextSection {
                name: "literal".into(),
                text: literal.into()
            },
            ContextSection {
                name: "template".into(),
                text: "expanded".into()
            },
        ]
    );
    assert_eq!(
        render_context_snapshot(&assembly).unwrap(),
        format!("{SNAPSHOT}\n\n{literal}\n\nexpanded")
    );
}

#[test]
fn the_placements_karaka_keeps_are_unique_integral_and_apart() {
    let orders = [
        SectionOrder::HarnessIdentity,
        SectionOrder::DeploymentPersonaPrefix,
        SectionOrder::DeploymentPersonaSuffix,
    ]
    .map(SectionOrder::order);
    assert!(orders.iter().all(|order| order.fract() == 0.0));
    assert!(orders.windows(2).all(|pair| pair[1] - pair[0] >= 10.0));
}

// --- built-in sections ---

#[tokio::test]
async fn the_environment_comes_after_guidance_and_interpolates_strictly() {
    let prompt = prompt(SystemPromptConfig {
        persona_prefix: "Model {{model}}.".into(),
        persona_suffix: "Workspace {{cwd}}.".into(),
        ..SystemPromptConfig::default()
    });
    let _ = prompt
        .variable(None, "model", |_| Some("m".into()))
        .unwrap();
    let _ = prompt
        .section(None, Section::new("guidance", 100.0, "Use tools."))
        .unwrap();
    let unresolved = prompt.assemble(&global()).await.unwrap();
    assert!(
        render_prompt(&unresolved)
            .unwrap_err()
            .to_string()
            .starts_with(
                "unknown prompt variable \"{{cwd}}\" in section \"deployment:persona-suffix\""
            )
    );
    let _ = prompt
        .variable(None, "cwd", |_| Some("/work".into()))
        .unwrap();
    assert_eq!(
        render_prompt(&prompt.assemble(&global()).await.unwrap()).unwrap(),
        format!("{HARNESS_IDENTITY}\n\nModel m.\n\nUse tools.\n\nWorkspace /work.")
    );
}

#[tokio::test]
async fn the_identity_and_the_persona_are_the_plugins_own() {
    let prompt = prompt(persona("You are the deployment."));
    let assembly = prompt.assemble(&global()).await.unwrap();
    assert_eq!(names(&assembly), BUILT_IN);
    assert_eq!(
        render_prompt(&assembly).unwrap(),
        format!("{HARNESS_IDENTITY}\n\nYou are the deployment.")
    );
    assert_eq!(HARNESS_IDENTITY, "You are an AI agent powered by Karaka.");
    let refusal = prompt
        .section(
            None,
            Section::new("deployment:persona-prefix", 0.0, "imposter"),
        )
        .unwrap_err();
    assert!(
        refusal
            .to_string()
            .starts_with("prompt section \"deployment:persona-prefix\" is already registered")
    );
}

#[tokio::test]
async fn a_persona_less_deployment_renders_only_the_identity() {
    assert_eq!(
        render_prompt(&plain().assemble(&global()).await.unwrap()).unwrap(),
        HARNESS_IDENTITY
    );
}

#[tokio::test]
async fn the_identity_can_be_left_out() {
    let prompt = prompt(SystemPromptConfig {
        include_harness_identity: false,
        ..persona("You are a helpful software engineer assistant.")
    });
    let assembly = prompt.assemble(&global()).await.unwrap();
    assert_eq!(names(&assembly), BUILT_IN[1..]);
    assert_eq!(
        render_prompt(&assembly).unwrap(),
        "You are a helpful software engineer assistant."
    );
}

#[tokio::test]
async fn suppressed_context_is_never_read_and_listener_additions_are_dropped() {
    let prompt = prompt(SystemPromptConfig {
        include_runtime_context: false,
        ..SystemPromptConfig::default()
    });
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&calls);
    let _ = prompt
        .context(
            None,
            PromptContext::new(
                "policy",
                0.0,
                Text::dynamic(move |_| {
                    format!("policy {}", counted.fetch_add(1, Ordering::SeqCst))
                }),
            ),
        )
        .unwrap();
    let _ = prompt.on_assemble(
        None,
        Arc::new(Before(
            |assembly: &mut PromptAssembly, _: &AssembleContext| {
                assembly
                    .contexts
                    .push(AssembledContext::new("late", "late context"));
            },
        )),
    );
    let assembly = prompt.assemble(&global()).await.unwrap();
    assert!(assembly.contexts.is_empty());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

// --- assembly ---

#[tokio::test]
async fn sections_contexts_and_tools_are_assembled_in_order() {
    let prompt = prompt(persona("You are the deployment."));
    let _ = prompt
        .section(
            None,
            Section::new("cwd", 20.0, Text::dynamic(|_| "cwd: /tmp".into())),
        )
        .unwrap();
    let _ = prompt
        .section(None, Section::new("rules", 10.0, "Be precise."))
        .unwrap();
    let _ = prompt
        .context(
            None,
            PromptContext::new("later", 20.0, Text::dynamic(|_| "context 2".into())),
        )
        .unwrap();
    let _ = prompt
        .context(None, PromptContext::new("earlier", 10.0, "context 1"))
        .unwrap();
    let _ = prompt.tools(None, |_| ToolProviderResult {
        schemas: vec![tool("echo", "echo back")],
        known_names: None,
    });
    let assembly = prompt.assemble(&global()).await.unwrap();
    assert_eq!(
        names(&assembly),
        [
            "harness:identity",
            "deployment:persona-prefix",
            "rules",
            "cwd",
            "deployment:persona-suffix"
        ]
    );
    let texts: Vec<&str> = assembly
        .sections
        .iter()
        .map(|section| section.text.as_str())
        .collect();
    assert_eq!(
        texts,
        [
            HARNESS_IDENTITY,
            "You are the deployment.",
            "Be precise.",
            "cwd: /tmp",
            ""
        ]
    );
    assert_eq!(
        assembly.contexts,
        [
            AssembledContext::new("earlier", "context 1"),
            AssembledContext::new("later", "context 2")
        ]
    );
    assert_eq!(assembly.tools, [tool("echo", "echo back")]);
    assert!(assembly.variables.is_empty());
    assert_eq!(
        render_prompt(&assembly).unwrap(),
        format!("{HARNESS_IDENTITY}\n\nYou are the deployment.\n\nBe precise.\n\ncwd: /tmp")
    );
    assert_eq!(
        render_context_snapshot(&assembly).unwrap(),
        format!("{SNAPSHOT}\n\ncontext 1\n\ncontext 2")
    );
}

#[tokio::test]
async fn equal_orders_go_by_code_unit_name_whatever_the_registration_order() {
    for names in [["äther", "zeta"], ["zeta", "äther"]] {
        let prompt = plain();
        for name in names {
            let _ = prompt
                .section(None, Section::new(name, 10.0, name))
                .unwrap();
        }
        let assembly = prompt.assemble(&global()).await.unwrap();
        let order: Vec<&str> = contributed(&assembly)
            .iter()
            .map(|section| section.name.as_str())
            .collect();
        assert_eq!(order, ["zeta", "äther"]);
    }
}

#[tokio::test]
async fn section_text_is_worked_out_for_each_assembly_and_its_context() {
    let prompt = plain();
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&calls);
    let _ = prompt
        .section(
            None,
            Section::new(
                "dynamic",
                0.0,
                Text::dynamic(move |context| {
                    let call = counted.fetch_add(1, Ordering::SeqCst) + 1;
                    let who = context.scope.as_ref().map_or("nobody", Scope::as_str);
                    format!("call {call} for {who}")
                }),
            ),
        )
        .unwrap();
    let alice = prompt
        .assemble(&AssembleContext::scoped(Scope::new("alice")))
        .await
        .unwrap();
    assert_eq!(contributed(&alice)[0].text, "call 1 for alice");
    let nobody = prompt.assemble(&global()).await.unwrap();
    assert_eq!(contributed(&nobody)[0].text, "call 2 for nobody");
}

#[tokio::test]
async fn a_removed_contribution_is_gone_and_the_built_ins_stay() {
    let prompt = plain();
    let registrations = [
        prompt
            .section(None, Section::new("scoped", 0.0, "scoped section"))
            .unwrap(),
        prompt
            .context(
                None,
                PromptContext::new("scoped-context", 0.0, "scoped context"),
            )
            .unwrap(),
        prompt.tools(None, |_| ToolProviderResult {
            schemas: vec![tool("scoped-tool", "")],
            known_names: None,
        }),
        prompt
            .variable(None, "scoped_var", |_| Some("v".into()))
            .unwrap(),
    ];
    let before = prompt.assemble(&global()).await.unwrap();
    assert_eq!(contributed(&before).len(), 1);
    assert_eq!(before.contexts.len(), 1);
    assert_eq!(before.variables.get("scoped_var"), Some(Some("v")));
    for registration in registrations {
        registration.remove();
    }
    let after = prompt.assemble(&global()).await.unwrap();
    assert!(contributed(&after).is_empty());
    assert!(after.contexts.is_empty());
    assert_eq!(names(&after), BUILT_IN);
    assert!(after.tools.is_empty());
    assert!(after.variables.is_empty());
}

#[tokio::test]
async fn bad_registrations_are_refused_and_leave_nothing() {
    let prompt = plain();
    let _ = prompt
        .section(None, Section::new("dup", 0.0, "first"))
        .unwrap();
    let refusal = prompt
        .section(None, Section::new("dup", 1.0, "second"))
        .unwrap_err();
    assert!(
        refusal
            .to_string()
            .starts_with("prompt section \"dup\" is already registered")
    );
    assert!(
        prompt
            .section(None, Section::new("bad-order", f64::NAN, "x"))
            .unwrap_err()
            .to_string()
            .contains("order must be a finite number")
    );
    let _ = prompt
        .context(None, PromptContext::new("policy", 1.0, "first"))
        .unwrap();
    assert!(
        prompt
            .context(None, PromptContext::new("policy", 2.0, "second"))
            .unwrap_err()
            .to_string()
            .starts_with("prompt context \"policy\" is already registered")
    );
    assert!(
        prompt
            .context(None, PromptContext::new("bad", f64::INFINITY, "x"))
            .unwrap_err()
            .to_string()
            .contains("order must be a finite number")
    );
    let _ = prompt
        .variable(None, "model", |_| Some("m1".into()))
        .unwrap();
    assert!(
        prompt
            .variable(None, "model", |_| Some("m2".into()))
            .unwrap_err()
            .to_string()
            .starts_with("prompt variable \"model\" is already registered")
    );
    assert!(
        prompt
            .variable(None, "Not Valid", |_| Some("x".into()))
            .unwrap_err()
            .to_string()
            .starts_with("invalid prompt variable name \"Not Valid\"")
    );
    let assembly = prompt.assemble(&global()).await.unwrap();
    let texts: Vec<&str> = contributed(&assembly)
        .iter()
        .map(|section| section.text.as_str())
        .collect();
    assert_eq!(texts, ["first"]);
    assert_eq!(
        assembly.contexts,
        [AssembledContext::new("policy", "first")]
    );
    assert_eq!(assembly.variables.names().collect::<Vec<_>>(), ["model"]);
}

#[tokio::test]
async fn tool_sources_are_taken_before_any_is_asked() {
    let prompt = plain();
    let added = Arc::new(AtomicUsize::new(0));
    let (registry, flag) = (Arc::clone(&prompt), Arc::clone(&added));
    let _ = prompt.tools(None, move |_| {
        if flag.fetch_add(1, Ordering::SeqCst) == 0 {
            let _ = registry.tools(None, |_| ToolProviderResult {
                schemas: vec![tool("late", "")],
                known_names: None,
            });
        }
        ToolProviderResult {
            schemas: vec![tool("first", "")],
            known_names: None,
        }
    });
    let tools = |assembly: PromptAssembly| {
        assembly
            .tools
            .into_iter()
            .map(|tool| tool.name)
            .collect::<Vec<_>>()
    };
    assert_eq!(tools(prompt.assemble(&global()).await.unwrap()), ["first"]);
    assert_eq!(
        tools(prompt.assemble(&global()).await.unwrap()),
        ["first", "late"]
    );
}

#[tokio::test]
async fn listeners_compose_in_order_and_see_the_context() {
    let prompt = plain();
    let _ = prompt
        .section(None, Section::new("base", 10.0, "base"))
        .unwrap();
    let scopes = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&scopes);
    let _ = prompt.on_assemble(
        None,
        Arc::new(Before(
            move |assembly: &mut PromptAssembly, context: &AssembleContext| {
                recorded.lock().unwrap().push(context.clone());
                assembly.sections.push(AssembledSection::new("from-a", "a"));
            },
        )),
    );
    let looked = Arc::clone(&seen);
    let _ = prompt.on_assemble(
        None,
        Arc::new(Before(
            move |assembly: &mut PromptAssembly, _: &AssembleContext| {
                looked.lock().unwrap().push(
                    assembly
                        .sections
                        .iter()
                        .map(|section| section.name.clone())
                        .collect::<Vec<_>>(),
                );
            },
        )),
    );
    let assembly = prompt.assemble(&global()).await.unwrap();
    let expected = [
        "harness:identity",
        "deployment:persona-prefix",
        "base",
        "deployment:persona-suffix",
        "from-a",
    ];
    assert_eq!(
        seen.lock().unwrap().clone(),
        [expected.map(str::to_owned).to_vec()]
    );
    assert_eq!(names(&assembly), expected);
    assert_eq!(scopes.lock().unwrap().clone(), [global()]);
}

/// Answers without running the rest of the chain.
struct ShortCircuit;

impl AssembleListener for ShortCircuit {
    fn assemble<'a>(
        &'a self,
        _assembly: PromptAssembly,
        _context: &'a AssembleContext,
        _next: AssembleNext<'a>,
    ) -> AssembleFuture<'a> {
        Box::pin(async { Ok(PromptAssembly::default()) })
    }
}

#[tokio::test]
async fn a_listener_can_answer_without_the_rest_of_the_chain() {
    let prompt = plain();
    let _ = prompt
        .section(None, Section::new("real", 0.0, "real"))
        .unwrap();
    let _ = prompt.on_assemble(None, Arc::new(ShortCircuit));
    assert!(
        prompt
            .assemble(&global())
            .await
            .unwrap()
            .sections
            .is_empty()
    );
}

#[tokio::test]
async fn a_complete_section_is_restored_after_the_listeners() {
    let prompt = plain();
    let _ = prompt
        .section(
            None,
            Section::new("complete", 10.0, "Exact prompt.").complete(),
        )
        .unwrap();
    let _ = prompt
        .section(None, Section::new("extra", 20.0, "extra"))
        .unwrap();
    let _ = prompt.on_assemble(
        None,
        Arc::new(Before(
            |assembly: &mut PromptAssembly, _: &AssembleContext| {
                let complete = assembly
                    .sections
                    .iter_mut()
                    .find(|section| section.name == "complete")
                    .expect("complete section missing before the listeners");
                complete.text = "mutated".into();
                assembly
                    .sections
                    .push(AssembledSection::new("late", "late"));
            },
        )),
    );
    assert_eq!(
        prompt.assemble(&global()).await.unwrap().sections,
        [AssembledSection::new("complete", "Exact prompt.")]
    );
}

#[tokio::test]
async fn two_complete_sections_fail_the_assembly() {
    let prompt = plain();
    let _ = prompt
        .section(None, Section::new("first", 10.0, "first").complete())
        .unwrap();
    let _ = prompt
        .section(None, Section::new("second", 20.0, "second").complete())
        .unwrap();
    assert_eq!(
        prompt.assemble(&global()).await.unwrap_err().to_string(),
        "multiple complete prompt sections are active: \"first\", \"second\""
    );
}

#[tokio::test]
async fn a_changed_assembly_doesnt_leak_into_the_next() {
    let prompt = plain();
    let _ = prompt
        .section(None, Section::new("base", 10.0, "base"))
        .unwrap();
    let _ = prompt.tools(None, |_| ToolProviderResult {
        schemas: vec![tool("t", "tool")],
        known_names: None,
    });
    let mut first = prompt.assemble(&global()).await.unwrap();
    first.sections[0].name = "mutated".into();
    first
        .contexts
        .push(AssembledContext::new("mutated", "mutated"));
    first.tools[0].description = "mutated".into();
    let second = prompt.assemble(&global()).await.unwrap();
    assert_eq!(
        names(&second),
        [
            "harness:identity",
            "deployment:persona-prefix",
            "base",
            "deployment:persona-suffix"
        ]
    );
    assert_eq!(second.sections[0].text, HARNESS_IDENTITY);
    assert!(second.contexts.is_empty());
    assert_eq!(second.tools, [tool("t", "tool")]);
}

#[tokio::test]
async fn empty_context_renders_nothing_and_context_interpolates() {
    let prompt = plain();
    let _ = prompt
        .context(None, PromptContext::new("empty", 0.0, ""))
        .unwrap();
    assert_eq!(
        render_context_snapshot(&prompt.assemble(&global()).await.unwrap()).unwrap(),
        ""
    );
    let _ = prompt
        .variable(None, "mode", |_| Some("read-only".into()))
        .unwrap();
    let _ = prompt
        .context(None, PromptContext::new("policy", 1.0, "Mode: {{mode}}."))
        .unwrap();
    assert_eq!(
        render_context_snapshot(&prompt.assemble(&global()).await.unwrap()).unwrap(),
        format!("{SNAPSHOT}\n\nMode: read-only.")
    );
}

// --- prompt variables ---

#[tokio::test]
async fn a_variable_is_worked_out_for_each_assembly() {
    let prompt = plain();
    let registration = prompt
        .variable(None, "who", |context| {
            context
                .scope
                .as_ref()
                .map(|scope| scope.as_str().to_owned())
        })
        .unwrap();
    let alice = prompt
        .assemble(&AssembleContext::scoped(Scope::new("alice")))
        .await
        .unwrap();
    assert_eq!(alice.variables.get("who"), Some(Some("alice")));
    // A provider giving nothing is registered with no value here.
    assert_eq!(
        prompt
            .assemble(&global())
            .await
            .unwrap()
            .variables
            .get("who"),
        Some(None)
    );
    registration.remove();
    assert!(
        prompt
            .assemble(&global())
            .await
            .unwrap()
            .variables
            .is_empty()
    );
}

#[tokio::test]
async fn a_variable_added_by_an_earlier_provider_is_read_in_the_same_assembly() {
    let prompt = plain();
    let added = Arc::new(AtomicUsize::new(0));
    let (registry, flag) = (Arc::clone(&prompt), Arc::clone(&added));
    let _ = prompt
        .variable(None, "first", move |_| {
            if flag.fetch_add(1, Ordering::SeqCst) == 0 {
                let _ = registry.variable(None, "late", |_| Some("second value".into()));
            }
            Some("first value".into())
        })
        .unwrap();
    let assembly = prompt.assemble(&global()).await.unwrap();
    assert_eq!(assembly.variables.get("first"), Some(Some("first value")));
    assert_eq!(assembly.variables.get("late"), Some(Some("second value")));
}

#[tokio::test]
async fn references_interpolate_at_render_the_persona_included() {
    let prompt = prompt(persona("You run on {{model}} in {{cwd}}."));
    let _ = prompt
        .variable(None, "model", |_| Some("deepseek-v4".into()))
        .unwrap();
    let _ = prompt
        .variable(None, "cwd", |_| Some("/work".into()))
        .unwrap();
    assert_eq!(
        render_prompt(&prompt.assemble(&global()).await.unwrap()).unwrap(),
        format!("{HARNESS_IDENTITY}\n\nYou run on deepseek-v4 in /work.")
    );
}

#[tokio::test]
async fn a_literal_section_stays_literal_complete_or_not_static_or_not() {
    for (complete, dynamic) in [(false, false), (false, true), (true, false), (true, true)] {
        let prompt = prompt(SystemPromptConfig {
            include_harness_identity: false,
            ..persona("{{model}}")
        });
        let _ = prompt
            .variable(None, "model", |_| Some("actual-model".into()))
            .unwrap();
        let text = "{{item}} {{model}} {{ model }} {{nested{{item}}}}";
        let body: Text = if dynamic {
            Text::dynamic(move |_| text.into())
        } else {
            text.into()
        };
        let mut section = Section::new("literal", 1.0, body).literal();
        section.complete = complete;
        let _ = prompt.section(None, section).unwrap();
        let rendered = render_prompt(&prompt.assemble(&global()).await.unwrap()).unwrap();
        let expected = if complete {
            text.to_owned()
        } else {
            format!("actual-model\n\n{text}")
        };
        assert_eq!(rendered, expected, "complete={complete} dynamic={dynamic}");
    }
}

#[tokio::test]
async fn a_listener_can_add_or_override_variables() {
    let prompt = plain();
    let _ = prompt
        .section(None, Section::new("s", 0.0, "{{extra}}"))
        .unwrap();
    let _ = prompt.on_assemble(
        None,
        Arc::new(Before(
            |assembly: &mut PromptAssembly, _: &AssembleContext| {
                assembly
                    .variables
                    .set("extra", Some("from-waterfall".into()));
            },
        )),
    );
    assert_eq!(
        render_prompt(&prompt.assemble(&global()).await.unwrap()).unwrap(),
        format!("{HARNESS_IDENTITY}\n\nfrom-waterfall")
    );
}
