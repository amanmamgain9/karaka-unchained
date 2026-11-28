//! The local skill provider: skills as SKILL.md files under project,
//! custom, user and bundled roots. (dsh: `packages/skill/skill-filesystem`,
//! `dsh-skill-filesystem`.)
//!
//! A root holds directory bundles, `<name>/SKILL.md`, and flat Markdown
//! skills, `<name>.md`, each with YAML frontmatter naming and describing
//! it. Roots are scanned in rank order: the project's `.dsh/skills` (100)
//! and `.agents/skills` (200), from the nearest directory up from the
//! lookup's `cwd` holding `.git`; each custom directory (300); the user's
//! `~/.dsh/skills` (400), its `.system` skipped, and `~/.agents/skills`
//! (500); and a bundled root (600). Everything is read through the
//! filesystem seam, and the roots are watched, so a change invalidates the
//! registry's catalogs.
//!
//! - `frontmatter.rs`: the frontmatter, and the fields read from it.
//! - `provider.rs`: the provider, its roots, discovery and reading.
//! - `watch.rs`: the roots' watches.
//! - `plugin.rs`: the plugin that registers the provider.

mod frontmatter;
mod plugin;
mod provider;
mod watch;

pub use plugin::{SkillFilesystemPlugin, SkillFilesystemPresetPlugin};
pub use provider::{
    AGENTS_HOME_ENV, BUNDLED_SKILL_DIR_ENV, ConfigError, DEFAULT_WATCH_MAX_PROJECTS, DSH_HOME_ENV,
    FileSystemSkillProvider, SkillFilesystemConfig,
};
