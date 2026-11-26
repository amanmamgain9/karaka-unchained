//! [`WorkingDirectory`]: each session's directory, the projection it is
//! folded into, and the checks and changes of it. (dsh:
//! `WorkingDirectoryService` in `working-directory/src/index.ts`.)

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex, PoisonError};

use karaka_agent::Agent;
use karaka_fs::{AbortController, AbortSignal, FileSystem, FsError, FsErrorCode, FsType};
use karaka_session::{
    Entry, Passthrough, Session, SessionEvent, SessionHeader, SessionId, SessionStore, UserMessage,
    UserSource,
};
use karaka_session_projection::{ClientView, Projection, SessionProjections};
use serde_json::json;
use tokio::sync::watch;

use crate::error::{WorkingDirectoryError, quoted};

/// The event a change is logged as. (dsh: `working-directory/change`,
/// `{ cwd }`.)
pub const CHANGE_EVENT: &str = "working-directory/change";

/// The runtime context the directory is shown in.
pub const CONTEXT_NAME: &str = "working-directory:current";

/// The service's settings. (dsh: `Config`.)
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorkingDirectoryConfig {
    /// The directory of a session with no project of its own. Must be
    /// absolute; the process's directory when unset. (dsh:
    /// `defaultDirectory`.)
    pub default_directory: Option<String>,
}

/// The `workingDirectory` projection: the last directory a change logged,
/// or the header's project, or `None` when neither is recorded. Clients see
/// the state itself. (dsh: the unit `WorkingDirectoryService` registers.)
pub struct WorkingDirectoryProjection;

impl Projection for WorkingDirectoryProjection {
    const KEY: &'static str = "workingDirectory";
    type State = Option<String>;

    fn state_version(&self) -> u64 {
        1
    }

    fn init(&self, header: &SessionHeader, _: u64) -> Option<String> {
        header.cwd.clone()
    }

    fn apply(&self, _: &Option<String>, entry: &Entry) -> Option<Option<String>> {
        match &entry.event {
            SessionEvent::Passthrough(event) if event.event_type == CHANGE_EVENT => event
                .data
                .get("cwd")
                .and_then(|cwd| cwd.as_str())
                .map(|cwd| Some(cwd.to_owned())),
            _ => None,
        }
    }
}

impl ClientView for WorkingDirectoryProjection {
    type View = Option<String>;

    fn view(&self, state: &Option<String>) -> Option<String> {
        state.clone()
    }
}

type Warn = Arc<dyn Fn(&str) + Send + Sync>;

/// One owner for each session's working directory and the notices of its
/// changes. (dsh: `WorkingDirectoryService`, `ctx.workingDirectory`.)
///
/// The directory is the `workingDirectory` projection of the session's
/// log: a session's header gives its project, and each change is a
/// `working-directory/change` event. Each session's checks and changes run
/// one at a time, in the order they were asked for. Each takes a cancel
/// signal, and [`stop`](Self::stop) cancels them all.
pub struct WorkingDirectory {
    default_directory: String,
    fs: Arc<dyn FileSystem>,
    projections: Arc<SessionProjections>,
    sessions: Arc<SessionStore>,
    /// Each session's turn. (dsh: `pending`.)
    turns: Mutex<HashMap<SessionId, Arc<tokio::sync::Mutex<()>>>>,
    /// Aborted by `stop`. (dsh: `lifecycle`.)
    lifecycle: AbortController,
    /// How many operations are running. (dsh: `operations`.)
    running: watch::Sender<usize>,
    warn: Warn,
}

impl WorkingDirectory {
    /// A service over `fs`, keeping its state as the `workingDirectory`
    /// projection of `projections`, which it registers.
    ///
    /// # Errors
    ///
    /// A default directory that isn't absolute.
    pub fn new(
        config: WorkingDirectoryConfig,
        fs: Arc<dyn FileSystem>,
        projections: Arc<SessionProjections>,
        sessions: Arc<SessionStore>,
    ) -> Result<Self, WorkingDirectoryError> {
        let default_directory = match config.default_directory {
            Some(directory) => directory,
            None => std::env::current_dir()
                .map(|dir| dir.to_string_lossy().into_owned())
                .unwrap_or_default(),
        };
        if !default_directory.starts_with('/') {
            return Err(WorkingDirectoryError::RelativeDefault);
        }
        // A second service shares the unit, at the same version.
        let _ = projections.register(WorkingDirectoryProjection);
        Ok(Self {
            default_directory,
            fs,
            projections,
            sessions,
            turns: Mutex::default(),
            lifecycle: AbortController::new(),
            running: watch::Sender::new(0),
            warn: Arc::new(|warning| eprintln!("{warning}")),
        })
    }

    /// The same service, with its warnings going to `warn` rather than
    /// stderr. (dsh: `ctx.logger.warn`.)
    pub fn with_warnings(mut self, warn: impl Fn(&str) + Send + Sync + 'static) -> Self {
        self.warn = Arc::new(warn);
        self
    }

    /// The directory of a session with no project of its own. (dsh:
    /// `defaultDirectory`.)
    pub fn default_directory(&self) -> &str {
        &self.default_directory
    }

    /// The session's directory as logged, without touching the filesystem.
    /// (dsh: `get`.)
    pub fn get(&self, session: &Session) -> String {
        self.recorded(session)
            .unwrap_or_else(|| self.default_directory.clone())
    }

    /// What the model is shown. (dsh: `contextText`.)
    pub fn context_text(&self, session: &Session) -> String {
        format!("Current working directory: {}.", quoted(&self.get(session)))
    }

    /// Whether [`stop`](Self::stop) was called.
    pub fn is_stopped(&self) -> bool {
        self.lifecycle.signal().is_aborted()
    }

    /// Checks the session's directory still exists, and when it doesn't,
    /// returns to the session's project, or the default directory, and
    /// queues a notice of it. A session whose directory isn't recorded yet
    /// gets it recorded. (dsh: `ensure`.)
    ///
    /// # Errors
    ///
    /// The project is gone too; a cancel; a stopped service; the
    /// filesystem or the log refused.
    pub async fn ensure(
        &self,
        agent: &dyn Agent,
        signal: Option<&AbortSignal>,
    ) -> Result<String, WorkingDirectoryError> {
        let session = self.session(agent)?;
        self.serialize(&session, signal, async {
            let current = self.get(&session);
            let target = self
                .fs
                .resolve(&current, None, signal)
                .await
                .map_err(|error| self.fs_error(error))?;
            let info = self
                .fs
                .stat(&target, signal)
                .await
                .map_err(|error| self.fs_error(error))?;
            self.check(signal)?;
            if info.is_some_and(|info| info.kind == FsType::Directory) {
                if self.recorded(&session).is_none() {
                    session.append(change(&current))?;
                }
                return Ok(current);
            }
            let original = session
                .header()
                .cwd
                .clone()
                .unwrap_or_else(|| self.default_directory.clone());
            let restored = self.directory(&original, &original, signal).await?;
            self.commit(agent, &session, &restored, true, signal)?;
            Ok(restored)
        })
        .await
    }

    /// Changes the session's directory to `path`, absolute or relative to
    /// the current one, and queues a notice of it. Running processes and
    /// permissions don't change. (dsh: `set`.)
    ///
    /// # Errors
    ///
    /// An empty path, or one that isn't an existing directory; a cancel; a
    /// stopped service; the filesystem or the log refused.
    pub async fn set(
        &self,
        agent: &dyn Agent,
        path: &str,
        signal: Option<&AbortSignal>,
    ) -> Result<String, WorkingDirectoryError> {
        let session = self.session(agent)?;
        self.serialize(&session, signal, async {
            if path.is_empty() {
                return Err(WorkingDirectoryError::EmptyPath);
            }
            let directory = self.directory(path, &self.get(&session), signal).await?;
            self.commit(agent, &session, &directory, false, signal)?;
            Ok(directory)
        })
        .await
    }

    /// Stops the service: what is pending is cancelled, and what comes
    /// after is refused, with "working-directory service disposed". Waits
    /// until nothing runs. (dsh: the service's disposal effect.)
    pub async fn stop(&self) {
        self.lifecycle.abort();
        let mut running = self.running.subscribe();
        let _ = running.wait_for(|count| *count == 0).await;
    }

    fn recorded(&self, session: &Session) -> Option<String> {
        self.projections
            .state_of::<WorkingDirectoryProjection>(session)
            .and_then(|state| (*state).clone())
    }

    fn session(&self, agent: &dyn Agent) -> Result<Session, WorkingDirectoryError> {
        self.sessions
            .get(agent.id())
            .ok_or_else(|| WorkingDirectoryError::NoSession(agent.id().clone()))
    }

    /// The canonical directory `path` names from `cwd`. (dsh: `directory`.)
    async fn directory(
        &self,
        path: &str,
        cwd: &str,
        signal: Option<&AbortSignal>,
    ) -> Result<String, WorkingDirectoryError> {
        let target = self
            .fs
            .resolve(path, Some(cwd), signal)
            .await
            .map_err(|error| self.fs_error(error))?;
        let info = self
            .fs
            .stat(&target, signal)
            .await
            .map_err(|error| self.fs_error(error))?;
        self.check(signal)?;
        if !info.is_some_and(|info| info.kind == FsType::Directory) {
            return Err(WorkingDirectoryError::Missing(path.to_owned()));
        }
        Ok(self.fs.process_path(&target))
    }

    /// Logs the change to `directory` and queues its notice. A notice that
    /// can't be queued is warned of; the change stands. (dsh: `commit`.)
    fn commit(
        &self,
        agent: &dyn Agent,
        session: &Session,
        directory: &str,
        recovered: bool,
        signal: Option<&AbortSignal>,
    ) -> Result<(), WorkingDirectoryError> {
        self.check(signal)?;
        let previous = self.get(session);
        if previous == directory {
            return Ok(());
        }
        let seq = session.append(change(directory))?;
        let text = if recovered {
            format!(
                "The working directory {} is unavailable. The working directory is now {}.",
                quoted(&previous),
                quoted(directory)
            )
        } else {
            format!(
                "The working directory changed from {} to {}.",
                quoted(&previous),
                quoted(directory)
            )
        };
        // dsh's id is random; Karaka's are named after a seq, as the loop
        // names its messages.
        let notice = UserMessage::text(
            format!("working-directory-{seq}"),
            UserSource::Other(json!({ "kind": "working-directory" })),
            text,
        );
        if let Err(error) = agent.inject(notice) {
            (self.warn)(&format!(
                "working-directory: committed {}, but could not queue its notice: {error}",
                quoted(directory)
            ));
        }
        Ok(())
    }

    /// Runs `action` in the session's turn, racing it against the caller's
    /// signal and the service's stop: either drops it, so it commits
    /// nothing. (dsh: `serialize`, with `AbortSignal.any`.)
    async fn serialize<T>(
        &self,
        session: &Session,
        signal: Option<&AbortSignal>,
        action: impl Future<Output = Result<T, WorkingDirectoryError>>,
    ) -> Result<T, WorkingDirectoryError> {
        let _running = Running::new(&self.running);
        self.check(signal)?;
        let turn = Arc::clone(
            self.turns
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .entry(session.id().clone())
                .or_default(),
        );
        let lifecycle = self.lifecycle.signal();
        let caller = signal.cloned().unwrap_or_else(AbortSignal::never);
        let run = async {
            let _turn = turn.lock().await;
            self.check(signal)?;
            action.await
        };
        tokio::select! {
            biased;
            () = lifecycle.wait() => Err(WorkingDirectoryError::Disposed),
            () = caller.wait() => Err(WorkingDirectoryError::Cancelled),
            result = run => result,
        }
    }

    /// Refuses once stopped or cancelled. (dsh: `throwIfAborted`.)
    fn check(&self, signal: Option<&AbortSignal>) -> Result<(), WorkingDirectoryError> {
        if self.is_stopped() {
            return Err(WorkingDirectoryError::Disposed);
        }
        if signal.is_some_and(AbortSignal::is_aborted) {
            return Err(WorkingDirectoryError::Cancelled);
        }
        Ok(())
    }

    fn fs_error(&self, error: FsError) -> WorkingDirectoryError {
        if error.code() != FsErrorCode::Aborted {
            return WorkingDirectoryError::Fs(error);
        }
        if self.is_stopped() {
            WorkingDirectoryError::Disposed
        } else {
            WorkingDirectoryError::Cancelled
        }
    }
}

fn change(directory: &str) -> SessionEvent {
    SessionEvent::Passthrough(Passthrough {
        event_type: CHANGE_EVENT.into(),
        data: json!({ "cwd": directory }),
        surface_op: None,
        source_event_seqs: None,
    })
}

/// Counts one running operation for as long as it lives.
struct Running<'a>(&'a watch::Sender<usize>);

impl<'a> Running<'a> {
    fn new(count: &'a watch::Sender<usize>) -> Self {
        count.send_modify(|count| *count += 1);
        Self(count)
    }
}

impl Drop for Running<'_> {
    fn drop(&mut self) {
        self.0.send_modify(|count| *count -= 1);
    }
}

impl std::fmt::Debug for WorkingDirectory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkingDirectory")
            .field("default_directory", &self.default_directory)
            .finish_non_exhaustive()
    }
}
