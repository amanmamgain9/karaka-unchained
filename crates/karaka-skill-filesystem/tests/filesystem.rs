//! The local skill provider, over real temporary directories. Ported from
//! dsh's `skill-filesystem/tests/skill-filesystem.spec.ts` and
//! `skill-filesystem-watcher.spec.ts`, where their cases fit.
//!
//! Most cases read through `karaka-fs-local`. dsh's `TestFileSystem`, which
//! injects failures, is [`Fake`] here, over the local one; its watch is
//! scripted too, so the watcher cases run without inotify. One case
//! watches the real filesystem, and skips, saying so, where the system has
//! no inotify instances left.
//!
//! Not ported:
//! - "declares stable plugin metadata": a Cordis export;
//! - the `withFileSystem: false` half of the linked-skills case, and the
//!   trusted bundled root read past a failing filesystem: every read here
//!   goes through the filesystem seam;
//! - "marks an unexpected native skill-file read failure incomplete":
//!   there is no native path, and through the seam a `SKILL.md` that is a
//!   directory is skipped, as dsh's filesystem path skips it;
//! - the Chokidar options, polling and symlink-following tunables, the
//!   watcher-close failure, and the disposal races of the watcher spec;
//!   and its event filtering, since a Karaka watch carries no path. Every
//!   event re-plans and invalidates;
//! - `fs/observed`'s actor filter: Karaka has no such event, so the caller
//!   of `observe_host_mutation` filters.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use karaka_fs::{
    AbortController, AbortSignal, ByteRange, FileSystem, FsDirEntry, FsEditOutcome, FsEditRequest,
    FsError, FsErrorCode, FsFuture, FsInfo, FsPathInfo, FsTarget, FsTargetKey, FsType, FsVersion,
    FsWatch, FsWriteIntent, FsWriteOutcome, TextStream, WatchListener,
};
use karaka_fs_local::{LocalFileSystem, LocalFsConfig};
use karaka_skill::{
    SkillLookupOptions, SkillProvider, SkillRegistration, SkillRegistry, SkillResourceBase,
    SkillSummary,
};
use karaka_skill_filesystem::{FileSystemSkillProvider, SkillFilesystemConfig};

/// A temporary directory, by its realpath, removed when dropped.
struct TempDir(String);

impl TempDir {
    fn new(name: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "karaka-skill-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(
            std::fs::canonicalize(path)
                .unwrap()
                .to_string_lossy()
                .into_owned(),
        )
    }

    fn at(&self, relative: &str) -> String {
        format!("{}/{relative}", self.0)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn write(path: &str, text: &str) {
    std::fs::create_dir_all(std::path::Path::new(path).parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// A directory bundle, `<root>/<name>/SKILL.md`. (dsh: `writeSkill`.)
fn write_skill(root: &str, name: &str, description: &str, body: &str) {
    write(
        &format!("{root}/{name}/SKILL.md"),
        &format!("---\nname: {name}\ndescription: {description}\n---\n\n{body}\n"),
    );
}

/// A flat skill, `<root>/<name>.md`. (dsh: `writeFlatSkill`.)
fn write_flat_skill(root: &str, name: &str, description: &str, body: &str) {
    write(
        &format!("{root}/{name}.md"),
        &format!("---\nname: {name}\ndescription: {description}\n---\n\n{body}\n"),
    );
}

fn local() -> Arc<dyn FileSystem> {
    Arc::new(LocalFileSystem::new(LocalFsConfig::default()).unwrap())
}

/// A config whose homes are under `home`, never the user's own, and which
/// doesn't watch.
fn config(home: &TempDir) -> SkillFilesystemConfig {
    SkillFilesystemConfig {
        dsh_home: Some(home.at(".dsh")),
        agents_home: Some(home.at(".agents")),
        watch: false,
        ..SkillFilesystemConfig::default()
    }
}

/// A registry with one provider over `fs`, and the provider. The
/// environment it sees is `env`, and nothing else.
fn mount_with_env(
    fs: Arc<dyn FileSystem>,
    config: SkillFilesystemConfig,
    env: HashMap<&'static str, String>,
) -> (SkillRegistry, Arc<FileSystemSkillProvider>) {
    let registry = SkillRegistry::new(karaka_skill::SkillConfig::default()).unwrap();
    let slot = Arc::new(OnceLock::new());
    let kept = Arc::clone(&slot);
    let _ = registry
        .register_provider(None, move |control| {
            let provider = FileSystemSkillProvider::with_env(fs, control, config, move |name| {
                env.get(name).cloned()
            })
            .unwrap();
            kept.set(Arc::clone(&provider)).unwrap();
            provider as Arc<dyn SkillProvider>
        })
        .unwrap();
    let provider = slot.get().unwrap().clone();
    (registry, provider)
}

fn mount(
    fs: Arc<dyn FileSystem>,
    config: SkillFilesystemConfig,
) -> (SkillRegistry, Arc<FileSystemSkillProvider>) {
    mount_with_env(fs, config, HashMap::new())
}

fn at(cwd: &str) -> SkillLookupOptions {
    SkillLookupOptions {
        cwd: Some(cwd.to_owned()),
        ..SkillLookupOptions::default()
    }
}

fn anywhere() -> SkillLookupOptions {
    SkillLookupOptions::default()
}

async fn list(registry: &SkillRegistry, options: &SkillLookupOptions) -> Vec<SkillSummary> {
    registry.list(options).await.unwrap()
}

async fn names(registry: &SkillRegistry, options: &SkillLookupOptions) -> Vec<String> {
    list(registry, options)
        .await
        .into_iter()
        .map(|skill| skill.name)
        .collect()
}

fn find<'a>(skills: &'a [SkillSummary], name: &str) -> &'a SkillSummary {
    skills
        .iter()
        .find(|skill| skill.name == name)
        .unwrap_or_else(|| panic!("no skill {name}"))
}

// FileSystemSkillProvider

// "discovers project, custom, user, and agents skill roots in priority
// order"
#[tokio::test]
async fn the_roots_are_read_in_priority_order() {
    let home = TempDir::new("home");
    let project = TempDir::new("project");
    let custom = TempDir::new("custom");
    let bundled = TempDir::new("bundled");
    std::fs::create_dir_all(project.at(".git")).unwrap();
    write_skill(
        &home.at(".agents/skills"),
        "same",
        "user agents skill",
        "Use the skill.",
    );
    write_skill(
        &home.at(".dsh/skills"),
        "same",
        "user dsh skill",
        "Use the skill.",
    );
    write_skill(&custom.0, "same", "custom skill", "Use the skill.");
    write_skill(
        &project.at(".agents/skills"),
        "same",
        "project agents skill",
        "Use the skill.",
    );
    write_skill(
        &project.at(".dsh/skills"),
        "same",
        "project dsh skill",
        "Use the skill.",
    );
    write_skill(&custom.0, "custom-only", "custom only", "Use the skill.");
    write_skill(
        &home.at(".dsh/skills/.system"),
        "hidden-system",
        "hidden system",
        "Hidden.",
    );
    write_skill(
        &bundled.0,
        "bundled-only",
        "bundled skill",
        "Use the skill.",
    );
    write_skill(&bundled.0, "same", "bundled skill", "Use the skill.");
    let (registry, _provider) = mount(
        local(),
        SkillFilesystemConfig {
            custom_skill_dirs: vec![custom.0.clone()],
            bundled_skill_dir: Some(bundled.0.clone()),
            ..config(&home)
        },
    );

    let skills = list(&registry, &at(&project.at("src"))).await;
    let named: Vec<&str> = skills.iter().map(|skill| skill.name.as_str()).collect();
    assert_eq!(named, ["bundled-only", "custom-only", "same"]);
    assert_eq!(find(&skills, "custom-only").description, "custom only");
    assert_eq!(find(&skills, "same").description, "project dsh skill");
    assert_eq!(find(&skills, "same").source, "project-dsh");
    assert_eq!(find(&skills, "bundled-only").source, "bundled");
    let bundled_body = registry
        .get("bundled-only", &anywhere())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(bundled_body.content, "Use the skill.");

    // With no `.git` above, the lookup's own directory is the project.
    let no_git = TempDir::new("no-git");
    write_skill(
        &no_git.at(".dsh/skills"),
        "fallback-root",
        "Fallback root",
        "Body.",
    );
    assert!(
        names(&registry, &at(&no_git.0))
            .await
            .contains(&"fallback-root".to_owned())
    );
}

// "lets project skills override runtime while runtime overrides custom and
// user skills"
#[tokio::test]
async fn project_beats_runtime_which_beats_custom_and_user() {
    let home = TempDir::new("runtime-home");
    let project = TempDir::new("runtime-project");
    let custom = TempDir::new("runtime-custom");
    std::fs::create_dir_all(project.at(".git")).unwrap();
    write_skill(
        &project.at(".dsh/skills"),
        "project-name",
        "Project wins",
        "Body.",
    );
    write_skill(&custom.0, "runtime-name", "Custom loses", "Body.");
    write_skill(
        &home.at(".dsh/skills"),
        "runtime-name",
        "User loses",
        "Body.",
    );
    let (registry, _provider) = mount(
        local(),
        SkillFilesystemConfig {
            custom_skill_dirs: vec![custom.0.clone()],
            ..config(&home)
        },
    );
    for (name, description) in [
        ("project-name", "Runtime loses to project"),
        ("runtime-name", "Runtime wins"),
    ] {
        let _ = registry
            .register(
                None,
                SkillRegistration::new(name, description, "runtime", "Runtime body."),
            )
            .unwrap();
    }
    let description = |name: &'static str| {
        let registry = &registry;
        let project = &project;
        async move {
            registry
                .get(name, &at(&project.0))
                .await
                .unwrap()
                .unwrap()
                .description
        }
    };
    assert_eq!(description("project-name").await, "Project wins");
    assert_eq!(description("runtime-name").await, "Runtime wins");
}

// "parses flat skills and filters invalid skills from the
// invocation-neutral listing", with dsh's warnings
#[tokio::test]
async fn flat_skills_parse_and_invalid_files_are_skipped_with_a_warning() {
    let home = TempDir::new("flat");
    let root = home.at(".dsh/skills");
    write_flat_skill(
        &root,
        "flat-skill",
        "flat description",
        "Flat instructions.",
    );
    write(
        &format!("{root}/rich-skill.md"),
        &[
            "---",
            "name: rich-skill",
            "description: rich description",
            "whenToUse: For richer local parsing",
            "disable-model-invocation: off",
            "user-invocable: YES",
            "metadata:",
            "  owner: tests",
            "---",
            "",
            "Rich body.",
        ]
        .join("\n"),
    );
    write(
        &format!("{root}/bad.md"),
        "---\nname: Bad_Name\ndescription: bad\n---\n\nbad",
    );
    write(
        &format!("{root}/missing-description.md"),
        "---\nname: missing-description\n---\n\nbad",
    );
    write(&format!("{root}/no-frontmatter.md"), "No frontmatter.");
    write(
        &format!("{root}/plain-markdown.md"),
        "# Notes\nNot a skill.",
    );
    write(
        &format!("{root}/open-frontmatter.md"),
        "---\nname: open-frontmatter",
    );
    write(&format!("{root}/non-object.md"), "---\n[]\n---\n\nbad");
    write(
        &format!("{root}/no-trailing-body.md"),
        "---\nname: no-trailing-body\ndescription: No trailing body\n---",
    );
    write(&format!("{root}/notes.txt"), "ignored");
    std::fs::create_dir_all(format!("{root}/not-a-skill")).unwrap();
    write(
        &format!("{root}/user-only-skill/SKILL.md"),
        "---\nname: user-only-skill\ndescription: user-only description\ndisable-model-invocation: true\n---\n\nUser-only.\n",
    );
    write(
        &format!("{root}/model-only-skill/SKILL.md"),
        "---\nname: model-only-skill\ndescription: model-only description\nuser-invocable: false\n---\n\nModel-only.\n",
    );
    let (registry, _provider) = mount(local(), config(&home));

    let listed = list(&registry, &anywhere()).await;
    std::fs::remove_file(format!("{root}/flat-skill.md")).unwrap();
    let named: Vec<&str> = listed.iter().map(|skill| skill.name.as_str()).collect();
    assert_eq!(
        named,
        [
            "flat-skill",
            "model-only-skill",
            "no-trailing-body",
            "rich-skill",
            "user-only-skill"
        ]
    );
    let flat = find(&listed, "flat-skill");
    assert!(flat.invocation.model_invocable && flat.invocation.user_invocable);
    assert_eq!(
        flat.resource_base,
        Some(SkillResourceBase::Directory { path: root.clone() })
    );
    assert_eq!(registry.get("flat-skill", &anywhere()).await, Ok(None));
    let get = |name: &'static str| {
        let registry = &registry;
        async move { registry.get(name, &anywhere()).await.unwrap().unwrap() }
    };
    let no_body = get("no-trailing-body").await;
    assert_eq!(no_body.content, "");
    let user_only = get("user-only-skill").await;
    assert!(!user_only.invocation.model_invocable && user_only.invocation.user_invocable);
    assert_eq!(user_only.content, "User-only.");
    assert_eq!(
        user_only.resource_base,
        Some(SkillResourceBase::Directory {
            path: format!("{root}/user-only-skill")
        })
    );
    let model_only = get("model-only-skill").await;
    assert!(model_only.invocation.model_invocable && !model_only.invocation.user_invocable);
    let rich = get("rich-skill").await;
    assert_eq!(
        rich.when_to_use.as_deref(),
        Some("For richer local parsing")
    );
    assert!(rich.invocation.model_invocable && rich.invocation.user_invocable);
    assert_eq!(
        serde_json::Value::Object(rich.metadata.unwrap()),
        serde_json::json!({ "owner": "tests" })
    );
    assert_eq!(registry.get("Bad_Name", &anywhere()).await, Ok(None));

    let warnings = registry.warnings();
    for expected in [
        format!("skill file {root}/bad.md ignored: invalid skill name \"Bad_Name\""),
        format!(
            "skill file {root}/missing-description.md ignored: frontmatter requires name and description"
        ),
        format!("skill file {root}/no-frontmatter.md ignored: missing YAML frontmatter"),
        format!("skill file {root}/open-frontmatter.md ignored: missing YAML frontmatter"),
        format!("skill file {root}/non-object.md ignored: missing YAML frontmatter"),
        format!("skill file {root}/not-a-skill/SKILL.md ignored: missing YAML frontmatter"),
    ] {
        // `not-a-skill` has no SKILL.md: it is simply absent.
        if expected.contains("not-a-skill") {
            assert!(!warnings.contains(&expected), "{warnings:?}");
            continue;
        }
        assert!(warnings.contains(&expected), "{expected}\n{warnings:?}");
    }
}

// "accepts the documented boolean spellings for invocation frontmatter"
#[tokio::test]
async fn invocation_booleans_take_dshs_spellings() {
    let home = TempDir::new("booleans");
    let root = home.at(".dsh/skills");
    let truthy = ["true", "TRUE", "\"true\"", "yes", "ON", "1", "\"1\""];
    let falsy = ["false", "FALSE", "\"false\"", "no", "OFF", "0", "\"0\""];
    for (index, value) in truthy.iter().enumerate() {
        write(
            &format!("{root}/truthy-{index}.md"),
            &format!(
                "---\nname: truthy-{index}\ndescription: Truthy {index}\ndisable-model-invocation: {value}\n---\n\nTruthy."
            ),
        );
    }
    for (index, value) in falsy.iter().enumerate() {
        write(
            &format!("{root}/falsy-{index}.md"),
            &format!(
                "---\nname: falsy-{index}\ndescription: Falsy {index}\nuser-invocable: {value}\n---\n\nFalsy."
            ),
        );
    }
    let (registry, _provider) = mount(local(), config(&home));
    for (index, value) in truthy.iter().enumerate() {
        let skill = registry
            .get(&format!("truthy-{index}"), &anywhere())
            .await
            .unwrap()
            .unwrap();
        assert!(!skill.invocation.model_invocable, "{value}");
        assert!(skill.invocation.user_invocable);
    }
    for (index, value) in falsy.iter().enumerate() {
        let skill = registry
            .get(&format!("falsy-{index}"), &anywhere())
            .await
            .unwrap()
            .unwrap();
        assert!(skill.invocation.model_invocable);
        assert!(!skill.invocation.user_invocable, "{value}");
    }
}

// "rejects legacy and invalid invocation frontmatter without hiding valid
// siblings"
#[tokio::test]
async fn legacy_and_invalid_invocation_fields_are_refused() {
    let home = TempDir::new("invalid-invocation");
    let root = home.at(".dsh/skills");
    write_skill(&root, "good-skill", "Good skill", "Body.");
    for (name, field) in [
        ("legacy-model", "disableModelInvocation: true"),
        ("legacy-positive-model", "modelInvocable: false"),
        ("legacy-user", "userInvocable: false"),
        ("bad-string", "disable-model-invocation: maybe"),
        ("bad-value", "user-invocable: null"),
    ] {
        write(
            &format!("{root}/{name}.md"),
            &format!("---\nname: {name}\ndescription: {name}\n{field}\n---\n\nBad.\n"),
        );
    }
    let (registry, _provider) = mount(local(), config(&home));
    assert_eq!(names(&registry, &anywhere()).await, ["good-skill"]);
    let warnings = registry.warnings();
    for expected in [
        format!(
            "skill file {root}/legacy-model.md ignored: invalid invocation frontmatter: Error: frontmatter field \"disableModelInvocation\" is unsupported; use \"disable-model-invocation\""
        ),
        format!(
            "skill file {root}/legacy-positive-model.md ignored: invalid invocation frontmatter: Error: frontmatter field \"modelInvocable\" is unsupported; use \"disable-model-invocation\""
        ),
        format!(
            "skill file {root}/legacy-user.md ignored: invalid invocation frontmatter: Error: frontmatter field \"userInvocable\" is unsupported; use \"user-invocable\""
        ),
        format!(
            "skill file {root}/bad-string.md ignored: invalid invocation frontmatter: TypeError: frontmatter field \"disable-model-invocation\" must be a boolean"
        ),
        format!(
            "skill file {root}/bad-value.md ignored: invalid invocation frontmatter: TypeError: frontmatter field \"user-invocable\" must be a boolean"
        ),
    ] {
        assert!(warnings.contains(&expected), "{expected}\n{warnings:?}");
    }
}

// "supports CRLF frontmatter and ignores delimiter-looking text inside
// YAML values"
#[tokio::test]
async fn crlf_frontmatter_and_dashes_inside_values() {
    let home = TempDir::new("crlf");
    let root = home.at(".dsh/skills");
    write(
        &format!("{root}/crlf-skill.md"),
        &[
            "---",
            "name: crlf-skill",
            "description: CRLF skill",
            "metadata:",
            "  marker: \"----\"",
            "---",
            "",
            "CRLF body.",
        ]
        .join("\r\n"),
    );
    write(
        &format!("{root}/block-skill.md"),
        &[
            "---",
            "name: block-skill",
            "description: |",
            "  Includes a ---- marker that is not a delimiter.",
            "---",
            "",
            "Block body.",
        ]
        .join("\n"),
    );
    let (registry, _provider) = mount(local(), config(&home));
    let crlf = registry
        .get("crlf-skill", &anywhere())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(crlf.content, "CRLF body.");
    assert_eq!(
        serde_json::Value::Object(crlf.metadata.unwrap()),
        serde_json::json!({ "marker": "----" })
    );
    let block = registry
        .get("block-skill", &anywhere())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        block.description,
        "Includes a ---- marker that is not a delimiter.\n"
    );
    assert_eq!(block.content, "Block body.");
}

// "skips invalid YAML skill files without hiding valid siblings"
#[tokio::test]
async fn invalid_yaml_is_skipped_with_a_warning() {
    let home = TempDir::new("invalid-yaml");
    let root = home.at(".dsh/skills");
    write_skill(&root, "good-skill", "Good skill", "Body.");
    write(
        &format!("{root}/bad-yaml.md"),
        "---\nname: bad-yaml\ndescription: [unclosed\n---\n\nBad body.\n",
    );
    let (registry, _provider) = mount(local(), config(&home));
    assert_eq!(names(&registry, &anywhere()).await, ["good-skill"]);
    let prefix = format!("skill file {root}/bad-yaml.md ignored: invalid YAML frontmatter: ");
    assert!(
        registry
            .warnings()
            .iter()
            .any(|warning| warning.starts_with(&prefix)),
        "{:?}",
        registry.warnings()
    );
}

// "publishes regular-file paths for linked skills while retaining their
// resource roots (filesystem service: true)"
#[tokio::test]
async fn a_linked_skill_publishes_its_real_file_and_keeps_its_link_root() {
    let home = TempDir::new("symlink-home");
    let external = TempDir::new("symlink-external");
    let root = home.at(".dsh/skills");
    write_skill(&external.0, "linked-dir", "Linked directory", "Body.");
    write_flat_skill(&external.0, "linked-flat", "Linked flat", "Flat body.");
    std::fs::create_dir_all(&root).unwrap();
    let link = |target: String, name: &str| {
        std::os::unix::fs::symlink(target, format!("{root}/{name}")).unwrap();
    };
    link(external.at("linked-dir"), "linked-dir");
    link(external.at("linked-flat.md"), "linked-flat.md");
    link(external.at("missing"), "broken-link");
    link("/dev/null".into(), "device-link");
    let (registry, _provider) = mount(local(), config(&home));

    let catalog = list(&registry, &anywhere()).await;
    let named: Vec<&str> = catalog.iter().map(|skill| skill.name.as_str()).collect();
    assert_eq!(named, ["linked-dir", "linked-flat"]);
    for name in ["linked-dir", "linked-flat"] {
        let (path, base) = if name == "linked-dir" {
            (
                external.at("linked-dir/SKILL.md"),
                format!("{root}/linked-dir"),
            )
        } else {
            (external.at("linked-flat.md"), root.clone())
        };
        assert_eq!(find(&catalog, name).path.as_deref(), Some(path.as_str()));
        let loaded = registry.get(name, &anywhere()).await.unwrap().unwrap();
        assert_eq!(loaded.path.as_deref(), Some(path.as_str()));
        assert_eq!(
            loaded.resource_base,
            Some(SkillResourceBase::Directory { path: base })
        );
    }
    write(
        &external.at("replacement.md"),
        "---\nname: linked-flat\ndescription: Replacement\n---\n\nReplacement body.\n",
    );
    std::fs::remove_file(format!("{root}/linked-flat.md")).unwrap();
    link(external.at("replacement.md"), "linked-flat.md");
    let replaced = registry
        .get("linked-flat", &anywhere())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(replaced.content, "Replacement body.");
}

/// A filesystem over the local one that fails where told, and whose watch
/// is scripted. (dsh: `TestFileSystem`.)
#[derive(Default)]
struct Fake {
    inner: Option<LocalFileSystem>,
    /// `resolve` fails as absent, or with an I/O error.
    absent_resolve: Mutex<HashSet<String>>,
    error_resolve: Mutex<HashSet<String>>,
    absent_stat: Mutex<HashSet<String>>,
    error_stat: Mutex<HashSet<String>>,
    error_list: Mutex<HashSet<String>>,
    error_read: Mutex<HashSet<String>>,
    absent_read: Mutex<HashSet<String>>,
    stat_overrides: Mutex<HashMap<String, Option<FsInfo>>>,
    /// Reads wait until their signal fires.
    hold_reads: AtomicBool,
    stat_signals: Mutex<Vec<bool>>,
    read_signals: Mutex<Vec<bool>>,
    list_calls: AtomicUsize,
    /// Watches that fail to start, from now on.
    watch_failures: AtomicUsize,
    watches: Mutex<Vec<FakeWatch>>,
}

#[derive(Clone)]
struct FakeWatch {
    path: String,
    listener: WatchListener,
    closed: Arc<AtomicBool>,
}

impl Fake {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Some(LocalFileSystem::new(LocalFsConfig::default()).unwrap()),
            ..Self::default()
        })
    }

    fn inner(&self) -> &LocalFileSystem {
        self.inner.as_ref().unwrap()
    }

    fn has(set: &Mutex<HashSet<String>>, path: &str) -> bool {
        set.lock().unwrap().contains(path)
    }

    fn add(set: &Mutex<HashSet<String>>, path: &str) {
        set.lock().unwrap().insert(path.to_owned());
    }

    fn clear(set: &Mutex<HashSet<String>>) {
        set.lock().unwrap().clear();
    }

    fn open_watches(&self) -> Vec<FakeWatch> {
        self.watches
            .lock()
            .unwrap()
            .iter()
            .filter(|watch| !watch.closed.load(Ordering::SeqCst))
            .cloned()
            .collect()
    }

    /// The local target for a fake one.
    async fn real(&self, target: &FsTarget) -> Result<FsTarget, FsError> {
        self.inner().resolve(&target.display_path, None, None).await
    }
}

fn plain(path: &str) -> FsTarget {
    FsTarget {
        target_key: FsTargetKey::new(path),
        display_path: path.to_owned(),
    }
}

fn io(what: &str) -> FsError {
    FsError::new(FsErrorCode::IoError, format!("{what} temporarily failed"))
}

impl FileSystem for Fake {
    fn watch<'a>(
        &'a self,
        target: &'a FsTarget,
        changed: WatchListener,
        _signal: &'a AbortSignal,
    ) -> FsFuture<'a, FsWatch> {
        Box::pin(async move {
            if self
                .watch_failures
                .try_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                    left.checked_sub(1)
                })
                .is_ok()
            {
                return Err(io("watch"));
            }
            let closed = Arc::new(AtomicBool::new(false));
            self.watches.lock().unwrap().push(FakeWatch {
                path: target.display_path.clone(),
                listener: changed,
                closed: Arc::clone(&closed),
            });
            Ok(FsWatch::new(move || {
                Box::pin(async move {
                    closed.store(true, Ordering::SeqCst);
                })
            }))
        })
    }

    fn resolve<'a>(
        &'a self,
        path: &'a str,
        _cwd: Option<&'a str>,
        _signal: Option<&'a AbortSignal>,
    ) -> FsFuture<'a, FsTarget> {
        Box::pin(async move {
            if Self::has(&self.absent_resolve, path) {
                return Err(FsError::new(FsErrorCode::NotFound, "resolve failed"));
            }
            if Self::has(&self.error_resolve, path) {
                return Err(io("resolve"));
            }
            Ok(plain(path))
        })
    }

    fn process_path(&self, target: &FsTarget) -> String {
        target.target_key.as_str().to_owned()
    }

    fn file_url(&self, target: &FsTarget) -> String {
        format!("file://{}", target.target_key.as_str())
    }

    fn contains(&self, parent: &FsTarget, child: &FsTarget) -> bool {
        child
            .target_key
            .as_str()
            .starts_with(parent.target_key.as_str())
    }

    fn stat<'a>(
        &'a self,
        target: &'a FsTarget,
        signal: Option<&'a AbortSignal>,
    ) -> FsFuture<'a, Option<FsInfo>> {
        Box::pin(async move {
            self.stat_signals.lock().unwrap().push(signal.is_some());
            let path = &target.display_path;
            if Self::has(&self.absent_stat, path) {
                return Err(FsError::new(FsErrorCode::NotFound, "stat failed"));
            }
            if Self::has(&self.error_stat, path) {
                return Err(io("stat"));
            }
            if let Some(info) = self.stat_overrides.lock().unwrap().get(path) {
                return Ok(info.clone());
            }
            match self.real(target).await {
                Ok(real) => self.inner().stat(&real, None).await,
                Err(_) => Ok(None),
            }
        })
    }

    fn lstat<'a>(
        &'a self,
        path: &'a str,
        cwd: Option<&'a str>,
        signal: Option<&'a AbortSignal>,
    ) -> FsFuture<'a, Option<FsPathInfo>> {
        self.inner().lstat(path, cwd, signal)
    }

    fn read_text<'a>(
        &'a self,
        target: &'a FsTarget,
        signal: Option<&'a AbortSignal>,
    ) -> FsFuture<'a, String> {
        Box::pin(async move {
            self.read_signals.lock().unwrap().push(signal.is_some());
            if self.hold_reads.load(Ordering::SeqCst) {
                match signal {
                    Some(signal) => signal.wait().await,
                    None => std::future::pending().await,
                }
                return Err(FsError::aborted("read"));
            }
            let path = &target.display_path;
            if Self::has(&self.absent_read, path) {
                return Err(FsError::new(FsErrorCode::NotFound, "read failed"));
            }
            if Self::has(&self.error_read, path) {
                return Err(io("read"));
            }
            let real = self.real(target).await?;
            self.inner().read_text(&real, signal).await
        })
    }

    fn stream_text<'a>(
        &'a self,
        _target: &'a FsTarget,
        _signal: Option<&'a AbortSignal>,
    ) -> FsFuture<'a, TextStream> {
        Box::pin(async { Err(io("stream")) })
    }

    fn read_bytes<'a>(
        &'a self,
        _target: &'a FsTarget,
        _signal: Option<&'a AbortSignal>,
        _max_bytes: u64,
    ) -> FsFuture<'a, Vec<u8>> {
        Box::pin(async { Err(io("read bytes")) })
    }

    fn read_byte_range<'a>(
        &'a self,
        _target: &'a FsTarget,
        _range: ByteRange,
        _signal: Option<&'a AbortSignal>,
    ) -> FsFuture<'a, Vec<u8>> {
        Box::pin(async { Err(io("read range")) })
    }

    fn list_dir<'a>(
        &'a self,
        target: &'a FsTarget,
        signal: Option<&'a AbortSignal>,
    ) -> FsFuture<'a, Vec<FsDirEntry>> {
        Box::pin(async move {
            self.list_calls.fetch_add(1, Ordering::SeqCst);
            if Self::has(&self.error_list, &target.display_path) {
                return Err(io("list"));
            }
            let real = self.real(target).await?;
            let entries = self.inner().list_dir(&real, signal).await?;
            Ok(entries
                .into_iter()
                .map(|entry| {
                    let path = format!("{}/{}", target.display_path, entry.name);
                    FsDirEntry {
                        target: plain(&path),
                        ..entry
                    }
                })
                .collect())
        })
    }

    fn write_text<'a>(
        &'a self,
        _target: &'a FsTarget,
        _content: &'a str,
        _expected: Option<&'a FsWriteIntent>,
        _signal: Option<&'a AbortSignal>,
    ) -> FsFuture<'a, FsWriteOutcome> {
        Box::pin(async { Err(io("write")) })
    }

    fn edit_text<'a>(
        &'a self,
        _target: &'a FsTarget,
        _edit: &'a FsEditRequest,
        _expected: Option<&'a FsVersion>,
        _signal: Option<&'a AbortSignal>,
    ) -> FsFuture<'a, FsEditOutcome> {
        Box::pin(async { Err(io("edit")) })
    }
}

// "uses the filesystem service for discovery, reads, and project-root
// lookup"
#[tokio::test]
async fn discovery_reads_and_the_project_root_go_through_the_filesystem() {
    let home = TempDir::new("read-fs");
    let project = TempDir::new("project-root-backend");
    let nested = project.at("packages/app");
    let root = home.at(".dsh/skills");
    std::fs::create_dir_all(&nested).unwrap();
    write_flat_skill(&root, "text-skill", "Text skill", "Text body.");
    write_flat_skill(&root, "resolve-fail", "Resolve fail", "Resolve body.");
    write_flat_skill(&root, "stat-fail", "Stat fail", "Stat body.");
    std::fs::create_dir_all(format!("{root}/empty-dir")).unwrap();
    std::fs::create_dir_all(format!("{root}/directory-skill/SKILL.md")).unwrap();
    let mut binary = b"---\nname: binary-skill\ndescription: Binary skill\n---\n\n".to_vec();
    binary.extend([0xff, b'\n']);
    std::fs::write(format!("{root}/binary-skill.md"), binary).unwrap();
    write_skill(
        &project.at(".agents/skills"),
        "backend-root",
        "Backend root skill",
        "Body.",
    );

    let fs = Fake::new();
    Fake::add(&fs.absent_resolve, &format!("{root}/resolve-fail.md"));
    Fake::add(&fs.absent_stat, &format!("{root}/stat-fail.md"));
    Fake::add(&fs.absent_resolve, &format!("{nested}/.git"));
    Fake::add(&fs.absent_stat, &project.at("packages/.git"));
    fs.stat_overrides.lock().unwrap().insert(
        project.at(".git"),
        Some(FsInfo {
            version: FsVersion::new("virtual-git"),
            kind: FsType::Directory,
            size: Some(0),
        }),
    );
    let (registry, _provider) = mount(Arc::clone(&fs) as Arc<dyn FileSystem>, config(&home));

    let listed: Vec<(String, String)> = list(&registry, &at(&nested))
        .await
        .into_iter()
        .map(|skill| (skill.name, skill.source))
        .collect();
    assert_eq!(
        listed,
        [
            ("backend-root".to_owned(), "project-agents".to_owned()),
            ("text-skill".to_owned(), "user-dsh".to_owned()),
        ]
    );
    assert!(fs.list_calls.load(Ordering::SeqCst) > 0);
    assert_eq!(registry.get("binary-skill", &anywhere()).await, Ok(None));
    let warned = format!(
        "skill file {root}/binary-skill.md ignored: failed to read text file at {root}/binary-skill.md: FsError: "
    );
    assert!(
        registry
            .warnings()
            .iter()
            .any(|warning| warning.starts_with(&warned)),
        "{:?}",
        registry.warnings()
    );
}

// "reports transient root reads as incomplete without caching an empty
// catalog"
#[tokio::test]
async fn a_failed_root_read_is_incomplete_and_uncached() {
    let home = TempDir::new("transient-root");
    let root = home.at(".agents/skills");
    write_skill(&root, "stable-skill", "Stable skill", "Body.");
    let fs = Fake::new();
    let (registry, provider) = mount(Arc::clone(&fs) as Arc<dyn FileSystem>, config(&home));
    let snapshot = registry.snapshot(&anywhere()).await.unwrap();
    assert_eq!(snapshot.skills[0].name, "stable-skill");
    assert!(snapshot.complete);

    Fake::add(&fs.error_list, &root);
    provider.observe_host_mutation(&format!("{root}/stable-skill/SKILL.md"));
    let failed = registry.snapshot(&anywhere()).await.unwrap();
    assert!(failed.skills.is_empty());
    assert!(!failed.complete);

    Fake::clear(&fs.error_list);
    let recovered = registry.snapshot(&anywhere()).await.unwrap();
    assert_eq!(recovered.skills[0].name, "stable-skill");
    assert!(recovered.complete);
}

// "distinguishes transient filesystem entry failures from confirmed
// disappearance"
#[tokio::test]
async fn a_transient_entry_failure_is_incomplete_and_a_missing_file_is_gone() {
    let home = TempDir::new("transient-entry");
    let root = home.at(".agents/skills");
    let path = format!("{root}/stable-skill/SKILL.md");
    write_skill(&root, "stable-skill", "Stable skill", "Body.");
    let fs = Fake::new();
    let (registry, provider) = mount(Arc::clone(&fs) as Arc<dyn FileSystem>, config(&home));
    assert!(registry.snapshot(&anywhere()).await.unwrap().complete);
    for failures in [&fs.error_resolve, &fs.error_stat, &fs.error_read] {
        Fake::add(failures, &path);
        provider.observe_host_mutation(&path);
        assert!(!registry.snapshot(&anywhere()).await.unwrap().complete);
        Fake::clear(failures);
    }
    Fake::add(&fs.absent_read, &path);
    provider.observe_host_mutation(&path);
    let gone = registry.snapshot(&anywhere()).await.unwrap();
    assert!(gone.skills.is_empty() && gone.complete);
    Fake::clear(&fs.absent_read);
    provider.observe_host_mutation(&path);
    let back = registry.snapshot(&anywhere()).await.unwrap();
    assert_eq!(back.skills[0].name, "stable-skill");
    assert!(back.complete);
}

// "forwards cancellation to filesystem reads while loading a skill"
#[tokio::test]
async fn a_cancel_reaches_the_filesystem_reads_of_a_load() {
    let home = TempDir::new("read-abort");
    write_skill(
        &home.at(".dsh/skills"),
        "abortable-skill",
        "Abortable skill",
        "Body.",
    );
    let fs = Fake::new();
    let (registry, _provider) = mount(Arc::clone(&fs) as Arc<dyn FileSystem>, config(&home));
    assert_eq!(names(&registry, &anywhere()).await, ["abortable-skill"]);
    fs.stat_signals.lock().unwrap().clear();
    fs.read_signals.lock().unwrap().clear();
    fs.hold_reads.store(true, Ordering::SeqCst);
    let controller = AbortController::new();
    let options = SkillLookupOptions {
        signal: Some(controller.signal()),
        ..SkillLookupOptions::default()
    };
    let registry = Arc::new(registry);
    let loading = tokio::spawn({
        let registry = Arc::clone(&registry);
        async move { registry.get("abortable-skill", &options).await }
    });
    while fs.read_signals.lock().unwrap().is_empty() {
        tokio::task::yield_now().await;
    }
    controller.abort();
    let loaded = tokio::time::timeout(Duration::from_secs(5), loading)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(loaded, Err(karaka_skill::SkillError::Aborted));
    assert_eq!(*fs.stat_signals.lock().unwrap(), [true]);
    assert_eq!(*fs.read_signals.lock().unwrap(), [true]);
}

// "uses fs/observed as a synchronous first-party invalidation path without
// a watcher": the paths that may be a skill's invalidate, and no others.
#[tokio::test]
async fn a_first_party_change_to_a_skill_path_invalidates_at_once() {
    let home = TempDir::new("observed");
    let root = home.at(".agents/skills");
    let (registry, provider) = mount(local(), config(&home));
    assert!(list(&registry, &anywhere()).await.is_empty());
    let invalidations = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&invalidations);
    let _ = registry.on_change(Arc::new(move || {
        counted.fetch_add(1, Ordering::SeqCst);
    }));
    write_skill(&root, "observed-skill", "Observed skill", "Body.");
    for path in [
        home.at("outside.md"),
        root.clone(),
        format!("{root}/observed-skill/references/notes.md"),
        home.at(".dsh/skills/.system/SKILL.md"),
        format!("{root}/flat-skill.md"),
        format!("{root}/observed-skill/SKILL.md"),
    ] {
        provider.observe_host_mutation(&path);
    }
    assert_eq!(invalidations.load(Ordering::SeqCst), 2);
    assert_eq!(names(&registry, &anywhere()).await, ["observed-skill"]);
}

/// Waits until `ready` holds, failing the test rather than hanging.
async fn until(mut ready: impl FnMut() -> bool) {
    for _ in 0..500 {
        if ready() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting");
}

fn watching(home: &TempDir) -> SkillFilesystemConfig {
    SkillFilesystemConfig {
        watch: true,
        ..config(home)
    }
}

// "bounds project watchers and re-observes an evicted project on its next
// lookup": with scripted watches.
#[tokio::test]
async fn project_watches_are_bounded_and_an_evicted_project_comes_back() {
    let home = TempDir::new("lru-home");
    let first = TempDir::new("lru-first");
    let second = TempDir::new("lru-second");
    std::fs::create_dir_all(first.at(".git")).unwrap();
    std::fs::create_dir_all(second.at(".git")).unwrap();
    write_skill(
        &first.at(".agents/skills"),
        "first-project",
        "First project",
        "Body.",
    );
    write_skill(
        &second.at(".agents/skills"),
        "second-project",
        "Second project",
        "Body.",
    );
    let fs = Fake::new();
    let (registry, _provider) = mount(
        Arc::clone(&fs) as Arc<dyn FileSystem>,
        SkillFilesystemConfig {
            watch_max_projects: 1,
            ..watching(&home)
        },
    );
    let changes = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&changes);
    let _ = registry.on_change(Arc::new(move || {
        counted.fetch_add(1, Ordering::SeqCst);
    }));
    let watched = |project: &TempDir| {
        fs.open_watches()
            .iter()
            .any(|watch| watch.path.starts_with(&project.0))
    };
    assert!(
        names(&registry, &at(&first.0))
            .await
            .contains(&"first-project".to_owned())
    );
    assert!(watched(&first));
    assert!(
        names(&registry, &at(&second.0))
            .await
            .contains(&"second-project".to_owned())
    );
    // The first project's watches closed, and the catalogs were told.
    assert!(!watched(&first));
    assert!(watched(&second));
    assert_eq!(changes.load(Ordering::SeqCst), 1);
    write_skill(
        &first.at(".agents/skills"),
        "first-project",
        "First project refreshed",
        "Body.",
    );
    let refreshed = list(&registry, &at(&first.0)).await;
    assert_eq!(
        find(&refreshed, "first-project").description,
        "First project refreshed"
    );
    assert!(watched(&first));
}

// "keeps skills loadable across persistent watcher startup failures without
// caching them"
#[tokio::test]
async fn a_watch_that_wont_start_leaves_skills_loadable_but_uncached() {
    let home = TempDir::new("watch-start-error");
    let root = home.at(".dsh/skills");
    write_skill(&root, "retry-skill", "retry-skill", "Body.");
    let fs = Fake::new();
    fs.watch_failures.store(usize::MAX, Ordering::SeqCst);
    let (registry, _provider) = mount(Arc::clone(&fs) as Arc<dyn FileSystem>, watching(&home));
    for _ in 0..2 {
        let snapshot = registry.snapshot(&anywhere()).await.unwrap();
        assert_eq!(snapshot.skills[0].name, "retry-skill");
        assert!(!snapshot.complete);
    }
    let body = registry
        .get("retry-skill", &anywhere())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(body.content, "Body.");
    assert!(registry.warnings().iter().any(|warning| warning
        == &format!(
            "skill-filesystem: failed to watch {root}: FsError: watch temporarily failed"
        )));
    // Once watching works, the catalog is complete again.
    fs.watch_failures.store(0, Ordering::SeqCst);
    assert!(registry.snapshot(&anywhere()).await.unwrap().complete);
}

// "filters events, coalesces invalidation, recovers runtime errors, and
// contains late callbacks": a burst invalidates once, a failure is warned
// of and rewatched, and nothing is heard after disposal.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_burst_invalidates_once_a_failure_rewatches_and_disposal_silences() {
    let home = TempDir::new("watch-runtime");
    let root = home.at(".dsh/skills");
    write_skill(&root, "watched-skill", "watched-skill", "Body.");
    let fs = Fake::new();
    let (registry, provider) = mount(Arc::clone(&fs) as Arc<dyn FileSystem>, watching(&home));
    assert_eq!(names(&registry, &anywhere()).await, ["watched-skill"]);
    // The root and its one skill directory are watched.
    let paths: HashSet<String> = fs
        .open_watches()
        .into_iter()
        .map(|watch| watch.path)
        .collect();
    assert!(paths.contains(&root), "{paths:?}");
    assert!(
        paths.contains(&format!("{root}/watched-skill")),
        "{paths:?}"
    );
    let changes = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&changes);
    let _ = registry.on_change(Arc::new(move || {
        counted.fetch_add(1, Ordering::SeqCst);
    }));
    let watch = fs
        .open_watches()
        .into_iter()
        .find(|watch| watch.path == root)
        .unwrap();
    (watch.listener)(None);
    (watch.listener)(None);
    until(|| changes.load(Ordering::SeqCst) >= 1).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    // One for the burst, and one when the rewatch confirms the plan.
    let after_burst = changes.load(Ordering::SeqCst);
    assert!((1..=3).contains(&after_burst), "{after_burst}");

    let opened = fs.watches.lock().unwrap().len();
    (watch.listener)(Some(FsError::new(
        FsErrorCode::IoError,
        "runtime watch failed",
    )));
    until(|| fs.watches.lock().unwrap().len() > opened).await;
    assert!(registry.warnings().iter().any(|warning| warning
        == &format!("skill-filesystem: watcher for {root} failed: FsError: runtime watch failed")));
    assert!(changes.load(Ordering::SeqCst) > after_burst);
    assert!(registry.snapshot(&anywhere()).await.unwrap().complete);

    provider.dispose().await;
    provider.dispose().await;
    assert!(fs.open_watches().is_empty());
    let settled = changes.load(Ordering::SeqCst);
    (watch.listener)(None);
    (watch.listener)(Some(FsError::new(FsErrorCode::IoError, "late error")));
    provider.observe_host_mutation(&format!("{root}/watched-skill/SKILL.md"));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(changes.load(Ordering::SeqCst), settled);
}

// "re-probes a retained root after child unlink and observes immediate
// recreation", and "replaces a retained watcher when its root emits
// unlinkDir": a missing root is watched from its nearest existing
// ancestor, and watched again once it is back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_missing_root_is_watched_from_its_ancestor_until_it_returns() {
    let home = TempDir::new("watch-reprobe");
    let root = home.at(".dsh/skills");
    write_skill(&root, "old-skill", "old-skill", "Body.");
    let fs = Fake::new();
    let (registry, _provider) = mount(Arc::clone(&fs) as Arc<dyn FileSystem>, watching(&home));
    assert_eq!(names(&registry, &anywhere()).await, ["old-skill"]);
    let root_watch = fs
        .open_watches()
        .into_iter()
        .find(|watch| watch.path == root)
        .unwrap();

    std::fs::remove_dir_all(&root).unwrap();
    (root_watch.listener)(None);
    let ancestor = home.at(".dsh");
    until(|| fs.open_watches().iter().any(|watch| watch.path == ancestor)).await;
    assert!(!fs.open_watches().iter().any(|watch| watch.path == root));
    let empty = registry.snapshot(&anywhere()).await.unwrap();
    assert!(empty.skills.is_empty() && empty.complete);

    write_skill(&root, "recreated-skill", "recreated-skill", "Body.");
    let ancestor_watch = fs
        .open_watches()
        .into_iter()
        .find(|watch| watch.path == ancestor)
        .unwrap();
    (ancestor_watch.listener)(None);
    until(|| fs.open_watches().iter().any(|watch| watch.path == root)).await;
    until(|| fs.open_watches().iter().all(|watch| watch.path != ancestor)).await;
    assert_eq!(names(&registry, &anywhere()).await, ["recreated-skill"]);
}

// "contains repeated disposal and late first-party observations"
#[tokio::test]
async fn a_disposed_provider_still_lists_and_ignores_observations() {
    let home = TempDir::new("watch-dispose");
    let not_a_directory = home.at("not-a-directory");
    write(&not_a_directory, "not a skill root");
    write_skill(
        &home.at(".agents/skills"),
        "disposed-skill",
        "Disposed skill",
        "Body.",
    );
    let fs = Fake::new();
    let (_registry, provider) = mount(
        Arc::clone(&fs) as Arc<dyn FileSystem>,
        SkillFilesystemConfig {
            custom_skill_dirs: vec![not_a_directory],
            ..watching(&home)
        },
    );
    let before = provider.list(&anywhere()).await.unwrap();
    let named: Vec<String> = before
        .candidates
        .iter()
        .map(|skill| skill.name.clone())
        .collect();
    assert_eq!(named, ["disposed-skill"]);
    provider.dispose().await;
    provider.dispose().await;
    provider.observe_host_mutation(&home.at(".agents/skills/disposed-skill/SKILL.md"));
    let after = provider.list(&anywhere()).await.unwrap();
    let named: Vec<String> = after
        .candidates
        .iter()
        .map(|skill| skill.name.clone())
        .collect();
    assert_eq!(named, ["disposed-skill"]);
    assert!(fs.open_watches().is_empty());
}

// "validates watcher tunables at plugin load": the one kept.
#[test]
fn a_config_with_no_projects_to_watch_is_refused() {
    let error = SkillFilesystemConfig {
        watch_max_projects: 0,
        ..SkillFilesystemConfig::default()
    }
    .validate()
    .unwrap_err();
    assert!(error.to_string().contains("watchMaxProjects"), "{error}");
}

// "uses default home root resolution without exposing builtin skills"
#[tokio::test]
async fn the_homes_and_the_bundled_root_come_from_the_environment() {
    let home = TempDir::new("env-home");
    let bundled = home.at("bundled-skills");
    write_skill(&home.at(".dsh/skills"), "env-skill", "Env skill", "Body.");
    write_skill(&bundled, "env-bundled-skill", "Env bundled skill", "Body.");
    let env = HashMap::from([
        ("DSH_HOME", home.at(".dsh")),
        ("DSH_AGENTS_HOME", home.at(".agents")),
        ("DSH_BUNDLED_SKILL_DIR", bundled.clone()),
        ("HOME", home.at("user")),
    ]);
    let plain = SkillFilesystemConfig {
        watch: false,
        ..SkillFilesystemConfig::default()
    };
    let (registry, _provider) = mount_with_env(local(), plain.clone(), env.clone());
    assert_eq!(
        names(&registry, &anywhere()).await,
        ["env-bundled-skill", "env-skill"]
    );

    // An isolated provider sees only its explicit roots: the environment's
    // bundled root is a default one.
    let custom = home.at("custom-only");
    write_skill(
        &custom,
        "custom-isolated-skill",
        "Custom isolated skill",
        "Body.",
    );
    let (isolated, provider) = mount_with_env(
        local(),
        SkillFilesystemConfig {
            provider_name: "isolated".into(),
            include_default_roots: false,
            custom_skill_dirs: vec![custom],
            ..plain.clone()
        },
        env.clone(),
    );
    assert_eq!(
        names(&isolated, &anywhere()).await,
        ["custom-isolated-skill"]
    );
    assert_eq!(provider.name(), "isolated");

    // An empty override is unset, and the default is under HOME.
    let empty = HashMap::from([
        ("DSH_HOME", "  ".to_owned()),
        ("HOME", home.at("empty-user")),
    ]);
    let (registry, provider) = mount_with_env(local(), plain, empty);
    assert!(list(&registry, &anywhere()).await.is_empty());
    assert_eq!(provider.name(), "filesystem");
    std::fs::create_dir_all(home.at("empty-user/.dsh/skills")).unwrap();
    write_skill(
        &home.at("empty-user/.dsh/skills"),
        "home-skill",
        "Home skill",
        "Body.",
    );
    provider.observe_host_mutation(&home.at("empty-user/.dsh/skills/home-skill/SKILL.md"));
    assert_eq!(names(&registry, &anywhere()).await, ["home-skill"]);
}

/// Whether this system can watch at all: no inotify instances left means
/// it can't.
async fn can_watch(dir: &TempDir) -> bool {
    let fs = LocalFileSystem::new(LocalFsConfig::default()).unwrap();
    let target = fs.resolve(&dir.0, None, None).await.unwrap();
    let controller = AbortController::new();
    match fs
        .watch(&target, Arc::new(|_| {}), &controller.signal())
        .await
    {
        Ok(watch) => {
            watch.close().await;
            true
        }
        Err(error) if error.to_string().contains("Too many open files") => {
            eprintln!("skipped: {error}");
            false
        }
        Err(error) => panic!("the probe watch didn't start: {error}"),
    }
}

/// Waits until a fresh listing passes `accept`.
async fn eventually(
    registry: &SkillRegistry,
    accept: impl Fn(&[SkillSummary]) -> bool,
) -> Vec<SkillSummary> {
    for _ in 0..500 {
        let skills = list(registry, &anywhere()).await;
        if accept(&skills) {
            return skills;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for the watcher");
}

// "refreshes additions, metadata changes, deletions, and a recreated
// missing root", and "refreshes frontmatter through a followed skill
// symlink": the real filesystem's watches.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_real_watches_refresh_the_catalog() {
    let home = TempDir::new("watch-home");
    if !can_watch(&home).await {
        return;
    }
    let agents = home.at(".agents/skills");
    let external = TempDir::new("watch-external");
    let (registry, provider) = mount(local(), watching(&home));
    assert!(list(&registry, &anywhere()).await.is_empty());

    write_skill(&agents, "watched-skill", "First description", "First body.");
    let added = eventually(&registry, |skills| {
        skills.iter().any(|skill| skill.name == "watched-skill")
    })
    .await;
    assert_eq!(
        find(&added, "watched-skill").description,
        "First description"
    );

    write_skill(
        &agents,
        "watched-skill",
        "Second description",
        "Second body.",
    );
    eventually(&registry, |skills| {
        skills
            .iter()
            .any(|skill| skill.description == "Second description")
    })
    .await;

    write_flat_skill(&agents, "flat-added", "Flat added", "Body.");
    eventually(&registry, |skills| {
        skills.iter().any(|skill| skill.name == "flat-added")
    })
    .await;

    std::fs::rename(
        format!("{agents}/watched-skill"),
        format!("{agents}/renamed-skill"),
    )
    .unwrap();
    write_skill(&agents, "renamed-skill", "Renamed skill", "Body.");
    eventually(&registry, |skills| {
        skills.iter().any(|skill| skill.name == "renamed-skill")
            && !skills.iter().any(|skill| skill.name == "watched-skill")
    })
    .await;

    std::fs::remove_dir_all(home.at(".agents")).unwrap();
    eventually(&registry, <[SkillSummary]>::is_empty).await;
    write_skill(&agents, "recreated-skill", "Recreated", "Body.");
    eventually(&registry, |skills| {
        skills.iter().any(|skill| skill.name == "recreated-skill")
    })
    .await;

    // A skill directory linked from elsewhere is watched through the link.
    write_skill(
        &external.0,
        "linked-skill",
        "First linked description",
        "Body.",
    );
    std::os::unix::fs::symlink(
        external.at("linked-skill"),
        format!("{agents}/linked-skill"),
    )
    .unwrap();
    eventually(&registry, |skills| {
        skills
            .iter()
            .any(|skill| skill.description == "First linked description")
    })
    .await;
    write_skill(
        &external.0,
        "linked-skill",
        "Second linked description",
        "Body.",
    );
    eventually(&registry, |skills| {
        skills
            .iter()
            .any(|skill| skill.description == "Second linked description")
    })
    .await;
    provider.dispose().await;
}
