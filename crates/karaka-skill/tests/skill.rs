//! The skill registry. Ported from dsh's `skill/tests/skill.spec.ts`, where
//! its cases fit.
//!
//! Not ported:
//! - the type checks of candidates, definitions, invocation policies and
//!   provider observations ("validates parsed candidate fields", "rejects
//!   malformed provider results and every malformed candidate scalar",
//!   most of "rejects every malformed scalar in provider-loaded
//!   definitions"): Rust's types rule them out. The checks a typed value
//!   can still fail are ported;
//! - a factory that throws, and a Cordis effect that fails, in "registers
//!   providers ...": a factory here can't fail, and there are no effects;
//! - "contains a provider rejection whose string coercion throws", and the
//!   hostile abort reason: a Rust error always renders, and a Karaka
//!   signal has no reason;
//! - a rejecting `skills/change` listener: a listener here is synchronous;
//! - "resolves the scope chain so an agent key inherits its preset layer
//!   and recompose follows the new parent": Karaka has no presets, so a
//!   scope has no parent;
//! - the `localeCompare` and `sort` spies: Rust's string order is by bytes.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use karaka_fs::{AbortController, AbortSignal};
use karaka_kernel::Runtime;
use karaka_scope::{PresetId, ScopeChain, ScopeKey};
use karaka_session::{SessionId, UserSource};
use karaka_skill::{
    BUNDLED_SKILL_RANK, GetFuture, Invocable, ListFuture, SkillCandidate, SkillConfig,
    SkillDefinition, SkillError, SkillInvocationPolicy, SkillLookupOptions, SkillProvider,
    SkillProviderControl, SkillProviderObservation, SkillRegistration, SkillRegistry,
    SkillResourceBase, SkillsPlugin, escape_text, is_skill_name, render_skill_content,
    skill_invocation_source,
};
use serde_json::json;
use tokio::sync::Notify;

fn registry() -> SkillRegistry {
    SkillRegistry::new(SkillConfig::default()).unwrap()
}

fn lookup() -> SkillLookupOptions {
    SkillLookupOptions::default()
}

fn in_scope(id: &str) -> SkillLookupOptions {
    SkillLookupOptions {
        scope: ScopeChain::session(SessionId::new(id)),
        ..SkillLookupOptions::default()
    }
}

/// A candidate whose body is its locator. (dsh: `memorySkill`.)
fn candidate(provider: &str, name: &str, description: &str, rank: f64) -> SkillCandidate {
    SkillCandidate {
        path: None,
        name: name.into(),
        description: description.into(),
        when_to_use: None,
        invocation: SkillInvocationPolicy::BOTH,
        source: "memory".into(),
        provider: provider.into(),
        resource_base: None,
        rank,
        locator: Arc::new(format!("{name} body.")),
        metadata: None,
    }
}

fn memory(name: &str, description: &str, rank: f64) -> SkillCandidate {
    candidate("memory", name, description, rank)
}

/// The definition a candidate loads to.
fn loaded(candidate: &SkillCandidate) -> SkillDefinition {
    SkillDefinition {
        path: candidate.path.clone(),
        name: candidate.name.clone(),
        description: candidate.description.clone(),
        when_to_use: candidate.when_to_use.clone(),
        invocation: candidate.invocation,
        source: candidate.source.clone(),
        provider: candidate.provider.clone(),
        resource_base: candidate.resource_base.clone(),
        content: candidate
            .locator
            .downcast_ref::<String>()
            .cloned()
            .unwrap_or_default(),
        metadata: candidate.metadata.clone(),
    }
}

/// Lists what it holds, and loads each body from its locator. (dsh:
/// `MemoryProvider`.)
struct Memory {
    name: String,
    candidates: Mutex<Vec<SkillCandidate>>,
    complete: bool,
    lists: AtomicUsize,
    gets: AtomicUsize,
}

impl Memory {
    fn named(name: &str, candidates: Vec<SkillCandidate>) -> Arc<Self> {
        Arc::new(Self {
            name: name.into(),
            candidates: Mutex::new(candidates),
            complete: true,
            lists: AtomicUsize::new(0),
            gets: AtomicUsize::new(0),
        })
    }

    fn new(candidates: Vec<SkillCandidate>) -> Arc<Self> {
        Self::named("memory", candidates)
    }

    fn replace(&self, candidates: Vec<SkillCandidate>) {
        *self.candidates.lock().unwrap() = candidates;
    }

    fn lists(&self) -> usize {
        self.lists.load(Ordering::SeqCst)
    }
}

impl SkillProvider for Memory {
    fn name(&self) -> &str {
        &self.name
    }

    fn list<'a>(&'a self, _: &'a SkillLookupOptions) -> ListFuture<'a> {
        self.lists.fetch_add(1, Ordering::SeqCst);
        let candidates = self.candidates.lock().unwrap().clone();
        let complete = self.complete;
        Box::pin(async move {
            Ok(SkillProviderObservation {
                candidates,
                complete,
            })
        })
    }

    fn get<'a>(
        &'a self,
        candidate: &'a SkillCandidate,
        _: &'a SkillLookupOptions,
    ) -> GetFuture<'a> {
        self.gets.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move { Ok(Some(loaded(candidate))) })
    }
}

fn add(registry: &SkillRegistry, provider: Arc<dyn SkillProvider>) {
    let _ = registry
        .register_provider(None, move |_| provider)
        .expect("the provider registers");
}

/// A provider made of two closures.
struct With<L, G> {
    name: String,
    list: L,
    get: G,
}

impl<L, G> SkillProvider for With<L, G>
where
    L: for<'a> Fn(&'a SkillLookupOptions) -> ListFuture<'a> + Send + Sync,
    G: for<'a> Fn(&'a SkillCandidate, &'a SkillLookupOptions) -> GetFuture<'a> + Send + Sync,
{
    fn name(&self) -> &str {
        &self.name
    }

    fn list<'a>(&'a self, options: &'a SkillLookupOptions) -> ListFuture<'a> {
        (self.list)(options)
    }

    fn get<'a>(
        &'a self,
        candidate: &'a SkillCandidate,
        options: &'a SkillLookupOptions,
    ) -> GetFuture<'a> {
        (self.get)(candidate, options)
    }
}

fn with<L, G>(name: &str, list: L, get: G) -> Arc<dyn SkillProvider>
where
    L: for<'a> Fn(&'a SkillLookupOptions) -> ListFuture<'a> + Send + Sync + 'static,
    G: for<'a> Fn(&'a SkillCandidate, &'a SkillLookupOptions) -> GetFuture<'a>
        + Send
        + Sync
        + 'static,
{
    Arc::new(With {
        name: name.into(),
        list,
        get,
    })
}

/// A provider listing `candidates`, whose bodies load from their locators.
fn listing(name: &str, candidates: Vec<SkillCandidate>) -> Arc<dyn SkillProvider> {
    with(
        name,
        move |_| {
            let candidates = candidates.clone();
            Box::pin(async move { Ok(SkillProviderObservation::complete(candidates)) })
        },
        |candidate, _| {
            let definition = loaded(candidate);
            Box::pin(async move { Ok(Some(definition)) })
        },
    )
}

async fn names(registry: &SkillRegistry, options: &SkillLookupOptions) -> Vec<String> {
    registry
        .list(options)
        .await
        .unwrap()
        .into_iter()
        .map(|skill| skill.name)
        .collect()
}

async fn content(
    registry: &SkillRegistry,
    name: &str,
    options: &SkillLookupOptions,
) -> Option<String> {
    registry
        .get(name, options)
        .await
        .unwrap()
        .map(|skill| skill.content)
}

#[derive(Debug)]
struct Plain(&'static str);

impl std::fmt::Display for Plain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for Plain {}

// SkillRegistry registry

// "registers providers, resolves duplicates first-wins, and disposes
// providers", and "sorts model-visible summaries without locale-sensitive
// collation"
#[tokio::test]
async fn providers_merge_by_rank_and_registration_and_leave_when_removed() {
    let registry = registry();
    let memory = Memory::new(vec![
        memory("z-skill", "Z skill", 20.0),
        memory("a-skill", "A skill", 10.0),
        memory("shadowed", "Lower priority", 20.0),
    ]);
    let removable = registry
        .register_provider(None, {
            let memory = Arc::clone(&memory);
            move |_| memory
        })
        .unwrap();
    let mut shadowing = candidate("override", "shadowed", "Higher priority", 5.0);
    shadowing.locator = Arc::new("Override body.".to_owned());
    add(&registry, listing("override", vec![shadowing]));

    let listed: Vec<(String, String, String)> = registry
        .list(&lookup())
        .await
        .unwrap()
        .into_iter()
        .map(|skill| (skill.name, skill.description, skill.provider))
        .collect();
    assert_eq!(
        listed,
        [
            ("a-skill".into(), "A skill".into(), "memory".into()),
            (
                "shadowed".into(),
                "Higher priority".into(),
                "override".into()
            ),
            ("z-skill".into(), "Z skill".into(), "memory".into()),
        ]
    );
    assert_eq!(
        content(&registry, "shadowed", &lookup()).await.as_deref(),
        Some("Override body.")
    );
    add(
        &registry,
        listing(
            "same-rank",
            vec![candidate("same-rank", "same-rank-skill", "Same rank", 10.0)],
        ),
    );
    let same_rank = registry.list(&lookup()).await.unwrap();
    assert_eq!(
        same_rank
            .iter()
            .find(|skill| skill.name == "same-rank-skill")
            .map(|skill| skill.provider.as_str()),
        Some("same-rank")
    );

    let duplicate = registry
        .register_provider(None, |_| Memory::new(Vec::new()) as Arc<dyn SkillProvider>)
        .err()
        .unwrap();
    assert_eq!(
        duplicate.to_string(),
        "a skill provider named \"memory\" is already registered"
    );
    let refused: Arc<OnceLock<AbortSignal>> = Arc::default();
    let kept = Arc::clone(&refused);
    let reserved = registry
        .register_provider(None, move |control| {
            kept.set(control.signal.clone()).unwrap();
            Memory::named("runtime", Vec::new()) as Arc<dyn SkillProvider>
        })
        .err()
        .unwrap();
    assert_eq!(
        reserved.to_string(),
        "\"runtime\" is reserved for runtime skill registrations"
    );
    assert!(refused.get().unwrap().is_aborted());

    removable.remove();
    assert_eq!(
        names(&registry, &lookup()).await,
        ["same-rank-skill", "shadowed"]
    );
}

fn runtime(name: &str, invocation: Option<SkillInvocationPolicy>) -> SkillRegistration {
    SkillRegistration {
        invocation,
        ..SkillRegistration::new(name, name, "runtime", format!("{name} body."))
    }
}

// "returns an invocation-neutral catalog and resolves model and user policy
// independently"
#[tokio::test]
async fn the_catalog_is_invocation_neutral() {
    let registry = registry();
    let policy = |model_invocable, user_invocable| {
        Some(SkillInvocationPolicy {
            model_invocable,
            user_invocable,
        })
    };
    for (name, invocation) in [
        ("both", None),
        ("model-only", policy(true, false)),
        ("user-only", policy(false, true)),
        ("trusted-only", policy(false, false)),
    ] {
        let _ = registry.register(None, runtime(name, invocation)).unwrap();
    }
    let listed = registry.list(&lookup()).await.unwrap();
    let named = |skills: Vec<&karaka_skill::SkillSummary>| -> Vec<String> {
        skills.into_iter().map(|skill| skill.name.clone()).collect()
    };
    assert_eq!(
        named(listed.iter().collect()),
        ["both", "model-only", "trusted-only", "user-only"]
    );
    assert_eq!(listed[0].invocation, SkillInvocationPolicy::BOTH);
    assert_eq!(
        named(
            listed
                .iter()
                .filter(|skill| skill.is_model_invocable())
                .collect()
        ),
        ["both", "model-only"]
    );
    assert_eq!(
        named(
            listed
                .iter()
                .filter(|skill| skill.is_user_invocable())
                .collect()
        ),
        ["both", "user-only"]
    );
    assert_eq!(
        content(&registry, "trusted-only", &lookup())
            .await
            .as_deref(),
        Some("trusted-only body.")
    );
    let both = registry.get("both", &lookup()).await.unwrap().unwrap();
    assert_eq!(both.invocation, SkillInvocationPolicy::BOTH);
}

// "validates provider candidates and invalid registry caps": the checks a
// typed candidate can fail, and a cache of no entries.
#[tokio::test]
async fn a_candidate_that_breaks_the_contract_fails_the_read() {
    let registry = SkillRegistry::new(SkillConfig::default()).unwrap();
    assert_eq!(registry.list(&lookup()).await.unwrap(), []);
    let mut nan = candidate("bad-rank", "bad-rank", "Bad rank", f64::NAN);
    nan.provider = "bad-rank".into();
    for (provider, bad, message) in [
        (
            "bad",
            candidate("bad", "Bad_Name", "bad", 1.0),
            "skill provider \"bad\" returned invalid skill name \"Bad_Name\"",
        ),
        (
            "empty-description",
            candidate("empty-description", "empty-description", "", 1.0),
            "skill provider \"empty-description\" returned skill \"empty-description\" without a description",
        ),
        (
            "bad-rank",
            nan,
            "skill provider \"bad-rank\" returned skill \"bad-rank\" with an invalid rank",
        ),
        (
            "wrong-provider",
            candidate("different", "wrong-provider", "Wrong provider", 1.0),
            "skill provider \"wrong-provider\" returned skill \"wrong-provider\" for provider \"different\"",
        ),
    ] {
        let registry = self::registry();
        add(&registry, listing(provider, vec![bad]));
        let error = registry.list(&lookup()).await.unwrap_err();
        assert_eq!(error, SkillError::Invalid(message.into()));
    }
    let error = SkillRegistry::new(SkillConfig {
        collect_cache_max_entries: 0,
    })
    .unwrap_err();
    assert!(
        error.to_string().contains("collectCacheMaxEntries"),
        "{error}"
    );
}

// "borrows the exact lookup options through discovery and loading"
#[tokio::test]
async fn providers_get_the_lookup_and_the_candidate_they_listed() {
    let registry = registry();
    let options = SkillLookupOptions {
        cwd: Some("/workspace/a".into()),
        ..SkillLookupOptions::default()
    };
    let listed_with = Arc::new(Mutex::new(None::<(usize, Option<String>)>));
    let loaded_with = Arc::new(Mutex::new(None::<(usize, bool)>));
    let original = candidate("contextual", "skill-a", "Skill A", 1.0);
    let locator = Arc::clone(&original.locator);
    let (listing_seen, loading_seen) = (Arc::clone(&listed_with), Arc::clone(&loaded_with));
    add(
        &registry,
        with(
            "contextual",
            move |received| {
                *listing_seen.lock().unwrap() =
                    Some((std::ptr::from_ref(received) as usize, received.cwd.clone()));
                let candidate = original.clone();
                Box::pin(async move { Ok(SkillProviderObservation::complete(vec![candidate])) })
            },
            move |received, options| {
                *loading_seen.lock().unwrap() = Some((
                    std::ptr::from_ref(options) as usize,
                    Arc::ptr_eq(&received.locator, &locator),
                ));
                let definition = SkillDefinition {
                    content: "Skill A body.".into(),
                    ..loaded(received)
                };
                Box::pin(async move { Ok(Some(definition)) })
            },
        ),
    );
    assert_eq!(names(&registry, &options).await, ["skill-a"]);
    assert_eq!(
        content(&registry, "skill-a", &options).await.as_deref(),
        Some("Skill A body.")
    );
    let address = std::ptr::from_ref(&options) as usize;
    assert_eq!(
        *listed_with.lock().unwrap(),
        Some((address, Some("/workspace/a".into())))
    );
    assert_eq!(*loaded_with.lock().unwrap(), Some((address, true)));
}

// "rechecks cancellation after cached discovery before provider loading"
#[tokio::test]
async fn a_cancel_after_a_cached_discovery_stops_the_load() {
    let registry = registry();
    let memory = Memory::new(vec![memory("cached-skill", "Cached skill", 1.0)]);
    add(&registry, Arc::clone(&memory) as Arc<dyn SkillProvider>);
    let cached = SkillLookupOptions {
        cwd: Some("/workspace/cache".into()),
        ..SkillLookupOptions::default()
    };
    registry.list(&cached).await.unwrap();
    let options = SkillLookupOptions {
        signal: Some(AbortSignal::aborted()),
        ..cached
    };
    assert_eq!(
        registry.get("cached-skill", &options).await,
        Err(SkillError::Aborted)
    );
    assert_eq!(memory.gets.load(Ordering::SeqCst), 0);
}

// "stops waiting for cached provider loading when a hostile abort reason
// fires": here, any cancel.
#[tokio::test]
async fn a_cancel_stops_waiting_for_a_held_load() {
    let registry = Arc::new(registry());
    let started = Arc::new(Notify::new());
    let starting = Arc::clone(&started);
    let saw_signal = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&saw_signal);
    add(
        &registry,
        with(
            "held",
            |_| {
                Box::pin(async {
                    Ok(SkillProviderObservation::complete(vec![candidate(
                        "held",
                        "held-skill",
                        "Held skill",
                        1.0,
                    )]))
                })
            },
            move |_, options| {
                if options.signal.is_some() {
                    seen.fetch_add(1, Ordering::SeqCst);
                }
                starting.notify_one();
                Box::pin(std::future::pending())
            },
        ),
    );
    registry.list(&lookup()).await.unwrap();
    let controller = AbortController::new();
    let options = SkillLookupOptions {
        signal: Some(controller.signal()),
        ..SkillLookupOptions::default()
    };
    let pending = tokio::spawn({
        let registry = Arc::clone(&registry);
        async move { registry.get("held-skill", &options).await }
    });
    started.notified().await;
    controller.abort();
    let settled = tokio::time::timeout(Duration::from_secs(5), pending)
        .await
        .expect("the load stops waiting")
        .unwrap();
    assert_eq!(settled, Err(SkillError::Aborted));
    assert_eq!(saw_signal.load(Ordering::SeqCst), 1);
}

// "borrows cached candidates and loaded definitions from the provider"
#[tokio::test]
async fn a_summary_is_the_candidates_and_a_load_is_the_providers() {
    let registry = registry();
    let mut stable = candidate("detached", "stable-skill", "Stable description", 1.0);
    stable.when_to_use = Some("When stability matters.".into());
    stable.resource_base = Some(SkillResourceBase::Opaque {
        description: "candidate resources".into(),
    });
    stable.path = Some("/skills/stable/SKILL.md".into());
    stable.metadata = Some(json!({ "owner": "candidate" }).as_object().unwrap().clone());
    let definition = SkillDefinition {
        resource_base: Some(SkillResourceBase::Opaque {
            description: "definition resources".into(),
        }),
        metadata: Some(
            json!({ "owner": "definition" })
                .as_object()
                .unwrap()
                .clone(),
        ),
        content: "Stable body.".into(),
        ..loaded(&stable)
    };
    let lists = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&lists);
    let listed_candidate = stable.clone();
    let given = definition.clone();
    add(
        &registry,
        with(
            "detached",
            move |_| {
                counted.fetch_add(1, Ordering::SeqCst);
                let candidate = listed_candidate.clone();
                Box::pin(async move { Ok(SkillProviderObservation::complete(vec![candidate])) })
            },
            move |_, _| {
                let definition = given.clone();
                Box::pin(async move { Ok(Some(definition)) })
            },
        ),
    );
    let listed = registry.list(&lookup()).await.unwrap();
    assert_eq!(listed, [stable.summary()]);
    assert_eq!(
        listed[0].resource_base,
        Some(SkillResourceBase::Opaque {
            description: "candidate resources".into()
        })
    );
    assert_eq!(lists.load(Ordering::SeqCst), 1);
    assert_eq!(
        registry.get("stable-skill", &lookup()).await.unwrap(),
        Some(definition)
    );
    assert_eq!(lists.load(Ordering::SeqCst), 1);
}

// "preserves readonly runtime resource identities while adding the default
// provider"
#[tokio::test]
async fn a_runtime_skill_keeps_its_fields_and_gets_the_runtime_provider() {
    let registry = registry();
    let resource_base = SkillResourceBase::Opaque {
        description: "runtime resources".into(),
    };
    let metadata = json!({ "owner": "runtime" }).as_object().unwrap().clone();
    let _ = registry
        .register(
            None,
            SkillRegistration {
                when_to_use: Some("When runtime data is needed.".into()),
                invocation: Some(SkillInvocationPolicy::BOTH),
                resource_base: Some(resource_base.clone()),
                metadata: Some(metadata.clone()),
                ..SkillRegistration::new("runtime-skill", "Runtime", "runtime", "Runtime body.")
            },
        )
        .unwrap();
    let _ = registry
        .register(
            None,
            SkillRegistration::new(
                "z-runtime",
                "Second runtime skill",
                "runtime",
                "Second runtime body.",
            ),
        )
        .unwrap();
    let listed = registry.list(&lookup()).await.unwrap();
    assert_eq!(listed[0].resource_base.as_ref(), Some(&resource_base));
    let loaded = registry
        .get("runtime-skill", &lookup())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(loaded.resource_base, Some(resource_base));
    assert_eq!(loaded.metadata, Some(metadata));
    assert_eq!(loaded.provider, "runtime");
    assert_eq!(
        loaded.when_to_use.as_deref(),
        Some("When runtime data is needed.")
    );
}

// "rejects every malformed scalar in provider-loaded definitions": the two
// a typed definition can fail.
#[tokio::test]
async fn a_loaded_skill_that_breaks_the_contract_fails_the_load() {
    for (patch, message) in [
        (
            ("Bad_Name", "Definition"),
            "loaded skill has invalid name \"Bad_Name\"",
        ),
        (
            ("definition", ""),
            "loaded skill \"definition\" requires a description",
        ),
    ] {
        let registry = registry();
        let (name, description) = patch;
        add(
            &registry,
            with(
                "definitions",
                |_| {
                    Box::pin(async {
                        Ok(SkillProviderObservation::complete(vec![candidate(
                            "definitions",
                            "definition",
                            "Candidate",
                            1.0,
                        )]))
                    })
                },
                move |candidate, _| {
                    let definition = SkillDefinition {
                        name: name.into(),
                        description: description.into(),
                        ..loaded(candidate)
                    };
                    Box::pin(async move { Ok(Some(definition)) })
                },
            ),
        );
        assert_eq!(
            registry.get("definition", &lookup()).await,
            Err(SkillError::Invalid(message.into()))
        );
    }
}

// "caches provider discovery, skips failing providers, and invalidates on
// runtime skills"
#[tokio::test]
async fn discovery_is_cached_a_failing_provider_skipped_and_a_change_invalidates() {
    let registry = SkillRegistry::new(SkillConfig {
        collect_cache_max_entries: 1,
    })
    .unwrap();
    let memory = Memory::new(vec![memory("first-skill", "First", 10.0)]);
    add(&registry, Arc::clone(&memory) as Arc<dyn SkillProvider>);
    assert_eq!(names(&registry, &lookup()).await, ["first-skill"]);
    memory.replace(vec![self::memory("second-skill", "Second", 10.0)]);
    assert_eq!(names(&registry, &lookup()).await, ["first-skill"]);

    let handle = registry
        .register(
            None,
            SkillRegistration {
                resource_base: Some(SkillResourceBase::Opaque {
                    description: "runtime memory".into(),
                }),
                path: Some("memory://runtime-skill".into()),
                metadata: Some(json!({ "owner": "tests" }).as_object().unwrap().clone()),
                ..SkillRegistration::new("runtime-skill", "Runtime", "runtime", "Runtime body.")
            },
        )
        .unwrap();
    assert_eq!(
        names(&registry, &lookup()).await,
        ["runtime-skill", "second-skill"]
    );
    let runtime = registry
        .get("runtime-skill", &lookup())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(runtime.content, "Runtime body.");
    assert_eq!(runtime.path.as_deref(), Some("memory://runtime-skill"));
    handle.remove();
    for cwd in ["/tmp/first-cache-key", "/tmp/second-cache-key"] {
        let options = SkillLookupOptions {
            cwd: Some(cwd.into()),
            ..SkillLookupOptions::default()
        };
        registry.list(&options).await.unwrap();
    }

    let fail = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let calls = Arc::new(AtomicUsize::new(0));
    let (failing, counted) = (Arc::clone(&fail), Arc::clone(&calls));
    add(
        &registry,
        with(
            "flaky",
            move |_| {
                counted.fetch_add(1, Ordering::SeqCst);
                let fail = failing.load(Ordering::SeqCst);
                Box::pin(async move {
                    if fail {
                        return Err(Box::new(Plain("transient discovery failure")) as _);
                    }
                    Ok(SkillProviderObservation::complete(vec![candidate(
                        "flaky",
                        "flaky-skill",
                        "Flaky",
                        10.0,
                    )]))
                })
            },
            |_, _| Box::pin(async { Ok(None) }),
        ),
    );
    let incomplete = registry.snapshot(&lookup()).await.unwrap();
    assert_eq!(incomplete.skills[0].name, "second-skill");
    assert_eq!(incomplete.skills.len(), 1);
    assert!(!incomplete.complete);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        registry.warnings().last().map(String::as_str),
        Some("skill provider \"flaky\" skipped: Error: transient discovery failure")
    );
    assert_eq!(names(&registry, &lookup()).await, ["second-skill"]);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    fail.store(false, Ordering::SeqCst);
    assert_eq!(
        names(&registry, &lookup()).await,
        ["flaky-skill", "second-skill"]
    );
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    assert_eq!(
        names(&registry, &lookup()).await,
        ["flaky-skill", "second-skill"]
    );
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

// "keeps candidates from incomplete provider observations loadable without
// caching them"
#[tokio::test]
async fn an_incomplete_observation_is_loadable_but_not_cached() {
    let registry = registry();
    let memory = Arc::new(Memory {
        complete: false,
        ..Arc::into_inner(Memory::named(
            "incomplete-candidates",
            vec![candidate(
                "incomplete-candidates",
                "available-skill",
                "Available",
                10.0,
            )],
        ))
        .unwrap()
    });
    add(&registry, Arc::clone(&memory) as Arc<dyn SkillProvider>);
    let snapshot = registry.snapshot(&lookup()).await.unwrap();
    assert_eq!(snapshot.skills[0].name, "available-skill");
    assert!(!snapshot.complete);
    assert_eq!(
        content(&registry, "available-skill", &lookup())
            .await
            .as_deref(),
        Some("available-skill body.")
    );
    assert_eq!(names(&registry, &lookup()).await, ["available-skill"]);
    assert_eq!(memory.lists(), 3);
}

/// Registers `provider`, keeping its control.
fn add_with_control(
    registry: &SkillRegistry,
    scope: Option<&ScopeKey>,
    provider: Arc<dyn SkillProvider>,
) -> (karaka_skill::ProviderRegistration, SkillProviderControl) {
    let control: Arc<OnceLock<SkillProviderControl>> = Arc::default();
    let kept = Arc::clone(&control);
    let registration = registry
        .register_provider(scope, move |given| {
            kept.set(given).unwrap();
            provider
        })
        .unwrap();
    let control = control.get().unwrap().clone();
    (registration, control)
}

// "invalidates only the exact registered provider and ignores its late
// callbacks"
#[tokio::test]
async fn a_control_invalidates_only_while_its_registration_lives() {
    let registry = registry();
    let memory = Memory::new(vec![memory("first-skill", "First", 10.0)]);
    let (registration, control) = add_with_control(
        &registry,
        None,
        Arc::clone(&memory) as Arc<dyn SkillProvider>,
    );
    assert!(registry.snapshot(&lookup()).await.unwrap().complete);
    memory.replace(vec![self::memory("second-skill", "Second", 10.0)]);
    assert_eq!(names(&registry, &lookup()).await, ["first-skill"]);
    control.invalidate();
    assert_eq!(names(&registry, &lookup()).await, ["second-skill"]);
    registration.remove();
    assert!(control.signal.is_aborted());

    let replacement = Memory::new(vec![self::memory("replacement-skill", "Replacement", 10.0)]);
    add(
        &registry,
        Arc::clone(&replacement) as Arc<dyn SkillProvider>,
    );
    assert_eq!(names(&registry, &lookup()).await, ["replacement-skill"]);
    control.invalidate();
    assert_eq!(names(&registry, &lookup()).await, ["replacement-skill"]);
    assert_eq!(replacement.lists(), 1);
}

// "emits catalog invalidations for live provider and runtime mutations"
#[tokio::test]
async fn each_change_is_told_to_the_listeners() {
    let registry = registry();
    let changes = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&changes);
    let _ = registry.on_change(Arc::new(move || {
        counted.fetch_add(1, Ordering::SeqCst);
    }));
    let count = || changes.load(Ordering::SeqCst);
    let (provider, control) = add_with_control(
        &registry,
        None,
        Memory::new(vec![memory("provider-skill", "Provider", 10.0)]),
    );
    assert_eq!(count(), 1);
    control.invalidate();
    assert_eq!(count(), 2);
    let handle = registry
        .register(
            None,
            SkillRegistration::new("runtime-skill", "Runtime", "runtime", "Runtime body."),
        )
        .unwrap();
    assert_eq!(count(), 3);
    handle.remove();
    assert_eq!(count(), 4);
    provider.remove();
    assert_eq!(count(), 5);
    control.invalidate();
    assert_eq!(count(), 5);
}

// "contains synchronous and asynchronous catalog observer failures": a
// panicking listener is warned of, and the others still hear the change.
#[tokio::test]
async fn a_panicking_listener_cant_stop_a_change() {
    let registry = registry();
    let _ = registry.on_change(Arc::new(|| panic!("observer threw")));
    let observed = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&observed);
    let _ = registry.on_change(Arc::new(move || {
        counted.fetch_add(1, Ordering::SeqCst);
    }));
    add(&registry, Memory::new(Vec::new()));
    assert_eq!(observed.load(Ordering::SeqCst), 1);
    assert_eq!(
        registry.warnings(),
        ["skills/change listener threw: Error: observer threw"]
    );
}

/// A provider whose first list waits for `gate`, after telling `started`.
struct Gated {
    memory: Arc<Memory>,
    started: Arc<Notify>,
    gate: Arc<Notify>,
}

impl SkillProvider for Gated {
    fn name(&self) -> &str {
        "gated"
    }

    fn list<'a>(&'a self, options: &'a SkillLookupOptions) -> ListFuture<'a> {
        let first = self.memory.lists() == 0;
        let stale = self.memory.candidates.lock().unwrap().clone();
        Box::pin(async move {
            if first {
                self.memory.lists.fetch_add(1, Ordering::SeqCst);
                self.started.notify_one();
                self.gate.notified().await;
                return Ok(SkillProviderObservation::complete(stale));
            }
            self.memory.list(options).await
        })
    }

    fn get<'a>(
        &'a self,
        candidate: &'a SkillCandidate,
        options: &'a SkillLookupOptions,
    ) -> GetFuture<'a> {
        self.memory.get(candidate, options)
    }
}

fn gated(candidates: Vec<SkillCandidate>) -> (Arc<Gated>, Arc<Notify>, Arc<Notify>) {
    let started = Arc::new(Notify::new());
    let gate = Arc::new(Notify::new());
    let mut candidates = candidates;
    for candidate in &mut candidates {
        candidate.provider = "gated".into();
    }
    let provider = Arc::new(Gated {
        memory: Memory::named("gated", candidates),
        started: Arc::clone(&started),
        gate: Arc::clone(&gate),
    });
    (provider, started, gate)
}

// "retries an in-flight catalog invalidated by its provider"
#[tokio::test]
async fn a_catalog_invalidated_while_in_flight_is_collected_again() {
    let registry = Arc::new(registry());
    let (provider, started, gate) = gated(vec![memory("stale-skill", "Stale", 10.0)]);
    let (_registration, control) = add_with_control(
        &registry,
        None,
        Arc::clone(&provider) as Arc<dyn SkillProvider>,
    );
    let pending = tokio::spawn({
        let registry = Arc::clone(&registry);
        async move { names(&registry, &lookup()).await }
    });
    started.notified().await;
    provider
        .memory
        .replace(vec![candidate("gated", "fresh-skill", "Fresh", 10.0)]);
    control.invalidate();
    gate.notify_one();
    assert_eq!(pending.await.unwrap(), ["fresh-skill"]);
    assert_eq!(provider.memory.lists(), 2);
}

// "bounds repeated in-flight invalidation and leaves the result uncached"
#[tokio::test]
async fn repeated_invalidation_in_flight_gives_an_uncached_result() {
    let registry = registry();
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&calls);
    let _ = registry
        .register_provider(None, move |control| {
            with(
                "self-invalidating",
                move |_| {
                    let attempt = counted.fetch_add(1, Ordering::SeqCst) + 1;
                    control.invalidate();
                    Box::pin(async move {
                        Ok(SkillProviderObservation::complete(vec![candidate(
                            "self-invalidating",
                            "bounded-skill",
                            &format!("Attempt {attempt}"),
                            10.0,
                        )]))
                    })
                },
                |_, _| Box::pin(async { Ok(None) }),
            )
        })
        .unwrap();
    let first = registry.snapshot(&lookup()).await.unwrap();
    assert!(!first.complete);
    assert_eq!(
        serde_json::to_value(&first.skills).unwrap(),
        json!([{
            "name": "bounded-skill",
            "description": "Attempt 2",
            "invocation": { "modelInvocable": true, "userInvocable": true },
            "source": "memory",
            "provider": "self-invalidating",
        }])
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let second = registry.snapshot(&lookup()).await.unwrap();
    assert_eq!(second.skills[0].description, "Attempt 4");
    assert_eq!(calls.load(Ordering::SeqCst), 4);
}

// "invalidates a provider whose loaded definition changed identity"
#[tokio::test]
async fn a_load_whose_name_changed_is_none_and_invalidates() {
    let registry = registry();
    let lists = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&lists);
    add(
        &registry,
        with(
            "renamed",
            move |_| {
                counted.fetch_add(1, Ordering::SeqCst);
                Box::pin(async {
                    Ok(SkillProviderObservation::complete(vec![candidate(
                        "renamed", "old-name", "Old name", 1.0,
                    )]))
                })
            },
            |candidate, _| {
                let definition = SkillDefinition {
                    name: "new-name".into(),
                    content: "Fresh body.".into(),
                    ..loaded(candidate)
                };
                Box::pin(async move { Ok(Some(definition)) })
            },
        ),
    );
    assert_eq!(registry.get("old-name", &lookup()).await, Ok(None));
    registry.list(&lookup()).await.unwrap();
    assert_eq!(lists.load(Ordering::SeqCst), 2);
}

// "returns undefined when a discovered candidate disappears before loading"
#[tokio::test]
async fn a_body_that_no_longer_loads_is_none() {
    let registry = registry();
    add(
        &registry,
        with(
            "vanished-body",
            |_| {
                Box::pin(async {
                    Ok(SkillProviderObservation::complete(vec![candidate(
                        "vanished-body",
                        "vanished-skill",
                        "Vanished",
                        10.0,
                    )]))
                })
            },
            |_, _| Box::pin(async { Ok(None) }),
        ),
    );
    assert_eq!(registry.get("vanished-skill", &lookup()).await, Ok(None));
}

// "propagates a load failure raced against an armed abort signal"
#[tokio::test]
async fn a_load_failure_is_the_reads_error() {
    let registry = registry();
    add(
        &registry,
        with(
            "failing-loader",
            |_| {
                Box::pin(async {
                    Ok(SkillProviderObservation::complete(vec![candidate(
                        "failing-loader",
                        "failing-skill",
                        "Failing",
                        10.0,
                    )]))
                })
            },
            |_, _| Box::pin(async { Err(Box::new(Plain("load failed")) as _) }),
        ),
    );
    let controller = AbortController::new();
    let options = SkillLookupOptions {
        signal: Some(controller.signal()),
        ..SkillLookupOptions::default()
    };
    assert_eq!(
        registry.get("failing-skill", &options).await,
        Err(SkillError::Load("load failed".into()))
    );
}

// "abandons an in-flight catalog when provider registrations change"
#[tokio::test]
async fn a_registration_removed_in_flight_leaves_the_catalog() {
    let registry = Arc::new(registry());
    let (provider, started, gate) = gated(vec![memory("stale-skill", "Stale", 10.0)]);
    let (registration, _) = add_with_control(
        &registry,
        None,
        Arc::clone(&provider) as Arc<dyn SkillProvider>,
    );
    let pending = tokio::spawn({
        let registry = Arc::clone(&registry);
        async move { names(&registry, &lookup()).await }
    });
    started.notified().await;
    registration.remove();
    gate.notify_one();
    assert_eq!(pending.await.unwrap(), Vec::<String>::new());
}

// "stops waiting for discovery when its lookup signal aborts"
#[tokio::test]
async fn a_cancel_stops_waiting_for_discovery() {
    let registry = Arc::new(registry());
    let started = Arc::new(Notify::new());
    let starting = Arc::clone(&started);
    add(
        &registry,
        with(
            "uncooperative",
            move |_| {
                starting.notify_one();
                Box::pin(std::future::pending())
            },
            |_, _| Box::pin(async { Ok(None) }),
        ),
    );
    let controller = AbortController::new();
    let options = SkillLookupOptions {
        signal: Some(controller.signal()),
        ..SkillLookupOptions::default()
    };
    let pending = tokio::spawn({
        let registry = Arc::clone(&registry);
        async move { registry.list(&options).await }
    });
    started.notified().await;
    controller.abort();
    let settled = tokio::time::timeout(Duration::from_secs(5), pending)
        .await
        .expect("discovery stops waiting")
        .unwrap();
    assert_eq!(settled, Err(SkillError::Aborted));
    assert_eq!(
        SkillError::Aborted.to_string(),
        "This operation was aborted"
    );
}

// "rejects invalid runtime skill registrations and ignores duplicates"
#[tokio::test]
async fn a_bad_runtime_skill_is_refused_and_a_duplicate_ignored() {
    let registry = registry();
    let refuse = |registration: SkillRegistration| {
        registry
            .register(None, registration)
            .err()
            .map(|error| error.to_string())
    };
    assert_eq!(
        refuse(SkillRegistration::new("Bad_Name", "Bad", "runtime", "bad")).as_deref(),
        Some("invalid skill name \"Bad_Name\"")
    );
    assert_eq!(
        refuse(SkillRegistration::new(
            "no-description",
            "",
            "runtime",
            "bad"
        ))
        .as_deref(),
        Some("skill \"no-description\" requires a description")
    );
    assert_eq!(registry.get("missing-skill", &lookup()).await, Ok(None));
    assert_eq!(registry.get("Bad_Name", &lookup()).await, Ok(None));

    let first = registry
        .register(
            None,
            SkillRegistration::new("same-skill", "First", "runtime", "first"),
        )
        .unwrap();
    let second = registry
        .register(
            None,
            SkillRegistration::new("same-skill", "Second", "runtime", "second"),
        )
        .unwrap();
    assert_eq!(
        registry.warnings(),
        ["runtime skill \"same-skill\" ignored because it is already registered"]
    );
    second.remove();
    let kept = registry
        .get("same-skill", &lookup())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(kept.description, "First");
    first.remove();
    assert_eq!(registry.get("same-skill", &lookup()).await, Ok(None));
}

// A duplicate name within a layer is warned of, as dsh logs it.
#[tokio::test]
async fn a_lower_priority_duplicate_is_warned_of() {
    let registry = registry();
    add(
        &registry,
        Memory::new(vec![
            memory("dup", "Second", 20.0),
            memory("dup", "First", 10.0),
        ]),
    );
    assert_eq!(
        registry.list(&lookup()).await.unwrap()[0].description,
        "First"
    );
    assert_eq!(
        registry.warnings(),
        ["skill \"dup\" from memory ignored because a higher-priority skill already exists"]
    );
}

// renderSkillContent

// "renders a directory-based skill with the shared wrapper"
#[test]
fn a_directory_skill_renders_with_the_shared_wrapper() {
    let text = render_skill_content(
        "demo-skill",
        "memory",
        Some(&SkillResourceBase::Directory {
            path: "/tmp/demo".into(),
        }),
        "Do the thing.",
    );
    assert_eq!(
        text,
        [
            "<skill_content name=\"demo-skill\">",
            "<skill_resources>",
            "Base directory for this skill: /tmp/demo",
            "Resolve relative paths mentioned by this skill against the base directory before using them. Load referenced resources only as needed.",
            "</skill_resources>",
            "",
            "<skill_instructions>",
            "Do the thing.",
            "</skill_instructions>",
            "</skill_content>",
        ]
        .join("\n")
    );
}

// "renders url and opaque resource hints"
#[test]
fn url_and_opaque_hints_render() {
    let url = render_skill_content(
        "url-skill",
        "memory",
        Some(&SkillResourceBase::Url {
            url: "https://example.test/base/".into(),
        }),
        "Body.",
    );
    assert!(url.contains("Base URL for this skill: https://example.test/base/"));
    assert!(url.contains(
        "Resolve relative URLs mentioned by this skill against the base URL before using them."
    ));
    let opaque = render_skill_content(
        "opaque-skill",
        "memory",
        Some(&SkillResourceBase::Opaque {
            description: "archive <bundle>".into(),
        }),
        "Body.",
    );
    assert!(opaque.contains("Resources for this skill: archive &lt;bundle&gt;"));
}

// "falls back to the provider hint without a resource base"
#[test]
fn without_a_resource_base_the_provider_is_named() {
    let text = render_skill_content("provider-skill", "remote <hub>", None, "Body.");
    assert!(
        text.contains("Resources for this skill are managed by provider \"remote &lt;hub&gt;\".")
    );
}

// "escapes hostile attribute names and keeps the body verbatim"
#[test]
fn the_name_is_escaped_and_the_body_kept() {
    let text = render_skill_content(
        "x\"&<y",
        "memory",
        Some(&SkillResourceBase::Directory {
            path: "/tmp".into(),
        }),
        "Keep </skill_content> and <tags> as-is.",
    );
    assert!(text.contains("<skill_content name=\"x&quot;&amp;&lt;y\">"));
    assert!(text.contains("Keep </skill_content> and <tags> as-is."));
    assert_eq!(escape_text("a & <b> \""), "a &amp; &lt;b&gt; \"");
}

// The helpers dsh exports.
#[test]
fn names_ranks_and_the_invocation_source_are_dshs() {
    for name in ["a", "a-b", "skill-2", "0"] {
        assert!(is_skill_name(name), "{name}");
    }
    for name in ["", "-a", "a-", "a--b", "A", "a_b", "a b", "é"] {
        assert!(!is_skill_name(name), "{name}");
    }
    assert_eq!(BUNDLED_SKILL_RANK, 600.0);
    assert_eq!(
        skill_invocation_source("demo"),
        UserSource::Other(
            json!({ "kind": "skill-invocation", "name": "demo", "form": "instructions" })
        )
    );
}

// SkillRegistry scoped layers

/// A provider of one skill, named `provider`.
fn one(provider: &str, name: &str, description: &str, rank: f64) -> Arc<dyn SkillProvider> {
    let mut skill = candidate(provider, name, description, rank);
    skill.locator = Arc::new(format!("{description} body."));
    listing(provider, vec![skill])
}

// "files a scoped provider into its layer and merges it into that scope
// view only"
#[tokio::test]
async fn a_scoped_provider_shows_only_in_its_scopes_view() {
    let registry = registry();
    add(
        &registry,
        Memory::new(vec![memory("global-skill", "Global", 100.0)]),
    );
    let scope = ScopeKey::from(SessionId::new("a"));
    let _ = registry
        .register_provider(Some(&scope), |_| {
            one("preset-local", "preset-skill", "Preset", 300.0)
        })
        .unwrap();
    assert_eq!(names(&registry, &lookup()).await, ["global-skill"]);
    assert_eq!(
        names(&registry, &in_scope("a")).await,
        ["global-skill", "preset-skill"]
    );
    assert_eq!(
        content(&registry, "preset-skill", &in_scope("a"))
            .await
            .as_deref(),
        Some("Preset body.")
    );
    assert_eq!(content(&registry, "preset-skill", &lookup()).await, None);
    assert_eq!(
        content(&registry, "preset-skill", &in_scope("b")).await,
        None
    );
}

// "lets the nearest layer win a duplicate name regardless of rank"
#[tokio::test]
async fn the_scopes_entry_wins_whatever_its_rank() {
    let registry = registry();
    add(
        &registry,
        Memory::new(vec![memory("shared-name", "Global wins ranks", 10.0)]),
    );
    let scope = ScopeKey::from(SessionId::new("shadow"));
    let _ = registry
        .register_provider(Some(&scope), |_| {
            one("preset-local", "shared-name", "Preset shadow", 900.0)
        })
        .unwrap();
    let scoped = registry.list(&in_scope("shadow")).await.unwrap();
    assert_eq!(scoped.len(), 1);
    assert_eq!(scoped[0].description, "Preset shadow");
    assert_eq!(
        content(&registry, "shared-name", &in_scope("shadow"))
            .await
            .as_deref(),
        Some("Preset shadow body.")
    );
    assert_eq!(
        registry.list(&lookup()).await.unwrap()[0].description,
        "Global wins ranks"
    );
}

// "scopes provider-name uniqueness per layer and reports scoped duplicates
// distinctly"
#[tokio::test]
async fn provider_names_are_unique_per_layer() {
    let registry = registry();
    add(&registry, Memory::new(Vec::new()));
    let (a, b) = (
        ScopeKey::from(SessionId::new("a")),
        ScopeKey::from(SessionId::new("b")),
    );
    let _ = registry
        .register_provider(Some(&a), |_| {
            Memory::new(vec![memory("a-only", "A", 100.0)]) as Arc<dyn SkillProvider>
        })
        .unwrap();
    let _ = registry
        .register_provider(Some(&b), |_| {
            Memory::new(vec![memory("b-only", "B", 100.0)]) as Arc<dyn SkillProvider>
        })
        .unwrap();
    let duplicate = registry
        .register_provider(Some(&a), |_| {
            Memory::new(Vec::new()) as Arc<dyn SkillProvider>
        })
        .err()
        .unwrap();
    assert_eq!(
        duplicate.to_string(),
        "a skill provider named \"memory\" is already registered in this scope"
    );
    assert_eq!(names(&registry, &in_scope("a")).await, ["a-only"]);
    assert_eq!(names(&registry, &in_scope("b")).await, ["b-only"]);
}

// "keeps runtime duplicate handling per layer and shadows a global runtime
// name"
#[tokio::test]
async fn runtime_duplicates_are_per_layer_and_a_scope_shadows() {
    let registry = registry();
    let _ = registry
        .register(
            None,
            SkillRegistration::new("told-twice", "Global runtime", "runtime", "Global body."),
        )
        .unwrap();
    let scope = ScopeKey::from(SessionId::new("runtime"));
    let shadow = registry
        .register(
            Some(&scope),
            SkillRegistration::new("told-twice", "Preset runtime", "preset", "Preset body."),
        )
        .unwrap();
    assert!(registry.warnings().is_empty());
    let _ = registry
        .register(
            Some(&scope),
            SkillRegistration::new("told-twice", "Ignored", "preset", "Ignored."),
        )
        .unwrap();
    assert_eq!(
        registry.warnings(),
        ["runtime skill \"told-twice\" ignored because it is already registered"]
    );
    assert_eq!(
        content(&registry, "told-twice", &in_scope("runtime"))
            .await
            .as_deref(),
        Some("Preset body.")
    );
    assert_eq!(
        content(&registry, "told-twice", &lookup()).await.as_deref(),
        Some("Global body.")
    );
    shadow.remove();
    assert_eq!(
        content(&registry, "told-twice", &in_scope("runtime"))
            .await
            .as_deref(),
        Some("Global body.")
    );
}

// "drops a disposed scoped registration from its scope view and notifies
// change"
#[tokio::test]
async fn a_removed_scoped_registration_leaves_its_view() {
    let registry = registry();
    let changes = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&changes);
    let _ = registry.on_change(Arc::new(move || {
        counted.fetch_add(1, Ordering::SeqCst);
    }));
    let scope = ScopeKey::from(SessionId::new("hmr"));
    let registration = registry
        .register_provider(Some(&scope), |_| {
            Memory::new(vec![memory("scoped-skill", "Scoped", 100.0)]) as Arc<dyn SkillProvider>
        })
        .unwrap();
    assert_eq!(names(&registry, &in_scope("hmr")).await, ["scoped-skill"]);
    let notified = changes.load(Ordering::SeqCst);
    registration.remove();
    assert!(changes.load(Ordering::SeqCst) > notified);
    assert_eq!(
        names(&registry, &in_scope("hmr")).await,
        Vec::<String>::new()
    );
}

// "invalidates through a scoped provider control only while its exact
// registration is live"
#[tokio::test]
async fn a_scoped_control_invalidates_only_while_live() {
    let registry = registry();
    let scope = ScopeKey::from(SessionId::new("invalidate"));
    let memory = Memory::new(vec![memory("watched", "Watched", 100.0)]);
    let (registration, control) = add_with_control(
        &registry,
        Some(&scope),
        Arc::clone(&memory) as Arc<dyn SkillProvider>,
    );
    let view = in_scope("invalidate");
    assert_eq!(names(&registry, &view).await, ["watched"]);
    memory.replace(vec![self::memory("replaced", "Replaced", 100.0)]);
    control.invalidate();
    assert_eq!(names(&registry, &view).await, ["replaced"]);
    registration.remove();
    memory.replace(vec![self::memory("ignored", "Ignored", 100.0)]);
    control.invalidate();
    assert_eq!(names(&registry, &view).await, Vec::<String>::new());
}

// The plugin provides an empty registry, and refuses a cache of nothing.
#[tokio::test]
async fn the_plugin_provides_the_registry() {
    let runtime = Runtime::start(vec![Box::new(SkillsPlugin::default())])
        .await
        .unwrap();
    let registry = runtime.get::<SkillRegistry>();
    assert_eq!(registry.list(&lookup()).await.unwrap(), []);
    let error = Runtime::start(vec![Box::new(SkillsPlugin::new(SkillConfig {
        collect_cache_max_entries: 0,
    }))])
    .await
    .err()
    .expect("the start fails");
    assert!(
        error.to_string().contains("collectCacheMaxEntries")
            || std::error::Error::source(&error)
                .is_some_and(|cause| cause.to_string().contains("collectCacheMaxEntries")),
        "{error}"
    );
}

// A preset's layer is seen through every chain naming the preset, between
// the global layer and the session's; the nearest layer wins a name
// whatever the ranks. (dsh: a preset's standing mount, with an agent scope
// bound under it.)
#[tokio::test]
async fn a_presets_layer_sits_between_global_and_the_session() {
    let registry = registry();
    add(
        &registry,
        Memory::new(vec![memory("shared-name", "Global", 10.0)]),
    );
    let coach = ScopeKey::from(PresetId::new("coach"));
    let _ = registry
        .register_provider(Some(&coach), |_| {
            one("coach-local", "shared-name", "Coach", 300.0)
        })
        .unwrap();
    let _ = registry
        .register_provider(Some(&coach), |_| {
            one("coach-only", "coach-skill", "Coach only", 300.0)
        })
        .unwrap();
    let coached = |session: &str| SkillLookupOptions {
        scope: ScopeChain::new(Some(PresetId::new("coach")), Some(SessionId::new(session))),
        ..SkillLookupOptions::default()
    };
    assert_eq!(
        names(&registry, &coached("s1")).await,
        ["coach-skill", "shared-name"]
    );
    assert_eq!(
        content(&registry, "shared-name", &coached("s1"))
            .await
            .as_deref(),
        Some("Coach body.")
    );

    // The session's own layer is nearer still.
    let s1 = ScopeKey::from(SessionId::new("s1"));
    let _ = registry
        .register_provider(Some(&s1), |_| one("s1-local", "shared-name", "Mine", 900.0))
        .unwrap();
    assert_eq!(
        content(&registry, "shared-name", &coached("s1"))
            .await
            .as_deref(),
        Some("Mine body.")
    );
    assert_eq!(
        content(&registry, "shared-name", &coached("s2"))
            .await
            .as_deref(),
        Some("Coach body.")
    );

    // No other chain sees the preset: not another preset, not a session
    // alone, and not a session named like the preset.
    let other = SkillLookupOptions {
        scope: ScopeChain::new(Some(PresetId::new("other")), Some(SessionId::new("s3"))),
        ..SkillLookupOptions::default()
    };
    for view in [other, in_scope("s3"), in_scope("coach"), lookup()] {
        assert_eq!(names(&registry, &view).await, ["shared-name"]);
        assert_eq!(
            content(&registry, "shared-name", &view).await.as_deref(),
            Some("shared-name body.")
        );
    }
}
