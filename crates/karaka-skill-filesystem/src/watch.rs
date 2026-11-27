//! The roots' watches. (dsh: `SkillWatchManager` in
//! `skill-filesystem/src/index.ts`.)
//!
//! dsh watches each root with Chokidar at depth 1, so it hears a root's
//! entries and each skill directory's `SKILL.md`, and a missing root by
//! polling the next path under its nearest existing ancestor. Karaka's
//! filesystem seam watches one directory's direct entries, and tells only
//! that something changed, not what. So a root here is watched by a watch
//! on the root and one on each of its directories, or, while it is
//! missing, one on its nearest existing ancestor. Every change, and every
//! failure, re-plans that root's watches and invalidates the catalogs,
//! coalesced; dsh's filter of which paths matter has no paths to read.
//!
//! A root is held by its owners: `shared:` for a custom, user or bundled
//! root, and `project:` for a project's two, whose projects are kept in
//! least-recently-used order, at most `watch_max_projects` of them. An
//! evicted project's watches close, and the catalogs are invalidated, so
//! its next lookup observes it again.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use karaka_fs::{AbortController, FileSystem, FsError, FsType, FsWatch, WatchListener};
use karaka_skill::SkillProviderControl;
use tokio::runtime::Handle;

use crate::provider::{SkillRoot, absent};

pub(crate) struct WatchManager {
    fs: Arc<dyn FileSystem>,
    control: SkillProviderControl,
    enabled: bool,
    max_projects: usize,
    /// Where follow-up work runs: a watch's callback may come on any thread.
    runtime: Option<Handle>,
    lifecycle: AbortController,
    state: Mutex<State>,
    me: Weak<WatchManager>,
}

#[derive(Default)]
struct State {
    roots: HashMap<String, Arc<Watched>>,
    /// Least recently used first: each project's root paths.
    projects: Vec<(String, Vec<String>)>,
    closing: bool,
    invalidation_queued: bool,
}

struct Watched {
    root: SkillRoot,
    owners: Mutex<HashSet<String>>,
    /// One opening at a time. (dsh: `opening`.)
    slot: tokio::sync::Mutex<Slot>,
}

#[derive(Default)]
struct Slot {
    /// The directories watched, by process path.
    plan: Option<Vec<String>>,
    watches: Vec<FsWatch>,
    healthy: bool,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // Every change is an insert, removal or flag, so the state is whole
    // even after a panic.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl WatchManager {
    pub(crate) fn new(
        fs: Arc<dyn FileSystem>,
        control: SkillProviderControl,
        enabled: bool,
        max_projects: usize,
    ) -> Arc<Self> {
        let runtime = Handle::try_current().ok();
        let manager = Arc::new_cyclic(|me| Self {
            fs,
            control,
            enabled,
            max_projects,
            runtime,
            lifecycle: AbortController::new(),
            state: Mutex::new(State::default()),
            me: me.clone(),
        });
        // The registration's end disposes the watches. (dsh: the control
        // signal's abort listener.)
        if let Some(runtime) = &manager.runtime {
            let signal = manager.control.signal.clone();
            let weak = Arc::downgrade(&manager);
            runtime.spawn(async move {
                signal.wait().await;
                if let Some(manager) = weak.upgrade() {
                    manager.dispose().await;
                }
            });
        }
        manager
    }

    pub(crate) fn disposed(&self) -> bool {
        lock(&self.state).closing
    }

    /// Holds and watches `roots` for one lookup. (dsh: `observeRoots`.)
    ///
    /// # Errors
    ///
    /// A watch that wouldn't start; its root is tried again next time.
    pub(crate) async fn observe_roots(&self, roots: &[SkillRoot]) -> Result<(), FsError> {
        let (held, released, evicted) = {
            let mut state = lock(&self.state);
            if state.closing {
                return Ok(());
            }
            let mut held = Vec::new();
            let mut projects: Vec<(String, Vec<&SkillRoot>)> = Vec::new();
            for root in roots {
                match &root.project_root {
                    None => held.push(retain(&mut state, root, format!("shared:{}", root.path))),
                    Some(project) => match projects.iter_mut().find(|(held, _)| held == project) {
                        Some((_, grouped)) => grouped.push(root),
                        None => projects.push((project.clone(), vec![root])),
                    },
                }
            }
            for (project, grouped) in projects {
                state.projects.retain(|(held, _)| *held != project);
                state.projects.push((
                    project.clone(),
                    grouped.iter().map(|root| root.path.clone()).collect(),
                ));
                for root in grouped {
                    held.push(retain(&mut state, root, format!("project:{project}")));
                }
            }
            let mut released = Vec::new();
            let mut evicted = false;
            while state.projects.len() > self.max_projects {
                let (project, paths) = state.projects.remove(0);
                for path in paths {
                    released.extend(release(&mut state, &path, &format!("project:{project}")));
                }
                evicted = true;
            }
            (held, released, evicted)
        };
        for watched in released {
            close(&watched).await;
        }
        let mut outcome = Ok(());
        if self.enabled {
            for watched in held {
                if let Err(error) = self.ensure(&watched).await
                    && outcome.is_ok()
                {
                    outcome = Err(error);
                }
            }
        }
        if evicted {
            self.control.invalidate();
        }
        outcome
    }

    /// Invalidates at once when `path` may be a skill file under a held
    /// root. (dsh: `observeHostMutation`.)
    pub(crate) fn observe_host_mutation(&self, path: &str) {
        let state = lock(&self.state);
        if state.closing {
            return;
        }
        let relevant = state
            .roots
            .values()
            .any(|watched| potential_skill_path(&watched.root, path));
        drop(state);
        if relevant {
            self.control.invalidate();
        }
    }

    /// Closes every watch. (dsh: `dispose`.)
    pub(crate) async fn dispose(&self) {
        let roots: Vec<Arc<Watched>> = {
            let mut state = lock(&self.state);
            state.closing = true;
            state.projects.clear();
            state.roots.drain().map(|(_, watched)| watched).collect()
        };
        self.lifecycle.abort();
        for watched in roots {
            close(&watched).await;
        }
    }

    /// Opens the root's watches, unless they already match its plan. (dsh:
    /// `ensureWatcher`, `ensureCurrentWatcher` and `replaceWatcher`.)
    async fn ensure(&self, watched: &Arc<Watched>) -> Result<(), FsError> {
        let mut slot = watched.slot.lock().await;
        if self.disposed() || lock(&watched.owners).is_empty() {
            return Ok(());
        }
        let opened = async {
            let plan = self.plan(&watched.root).await?;
            if slot.healthy && slot.plan.as_ref() == Some(&plan) {
                return Ok(None);
            }
            for watch in std::mem::take(&mut slot.watches) {
                watch.close().await;
            }
            slot.plan = None;
            let mut watches = Vec::new();
            for path in &plan {
                match self.open(&watched.root.path, path).await {
                    Ok(watch) => watches.push(watch),
                    Err(error) => {
                        for watch in watches {
                            watch.close().await;
                        }
                        return Err(error);
                    }
                }
            }
            Ok(Some((plan, watches)))
        };
        match opened.await {
            Ok(None) => Ok(()),
            Ok(Some((plan, watches))) => {
                if self.disposed() || lock(&watched.owners).is_empty() {
                    for watch in watches {
                        watch.close().await;
                    }
                    return Ok(());
                }
                slot.plan = Some(plan);
                slot.watches = watches;
                slot.healthy = true;
                Ok(())
            }
            Err(error) => {
                slot.healthy = false;
                if !self.disposed() {
                    self.control.warn(format!(
                        "skill-filesystem: failed to watch {}: FsError: {error}",
                        watched.root.path
                    ));
                }
                Err(error)
            }
        }
    }

    /// The directories that watch a root: the root and each of its
    /// directories, or, while it is missing, its nearest existing ancestor.
    /// (dsh: `resolveRootWatchMode`.)
    async fn plan(&self, root: &SkillRoot) -> Result<Vec<String>, FsError> {
        let mut candidate = root.path.clone();
        loop {
            let target = match self.fs.resolve(&candidate, None, None).await {
                Ok(target) => Some(target),
                Err(error) if absent(&error) => None,
                Err(error) => return Err(error),
            };
            if let Some(target) = target
                && self
                    .fs
                    .stat(&target, None)
                    .await?
                    .is_some_and(|info| info.kind == FsType::Directory)
            {
                let mut plan = vec![self.fs.process_path(&target)];
                if candidate == root.path {
                    for entry in self.fs.list_dir(&target, None).await? {
                        if entry.kind != FsType::Directory
                            || (root.skip_system && entry.name == ".system")
                        {
                            continue;
                        }
                        plan.push(self.fs.process_path(&entry.target));
                    }
                }
                plan.sort();
                return Ok(plan);
            }
            match Path::new(&candidate).parent() {
                Some(parent) => candidate = parent.to_string_lossy().into_owned(),
                None => return Ok(Vec::new()),
            }
        }
    }

    async fn open(&self, root: &str, path: &str) -> Result<FsWatch, FsError> {
        let target = self.fs.resolve(path, None, None).await?;
        let weak = self.me.clone();
        let root = root.to_owned();
        let listener: WatchListener = Arc::new(move |error: Option<FsError>| {
            if let Some(manager) = weak.upgrade() {
                manager.heard(&root, error);
            }
        });
        self.fs
            .watch(&target, listener, &self.lifecycle.signal())
            .await
    }

    /// A watch of `root` heard a change, or failed. (dsh:
    /// `handleWatchEvent`, `handleAncestorWatchEvent` and
    /// `handleWatcherError`.)
    fn heard(&self, root: &str, error: Option<FsError>) {
        let watched = {
            let state = lock(&self.state);
            if state.closing {
                return;
            }
            state.roots.get(root).cloned()
        };
        if let Some(error) = &error {
            self.control.warn(format!(
                "skill-filesystem: watcher for {root} failed: FsError: {error}"
            ));
        }
        self.queue_invalidation();
        let (Some(watched), Some(runtime)) = (watched, &self.runtime) else {
            return;
        };
        let weak = self.me.clone();
        let failed = error.is_some();
        runtime.spawn(async move {
            let Some(manager) = weak.upgrade() else {
                return;
            };
            if failed {
                watched.slot.lock().await.healthy = false;
            }
            // A failed rewatch was warned of; the next lookup tries again.
            if manager.ensure(&watched).await.is_ok() {
                manager.queue_invalidation();
            }
        });
    }

    /// Invalidates once for a burst of changes. (dsh: `queueInvalidation`.)
    fn queue_invalidation(&self) {
        {
            let mut state = lock(&self.state);
            if state.closing || state.invalidation_queued {
                return;
            }
            state.invalidation_queued = true;
        }
        let Some(runtime) = &self.runtime else {
            lock(&self.state).invalidation_queued = false;
            self.control.invalidate();
            return;
        };
        let weak = self.me.clone();
        runtime.spawn(async move {
            tokio::task::yield_now().await;
            let Some(manager) = weak.upgrade() else {
                return;
            };
            let closing = {
                let mut state = lock(&manager.state);
                state.invalidation_queued = false;
                state.closing
            };
            if !closing {
                manager.control.invalidate();
            }
        });
    }
}

/// Adds `owner` to the root at `root.path`. (dsh: `retainRoot`.)
fn retain(state: &mut State, root: &SkillRoot, owner: String) -> Arc<Watched> {
    let watched = state.roots.entry(root.path.clone()).or_insert_with(|| {
        Arc::new(Watched {
            root: root.clone(),
            owners: Mutex::new(HashSet::new()),
            slot: tokio::sync::Mutex::new(Slot::default()),
        })
    });
    lock(&watched.owners).insert(owner);
    Arc::clone(watched)
}

/// Drops `owner` from the root at `path`, and gives the root to close when
/// it was the last. (dsh: `releaseRoot`.)
fn release(state: &mut State, path: &str, owner: &str) -> Option<Arc<Watched>> {
    let watched = state.roots.get(path)?;
    let mut owners = lock(&watched.owners);
    owners.remove(owner);
    if !owners.is_empty() {
        return None;
    }
    drop(owners);
    state.roots.remove(path)
}

async fn close(watched: &Watched) {
    let mut slot = watched.slot.lock().await;
    slot.plan = None;
    slot.healthy = false;
    for watch in std::mem::take(&mut slot.watches) {
        watch.close().await;
    }
}

/// Whether `path` may be a skill file of `root`: a Markdown file in it, or
/// a `SKILL.md` one directory down, outside the user root's `.system`.
/// (dsh: `isPotentialSkillPath`.)
fn potential_skill_path(root: &SkillRoot, path: &str) -> bool {
    let Ok(child) = Path::new(path).strip_prefix(&root.path) else {
        return false;
    };
    let segments: Vec<&str> = child
        .components()
        .map(|component| component.as_os_str().to_str().unwrap_or_default())
        .collect();
    if segments.is_empty() || segments.len() > 2 {
        return false;
    }
    if root.skip_system && segments[0] == ".system" {
        return false;
    }
    match segments.as_slice() {
        [file] => file.ends_with(".md"),
        [_, file] => *file == "SKILL.md",
        _ => false,
    }
}
