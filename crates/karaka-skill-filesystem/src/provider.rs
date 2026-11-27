//! The provider: its roots, their discovery, and a skill file's reading and
//! parsing. (dsh: `FileSystemSkillProvider`, `discoverRoot`,
//! `parseSkillFile`, `readSkillText` and `findProjectRoot` in
//! `skill-filesystem/src/index.ts`.)

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use karaka_fs::{AbortSignal, FileSystem, FsError, FsErrorCode, FsType};
use karaka_skill::{
    BUNDLED_SKILL_RANK, GetFuture, ListFuture, SkillCandidate, SkillDefinition,
    SkillInvocationPolicy, SkillLookupOptions, SkillProvider, SkillProviderControl,
    SkillProviderObservation, SkillResourceBase, is_skill_name, source,
};
use serde_json::{Map, Value};

use crate::frontmatter::{invocation_policy, metadata, parse_frontmatter, string_field};
use crate::watch::WatchManager;

const PROJECT_DSH_RANK: f64 = 100.0;
const PROJECT_AGENTS_RANK: f64 = 200.0;
const CUSTOM_RANK: f64 = 300.0;
const USER_DSH_RANK: f64 = 400.0;
const USER_AGENTS_RANK: f64 = 500.0;

/// The default of [`SkillFilesystemConfig::watch_max_projects`].
pub const DEFAULT_WATCH_MAX_PROJECTS: usize = 128;

/// The environment variables dsh reads its roots from. (dsh: `DSH_HOME`,
/// `DSH_AGENTS_HOME` and `DSH_BUNDLED_SKILL_DIR`.)
pub const DSH_HOME_ENV: &str = "DSH_HOME";
pub const AGENTS_HOME_ENV: &str = "DSH_AGENTS_HOME";
pub const BUNDLED_SKILL_DIR_ENV: &str = "DSH_BUNDLED_SKILL_DIR";

/// The provider's settings. (dsh: `Config`, without its Chokidar tunables.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillFilesystemConfig {
    /// Unique in the registry's layer. `filesystem` by default.
    pub provider_name: String,
    /// Whether the project and user roots, and the environment's bundled
    /// root, are scanned around the custom ones. `true` by default.
    pub include_default_roots: bool,
    /// The harness home; `$DSH_HOME`, else `~/.dsh`.
    pub dsh_home: Option<String>,
    /// The shared agent home; `$DSH_AGENTS_HOME`, else `~/.agents`.
    pub agents_home: Option<String>,
    /// Scanned after the project roots and before the user roots.
    pub custom_skill_dirs: Vec<String>,
    /// Whether the roots are watched for changes. `true` by default.
    pub watch: bool,
    /// The most project roots whose skill directories stay watched.
    pub watch_max_projects: usize,
    /// The bundled root; by default `$DSH_BUNDLED_SKILL_DIR`, and only with
    /// the default roots.
    pub bundled_skill_dir: Option<String>,
}

impl Default for SkillFilesystemConfig {
    fn default() -> Self {
        Self {
            provider_name: "filesystem".into(),
            include_default_roots: true,
            dsh_home: None,
            agents_home: None,
            custom_skill_dirs: Vec::new(),
            watch: true,
            watch_max_projects: DEFAULT_WATCH_MAX_PROJECTS,
            bundled_skill_dir: None,
        }
    }
}

impl SkillFilesystemConfig {
    /// Checks what dsh's config schema and tunable checks refuse here.
    ///
    /// # Errors
    ///
    /// An empty provider name, or no projects to watch.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.provider_name.is_empty() {
            return Err(ConfigError(
                "skill-filesystem: providerName must not be empty".into(),
            ));
        }
        if self.watch_max_projects < 1 {
            return Err(ConfigError(
                "skill-filesystem: watchMaxProjects must be a positive integer".into(),
            ));
        }
        Ok(())
    }
}

/// Why a config was refused, in dsh's words.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigError(pub String);

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ConfigError {}

/// One root the provider scans. (dsh: `SkillRoot`.)
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SkillRoot {
    pub(crate) path: String,
    pub(crate) source: &'static str,
    pub(crate) rank: f64,
    /// The user root's `.system` directory is the harness's own.
    pub(crate) skip_system: bool,
    pub(crate) project_root: Option<String>,
}

/// Where a candidate's file is, and its resource base. (dsh:
/// `LocalLocator`.)
#[derive(Clone, Debug)]
struct LocalLocator {
    path: String,
    directory: String,
}

/// A parsed skill file. (dsh: `ParsedSkill`.)
struct ParsedSkill {
    path: String,
    name: String,
    description: String,
    when_to_use: Option<String>,
    invocation: SkillInvocationPolicy,
    metadata: Option<Map<String, Value>>,
    content: String,
}

/// The provider of SKILL.md files under local roots. (dsh:
/// `FileSystemSkillProvider`.)
pub struct FileSystemSkillProvider {
    name: String,
    fs: Arc<dyn FileSystem>,
    control: SkillProviderControl,
    include_default_roots: bool,
    dsh_home: String,
    agents_home: String,
    custom_skill_dirs: Vec<String>,
    bundled_skill_dir: Option<String>,
    watch: Arc<WatchManager>,
}

impl FileSystemSkillProvider {
    /// A provider reading through `fs`, given its registration's `control`,
    /// with its roots from `config` and the process's environment.
    ///
    /// # Errors
    ///
    /// An empty provider name, or no projects to watch.
    pub fn new(
        fs: Arc<dyn FileSystem>,
        control: SkillProviderControl,
        config: SkillFilesystemConfig,
    ) -> Result<Arc<Self>, ConfigError> {
        Self::with_env(fs, control, config, |name| std::env::var(name).ok())
    }

    /// As [`new`](Self::new), reading the environment through `env`.
    pub fn with_env(
        fs: Arc<dyn FileSystem>,
        control: SkillProviderControl,
        config: SkillFilesystemConfig,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<Arc<Self>, ConfigError> {
        config.validate()?;
        let home = env("HOME").unwrap_or_else(|| "/".into());
        let root = |configured: Option<String>, env_name: &str, default: &str| {
            let chosen = configured
                .or_else(|| env(env_name).filter(|value| !value.trim().is_empty()))
                .unwrap_or_else(|| format!("{home}/{default}"));
            absolute(&expand_home(&chosen, &home))
        };
        let dsh_home = root(config.dsh_home, DSH_HOME_ENV, ".dsh");
        let agents_home = root(config.agents_home, AGENTS_HOME_ENV, ".agents");
        // The environment's bundled root is a default root: an isolated
        // provider sees only its explicit roots.
        let bundled = config.bundled_skill_dir.or_else(|| {
            config
                .include_default_roots
                .then(|| env(BUNDLED_SKILL_DIR_ENV))
                .flatten()
        });
        let watch = WatchManager::new(
            Arc::clone(&fs),
            control.clone(),
            config.watch,
            config.watch_max_projects,
        );
        Ok(Arc::new(Self {
            name: config.provider_name,
            fs,
            control,
            include_default_roots: config.include_default_roots,
            dsh_home,
            agents_home,
            custom_skill_dirs: config
                .custom_skill_dirs
                .iter()
                .map(|dir| absolute(dir))
                .collect(),
            bundled_skill_dir: bundled.map(|dir| absolute(&dir)),
            watch,
        }))
    }

    /// Invalidates the catalogs at once after a first-party write or edit
    /// of `path`, when it may be a skill file under a watched root. (dsh:
    /// `observeHostMutation`, which its plugin calls on `fs/observed`.)
    pub fn observe_host_mutation(&self, path: &str) {
        self.watch.observe_host_mutation(&absolute(path));
    }

    /// Closes every watch; later events and observations change nothing.
    /// It is the same however often it is called. (dsh: `dispose`.)
    pub async fn dispose(&self) {
        self.watch.dispose().await;
    }

    /// The roots for a lookup in `cwd`, in rank order. (dsh: `roots`.)
    async fn roots(&self, cwd: Option<&str>) -> Vec<SkillRoot> {
        let mut roots = Vec::new();
        if self.include_default_roots
            && let Some(cwd) = cwd
        {
            let project = self.find_project_root(&absolute(cwd)).await;
            for (directory, source, rank) in [
                (".dsh/skills", source::PROJECT_DSH, PROJECT_DSH_RANK),
                (
                    ".agents/skills",
                    source::PROJECT_AGENTS,
                    PROJECT_AGENTS_RANK,
                ),
            ] {
                roots.push(SkillRoot {
                    path: join(&project, directory),
                    source,
                    rank,
                    skip_system: false,
                    project_root: Some(project.clone()),
                });
            }
        }
        for path in &self.custom_skill_dirs {
            roots.push(SkillRoot {
                path: path.clone(),
                source: source::CUSTOM,
                rank: CUSTOM_RANK,
                skip_system: false,
                project_root: None,
            });
        }
        if self.include_default_roots {
            roots.push(SkillRoot {
                path: join(&self.dsh_home, "skills"),
                source: source::USER_DSH,
                rank: USER_DSH_RANK,
                skip_system: true,
                project_root: None,
            });
            roots.push(SkillRoot {
                path: join(&self.agents_home, "skills"),
                source: source::USER_AGENTS,
                rank: USER_AGENTS_RANK,
                skip_system: false,
                project_root: None,
            });
        }
        if let Some(path) = &self.bundled_skill_dir {
            roots.push(SkillRoot {
                path: path.clone(),
                source: source::BUNDLED,
                rank: BUNDLED_SKILL_RANK,
                skip_system: false,
                project_root: None,
            });
        }
        roots
    }

    /// The nearest directory up from `cwd` holding `.git`, else `cwd`.
    /// (dsh: `findProjectRoot`.)
    async fn find_project_root(&self, cwd: &str) -> String {
        let mut current = PathBuf::from(cwd);
        loop {
            if self.exists(&join(&current.to_string_lossy(), ".git")).await {
                return current.to_string_lossy().into_owned();
            }
            if !current.pop() {
                return cwd.to_owned();
            }
        }
    }

    /// Whether `path` exists; a backend failure is a no. (dsh:
    /// `pathExistsInFileSystem`.)
    async fn exists(&self, path: &str) -> bool {
        let Ok(target) = self.fs.resolve(path, None, None).await else {
            return false;
        };
        matches!(self.fs.stat(&target, None).await, Ok(Some(_)))
    }

    /// One root's candidates, in name order. (dsh: `discoverRoot`.)
    async fn discover(&self, root: &SkillRoot) -> Result<Vec<SkillCandidate>, FsError> {
        let mut candidates = Vec::new();
        for entry in self.entries(root).await? {
            if root.skip_system && entry.name == ".system" {
                continue;
            }
            let entry_path = entry.target.display_path.clone();
            let locator = match entry.kind {
                FsType::Directory => LocalLocator {
                    path: join(&entry_path, "SKILL.md"),
                    directory: entry_path,
                },
                FsType::File if entry.name.ends_with(".md") => LocalLocator {
                    path: entry_path,
                    directory: root.path.clone(),
                },
                _ => continue,
            };
            let Some(parsed) = self.parse(&locator.path, None).await? else {
                continue;
            };
            candidates.push(SkillCandidate {
                path: Some(parsed.path),
                name: parsed.name,
                description: parsed.description,
                when_to_use: parsed.when_to_use,
                invocation: parsed.invocation,
                source: root.source.to_owned(),
                provider: self.name.clone(),
                resource_base: Some(SkillResourceBase::Directory {
                    path: locator.directory.clone(),
                }),
                rank: root.rank,
                locator: Arc::new(locator),
                metadata: parsed.metadata,
            });
        }
        Ok(candidates)
    }

    /// A root's entries; none when it is absent. (dsh:
    /// `listSkillRootEntriesFromFileSystem`.)
    async fn entries(&self, root: &SkillRoot) -> Result<Vec<karaka_fs::FsDirEntry>, FsError> {
        let listed = async {
            let target = self.fs.resolve(&root.path, None, None).await?;
            self.fs.list_dir(&target, None).await
        };
        match listed.await {
            Ok(entries) => Ok(entries),
            Err(error) if absent(&error) => Ok(Vec::new()),
            Err(error) => Err(error),
        }
    }

    /// A skill file, parsed and checked; `None`, with a warning where dsh
    /// warns, for one that isn't a skill. (dsh: `parseSkillFile`.)
    async fn parse(
        &self,
        path: &str,
        signal: Option<&AbortSignal>,
    ) -> Result<Option<ParsedSkill>, FsError> {
        let Some((real_path, content)) = self.read(path, signal).await? else {
            return Ok(None);
        };
        if signal.is_some_and(AbortSignal::is_aborted) {
            return Err(FsError::aborted("read"));
        }
        let ignored = |reason: String| {
            self.control
                .warn(format!("skill file {path} ignored: {reason}"));
            Ok(None)
        };
        let frontmatter = match parse_frontmatter(&content) {
            Err(error) => return ignored(format!("invalid YAML frontmatter: {error}")),
            Ok(None) => return ignored("missing YAML frontmatter".into()),
            Ok(Some(frontmatter)) => frontmatter,
        };
        let data = &frontmatter.data;
        let (Some(name), Some(description)) = (
            string_field(data, "name"),
            string_field(data, "description"),
        ) else {
            return ignored("frontmatter requires name and description".into());
        };
        if !is_skill_name(&name) {
            return ignored(format!("invalid skill name \"{name}\""));
        }
        let invocation = match invocation_policy(data) {
            Ok(invocation) => invocation,
            Err(error) => {
                return ignored(format!("invalid invocation frontmatter: {error}"));
            }
        };
        Ok(Some(ParsedSkill {
            path: real_path,
            name,
            description,
            when_to_use: string_field(data, "whenToUse"),
            invocation,
            metadata: metadata(data),
            content: frontmatter.body.trim().to_owned(),
        }))
    }

    /// A skill file's real path and text; `None` when it is absent, isn't a
    /// regular file, or isn't text, the last with a warning. (dsh:
    /// `readSkillTextFromFileSystem`.)
    async fn read(
        &self,
        path: &str,
        signal: Option<&AbortSignal>,
    ) -> Result<Option<(String, String)>, FsError> {
        let aborted = || signal.is_some_and(AbortSignal::is_aborted);
        if aborted() {
            return Err(FsError::aborted("read"));
        }
        let target = match self.fs.resolve(path, None, signal).await {
            Ok(target) => target,
            Err(error) if absent(&error) => return Ok(None),
            Err(error) => return Err(error),
        };
        let info = match self.fs.stat(&target, signal).await {
            Ok(info) => info,
            Err(error) if !aborted() && absent(&error) => return Ok(None),
            Err(error) => return Err(error),
        };
        if info.is_none_or(|info| info.kind != FsType::File) {
            return Ok(None);
        }
        match self.fs.read_text(&target, signal).await {
            Ok(text) => Ok(Some((self.fs.process_path(&target), text))),
            Err(error) if aborted() => Err(error),
            Err(error) if absent(&error) => Ok(None),
            Err(error) if error.code() == FsErrorCode::NotText => {
                self.control.warn(format!(
                    "skill file {path} ignored: failed to read text file at {}: FsError: {error}",
                    target.display_path
                ));
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }
}

impl SkillProvider for FileSystemSkillProvider {
    fn name(&self) -> &str {
        &self.name
    }

    /// The candidates of every root for the lookup's `cwd`. A watch that
    /// failed to start makes the observation incomplete, so it isn't cached
    /// and the watch is tried again on the next lookup. (dsh: `list`.)
    fn list<'a>(&'a self, options: &'a SkillLookupOptions) -> ListFuture<'a> {
        Box::pin(async move {
            let roots = self.roots(options.cwd.as_deref()).await;
            let watched = self.watch.observe_roots(&roots).await;
            if let Err(error) = &watched
                && self.watch.disposed()
            {
                return Err(Box::new(error.clone()) as _);
            }
            let mut candidates = Vec::new();
            for root in &roots {
                candidates.extend(self.discover(root).await?);
            }
            Ok(SkillProviderObservation {
                candidates,
                complete: watched.is_ok(),
            })
        })
    }

    /// The whole skill, read again from its file. (dsh: `get`.)
    fn get<'a>(
        &'a self,
        candidate: &'a SkillCandidate,
        options: &'a SkillLookupOptions,
    ) -> GetFuture<'a> {
        Box::pin(async move {
            let Some(locator) = candidate.locator.downcast_ref::<LocalLocator>() else {
                return Ok(None);
            };
            let Some(parsed) = self.parse(&locator.path, options.signal.as_ref()).await? else {
                return Ok(None);
            };
            Ok(Some(SkillDefinition {
                path: Some(parsed.path),
                name: parsed.name,
                description: parsed.description,
                when_to_use: parsed.when_to_use,
                invocation: parsed.invocation,
                source: candidate.source.clone(),
                provider: self.name.clone(),
                resource_base: Some(SkillResourceBase::Directory {
                    path: locator.directory.clone(),
                }),
                content: parsed.content,
                metadata: parsed.metadata,
            }))
        })
    }
}

impl std::fmt::Debug for FileSystemSkillProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileSystemSkillProvider")
            .field("name", &self.name)
            .field("include_default_roots", &self.include_default_roots)
            .field("custom_skill_dirs", &self.custom_skill_dirs)
            .finish_non_exhaustive()
    }
}

/// A path that isn't there. (dsh: `isAbsentSkillPathError`.)
pub(crate) fn absent(error: &FsError) -> bool {
    matches!(
        error.code(),
        FsErrorCode::NotFound | FsErrorCode::NotDirectory
    )
}

/// `path` joined to `child`.
pub(crate) fn join(path: &str, child: &str) -> String {
    Path::new(path).join(child).to_string_lossy().into_owned()
}

/// `~`, `~/` and `~\` against `home`. (dsh: `expandHomePath`.)
fn expand_home(path: &str, home: &str) -> String {
    if path == "~" {
        return home.to_owned();
    }
    match path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        Some(rest) => join(home, rest),
        None => path.to_owned(),
    }
}

/// `path` made absolute against the process's directory, with `.` and `..`
/// resolved by name, as Node's `path.resolve`.
pub(crate) fn absolute(path: &str) -> String {
    let path = Path::new(path);
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(path)
    };
    let mut normal = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::ParentDir => {
                normal.pop();
            }
            Component::CurDir => {}
            other => normal.push(other.as_os_str()),
        }
    }
    normal.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::{absolute, expand_home};

    #[test]
    fn paths_resolve_as_node_resolves_them() {
        assert_eq!(absolute("/a/b/../c/./d"), "/a/c/d");
        assert_eq!(absolute("/.."), "/");
        assert_eq!(expand_home("~", "/home/u"), "/home/u");
        assert_eq!(expand_home("~/x", "/home/u"), "/home/u/x");
        assert_eq!(expand_home("/x/~", "/home/u"), "/x/~");
    }
}
