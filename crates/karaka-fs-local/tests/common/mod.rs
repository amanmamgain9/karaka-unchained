//! A temporary directory per test, removed when it is dropped. Tests touch
//! nothing outside one.

#![allow(dead_code)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(prefix: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let name = format!(
            "karaka-{prefix}-{}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let path = std::env::temp_dir().join(name);
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    pub fn join(&self, relative: &str) -> PathBuf {
        self.0.join(relative)
    }

    /// `relative` under the directory, as a string.
    pub fn at(&self, relative: &str) -> String {
        self.join(relative).to_str().unwrap().to_owned()
    }

    pub fn str(&self) -> String {
        self.0.to_str().unwrap().to_owned()
    }

    /// The directory's realpath.
    pub fn real(&self) -> String {
        real(&self.0)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        // A test may have left a directory unreadable.
        restore_permissions(&self.0);
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn restore_permissions(path: &Path) {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return;
    };
    if metadata.is_dir() {
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
        if let Ok(entries) = std::fs::read_dir(path) {
            for entry in entries.flatten() {
                restore_permissions(&entry.path());
            }
        }
    }
}

pub fn real(path: impl AsRef<Path>) -> String {
    std::fs::canonicalize(path)
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned()
}

pub fn write(path: impl AsRef<Path>, content: impl AsRef<[u8]>) {
    std::fs::write(path, content).unwrap();
}

pub fn read(path: impl AsRef<Path>) -> String {
    std::fs::read_to_string(path).unwrap()
}

pub fn mode(path: impl AsRef<Path>) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

pub fn chmod(path: impl AsRef<Path>, mode: u32) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

pub fn symlink(original: impl AsRef<Path>, link: impl AsRef<Path>) {
    std::os::unix::fs::symlink(original, link).unwrap();
}

/// The names in `dir` that look like staging residue.
pub fn residue(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains(".tmp"))
        .collect()
}
