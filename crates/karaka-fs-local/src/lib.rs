//! The host filesystem behind the filesystem seam. (dsh:
//! `@deepseek-ai/dsh-fs-local`, 0.2.1-rc.2.)
//!
//! [`LocalFileSystem`] keys each target by its realpath, so every alias of
//! a file shares its stale guards. It reads regular UTF-8 text and refuses
//! binary files, and writes by staging an owner-only file in a private
//! directory beside the target, then publishing it atomically.
//! [`LocalFsPlugin`] provides it as the `dyn FileSystem` port.
//!
//! Unix only, for now: dsh's Windows publication (`win32.ts`) isn't
//! ported, and the crate doesn't build elsewhere.
//!
//! - `fsio.rs`: the mechanics, apart from the port.
//! - `paths.rs`: Node's path functions, which dsh's paths follow.
//! - `local.rs`: the backend, its per-target turns, and watching.
//! - `plugin.rs`: the plugin that provides the port.

#[cfg(not(unix))]
compile_error!("karaka-fs-local supports Unix only: dsh's Windows publication isn't ported");

pub mod fsio;
mod local;
mod paths;
mod plugin;

pub use local::{ConfigError, DEFAULT_DIFF_BASIS_MAX_BYTES, LocalFileSystem, LocalFsConfig};
pub use plugin::LocalFsPlugin;
