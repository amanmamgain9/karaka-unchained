//! Each session's working directory: changes, recovery, serialization,
//! cancellation and stopping, the notices, and the runtime context. (dsh:
//! `working-directory/tests/working-directory.spec.ts`, and the directory
//! half of `admission.spec.ts`.)
//!
//! Deferred: the admission case's file-policy and approval lines, which
//! need `dsh-sandbox-policy` and `dsh-user-approval`; the `working_directory`
//! tool's case, which is a plugin of its own; and "refuses an Agent
//! disposed during directory inspection", since Karaka's agents live as long
//! as the runtime.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use karaka_agent::{
    Agent, Agents, FrameListener, FrameListeners, FrameSubscription, HookFuture, InboxError, Next,
    RequestError, RequestErrorAction, RequestErrorHandler, RequestErrorHooks, WhenIdle,
};
use karaka_agent_loop::{AgentLoopPlugin, InboxProjection, LoopConfig};
use karaka_fs::{
    AbortController, AbortSignal, ByteRange, FileSystem, FsDirEntry, FsEditOutcome, FsEditRequest,
    FsFuture, FsInfo, FsPathInfo, FsTarget, FsVersion, FsWriteIntent, FsWriteOutcome, TextStream,
};
use karaka_fs_local::{LocalFileSystem, LocalFsConfig};
use karaka_kernel::{BoxError, Plugin, PluginContext, PortId, Runtime};
use karaka_model::{
    Block, ErrorKind, Event, EventStream, FinishReason, Model, ModelError, ModelRegistry,
    ModelsPlugin, Request, Usage, stream_of,
};
use karaka_session::{
    CancelCause, ContentBlock, Session, SessionEvent, SessionHeader, SessionId, SessionStore,
    SessionsPlugin, TurnEndReason, UserMessage, UserSource,
};
use karaka_session_projection::{SessionProjections, SessionProjectionsPlugin};
use karaka_system_prompt::{
    AssembleContext, AssembleFuture, AssembleListener, AssembleNext, AssembledContext,
    PromptAssembly, Scope, SystemPrompt, SystemPromptConfig, SystemPromptPlugin,
    render_context_snapshot, render_prompt,
};
use karaka_tools::ToolsPlugin;
use karaka_working_directory::{
    WorkingDirectory, WorkingDirectoryConfig, WorkingDirectoryError, WorkingDirectoryPlugin,
};
use serde_json::{Value, json};
use tokio::sync::Notify;

const ROUTE: &str = "mock-work";

/// A temporary directory, by its realpath, removed when dropped.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "karaka-working-directory-{}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&path).unwrap();
        Self(std::fs::canonicalize(path).unwrap())
    }

    fn at(&self, relative: &str) -> String {
        self.0.join(relative).to_str().unwrap().to_owned()
    }

    fn root(&self) -> String {
        self.0.to_str().unwrap().to_owned()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The host's files, with a `stat` that can be held until released.
struct Parked {
    inner: LocalFileSystem,
    hold: Mutex<Option<(Arc<Notify>, Arc<Notify>)>>,
    stats: AtomicUsize,
}

impl Parked {
    /// Holds the next `stat` after it reads, until `release` is notified;
    /// `entered` is notified once it holds.
    fn hold_next(&self) -> (Arc<Notify>, Arc<Notify>) {
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        *self.hold.lock().unwrap() = Some((Arc::clone(&entered), Arc::clone(&release)));
        (entered, release)
    }
}

impl FileSystem for Parked {
    fn resolve<'a>(
        &'a self,
        path: &'a str,
        cwd: Option<&'a str>,
        signal: Option<&'a AbortSignal>,
    ) -> FsFuture<'a, FsTarget> {
        self.inner.resolve(path, cwd, signal)
    }

    fn process_path(&self, target: &FsTarget) -> String {
        self.inner.process_path(target)
    }

    fn file_url(&self, target: &FsTarget) -> String {
        self.inner.file_url(target)
    }

    fn contains(&self, parent: &FsTarget, child: &FsTarget) -> bool {
        self.inner.contains(parent, child)
    }

    fn stat<'a>(
        &'a self,
        target: &'a FsTarget,
        signal: Option<&'a AbortSignal>,
    ) -> FsFuture<'a, Option<FsInfo>> {
        Box::pin(async move {
            self.stats.fetch_add(1, Ordering::SeqCst);
            let info = self.inner.stat(target, None).await;
            let hold = self.hold.lock().unwrap().take();
            if let Some((entered, release)) = hold {
                entered.notify_one();
                release.notified().await;
            }
            if signal.is_some_and(AbortSignal::is_aborted) {
                return Err(karaka_fs::FsError::aborted("stat"));
            }
            info
        })
    }

    fn lstat<'a>(
        &'a self,
        path: &'a str,
        cwd: Option<&'a str>,
        signal: Option<&'a AbortSignal>,
    ) -> FsFuture<'a, Option<FsPathInfo>> {
        self.inner.lstat(path, cwd, signal)
    }

    fn read_text<'a>(
        &'a self,
        target: &'a FsTarget,
        signal: Option<&'a AbortSignal>,
    ) -> FsFuture<'a, String> {
        self.inner.read_text(target, signal)
    }

    fn stream_text<'a>(
        &'a self,
        target: &'a FsTarget,
        signal: Option<&'a AbortSignal>,
    ) -> FsFuture<'a, TextStream> {
        self.inner.stream_text(target, signal)
    }

    fn read_bytes<'a>(
        &'a self,
        target: &'a FsTarget,
        signal: Option<&'a AbortSignal>,
        max_bytes: u64,
    ) -> FsFuture<'a, Vec<u8>> {
        self.inner.read_bytes(target, signal, max_bytes)
    }

    fn read_byte_range<'a>(
        &'a self,
        target: &'a FsTarget,
        range: ByteRange,
        signal: Option<&'a AbortSignal>,
    ) -> FsFuture<'a, Vec<u8>> {
        self.inner.read_byte_range(target, range, signal)
    }

    fn list_dir<'a>(
        &'a self,
        target: &'a FsTarget,
        signal: Option<&'a AbortSignal>,
    ) -> FsFuture<'a, Vec<FsDirEntry>> {
        self.inner.list_dir(target, signal)
    }

    fn write_text<'a>(
        &'a self,
        target: &'a FsTarget,
        content: &'a str,
        expected: Option<&'a FsWriteIntent>,
        signal: Option<&'a AbortSignal>,
    ) -> FsFuture<'a, FsWriteOutcome> {
        self.inner.write_text(target, content, expected, signal)
    }

    fn edit_text<'a>(
        &'a self,
        target: &'a FsTarget,
        edit: &'a FsEditRequest,
        expected: Option<&'a FsVersion>,
        signal: Option<&'a AbortSignal>,
    ) -> FsFuture<'a, FsEditOutcome> {
        self.inner.edit_text(target, edit, expected, signal)
    }
}

/// Provides a given filesystem as the port.
struct FsPlugin(Arc<Parked>);

impl Plugin for FsPlugin {
    fn name(&self) -> &str {
        "fs-parked"
    }

    fn provides(&self) -> Vec<PortId> {
        vec![PortId::of::<dyn FileSystem>()]
    }

    async fn setup(&self, ctx: &mut PluginContext<'_>) -> Result<(), BoxError> {
        let fs: Arc<dyn FileSystem> = Arc::clone(&self.0) as Arc<dyn FileSystem>;
        ctx.provide(fs);
        Ok(())
    }
}

/// Plays one scripted reply per call: an answer, or a failure.
struct Scripted {
    replies: Mutex<VecDeque<Result<&'static str, ErrorKind>>>,
    requests: Mutex<Vec<Request>>,
}

impl Model for Scripted {
    fn provider(&self) -> &str {
        "mock"
    }

    fn stream(&self, request: Request) -> EventStream {
        self.requests.lock().unwrap().push(request);
        let events: Vec<Result<Event, ModelError>> = match self.replies.lock().unwrap().pop_front()
        {
            Some(Ok(text)) => stream_of(
                &[Block::Text(text.into())],
                FinishReason::Stop,
                Usage::default(),
            )
            .into_iter()
            .map(Ok)
            .collect(),
            Some(Err(kind)) => vec![Err(ModelError::new(kind, "busy"))],
            None => vec![Err(ModelError::new(ErrorKind::InvalidRequest, "exhausted"))],
        };
        Box::pin(Playback(events.into()))
    }
}

struct Playback(VecDeque<Result<Event, ModelError>>);

impl futures_core::Stream for Playback {
    type Item = Result<Event, ModelError>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        std::task::Poll::Ready(self.get_mut().0.pop_front())
    }
}

struct Fixture {
    dir: TempDir,
    fs: Arc<Parked>,
    sessions: Arc<SessionStore>,
    agents: Arc<dyn Agents>,
    projections: Arc<SessionProjections>,
    prompt: Arc<SystemPrompt>,
    hooks: Arc<RequestErrorHooks>,
    model: Arc<Scripted>,
    wd: Arc<WorkingDirectory>,
    _runtime: Runtime,
}

/// Makes the service's config from the fixture's root.
type ConfigOf = Box<dyn FnOnce(&str) -> WorkingDirectoryConfig>;

#[derive(Default)]
struct Options {
    config: Option<ConfigOf>,
    include_runtime_context: bool,
    replies: Vec<Result<&'static str, ErrorKind>>,
}

async fn start(dir: TempDir, options: Options) -> Result<Fixture, String> {
    let fs = Arc::new(Parked {
        inner: LocalFileSystem::new(LocalFsConfig {
            cwd: dir.root(),
            ..LocalFsConfig::default()
        })
        .unwrap(),
        hold: Mutex::new(None),
        stats: AtomicUsize::new(0),
    });
    let config = match options.config {
        Some(config) => config(&dir.root()),
        None => WorkingDirectoryConfig::default(),
    };
    let runtime = Runtime::start(vec![
        Box::new(SessionsPlugin),
        Box::new(SessionProjectionsPlugin),
        Box::new(SystemPromptPlugin::new(SystemPromptConfig {
            include_harness_identity: false,
            include_runtime_context: options.include_runtime_context,
            ..SystemPromptConfig::default()
        })),
        Box::new(ToolsPlugin),
        Box::new(ModelsPlugin),
        Box::new(AgentLoopPlugin::new(LoopConfig::new(ROUTE, "mock-model"))),
        Box::new(FsPlugin(Arc::clone(&fs))),
        Box::new(WorkingDirectoryPlugin::new(config)),
    ])
    .await
    .map_err(|error| {
        let mut message = error.to_string();
        let mut source = std::error::Error::source(&error);
        while let Some(cause) = source {
            message = format!("{message}: {cause}");
            source = cause.source();
        }
        message
    })?;
    let model = Arc::new(Scripted {
        replies: Mutex::new(options.replies.into()),
        requests: Mutex::new(Vec::new()),
    });
    runtime
        .get::<ModelRegistry>()
        .register(ROUTE, Arc::clone(&model) as Arc<dyn Model>)
        .unwrap();
    Ok(Fixture {
        fs,
        sessions: runtime.get::<SessionStore>(),
        agents: runtime.get::<dyn Agents>(),
        projections: runtime.get::<SessionProjections>(),
        prompt: runtime.get::<SystemPrompt>(),
        hooks: runtime.get::<RequestErrorHooks>(),
        wd: runtime.get::<WorkingDirectory>(),
        model,
        dir,
        _runtime: runtime,
    })
}

async fn fixture() -> Fixture {
    start(TempDir::new(), Options::default()).await.unwrap()
}

impl Fixture {
    fn root(&self) -> String {
        self.dir.root()
    }

    fn mkdir(&self, relative: &str) -> String {
        std::fs::create_dir_all(self.dir.0.join(relative)).unwrap();
        self.dir.at(relative)
    }

    /// The agent of a session created in `cwd`, or with no origin.
    fn agent_in(&self, id: &str, cwd: Option<&str>) -> Arc<dyn Agent> {
        let mut header = SessionHeader::new(SessionId::new(id), 1);
        header.cwd = cwd.map(str::to_owned);
        self.sessions.create_with(header).unwrap();
        self.agents.open(SessionId::new(id)).unwrap()
    }

    fn agent(&self) -> Arc<dyn Agent> {
        match self.agents_opened() {
            Some(agent) => agent,
            None => self.agent_in("directory-owner", Some(&self.root())),
        }
    }

    fn agents_opened(&self) -> Option<Arc<dyn Agent>> {
        self.sessions
            .get(&SessionId::new("directory-owner"))
            .map(|_| self.agents.open(SessionId::new("directory-owner")).unwrap())
    }

    fn session(&self, agent: &dyn Agent) -> Session {
        self.sessions.get(agent.id()).unwrap()
    }

    /// The directories each `working-directory/change` recorded.
    fn changes(&self, agent: &dyn Agent) -> Vec<String> {
        self.session(agent)
            .events()
            .iter()
            .filter(|entry| entry.event.event_type() == "working-directory/change")
            .map(|entry| {
                serde_json::to_value(entry).unwrap()["data"]["cwd"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect()
    }

    /// The notices waiting in next-step, taken: an idle agent's cancel only
    /// clears its inbox.
    fn take_notices(&self, agent: &dyn Agent) -> Vec<String> {
        let session = self.session(agent);
        let state = self
            .projections
            .state_of::<InboxProjection>(&session)
            .unwrap();
        let texts = state.next_step.iter().map(text_of).collect();
        agent.cancel(CancelCause::User);
        texts
    }

    async fn assemble(&self, agent: &dyn Agent) -> PromptAssembly {
        self.prompt
            .assemble(&AssembleContext::scoped(Scope::new(agent.id().as_str())))
            .await
            .unwrap()
    }
}

fn text_of(message: &UserMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect()
}

fn quoted(path: &str) -> String {
    serde_json::to_string(path).unwrap()
}

fn current(path: &str) -> String {
    format!("Current working directory: {}.", quoted(path))
}

fn snapshot(assembly: &PromptAssembly) -> String {
    render_context_snapshot(assembly).unwrap()
}

// "keeps origin metadata and other Sessions independent while logging the
// changed directory"
#[tokio::test]
async fn a_change_is_logged_noticed_and_kept_to_its_session() {
    let f = fixture().await;
    let child = f.mkdir("child");
    let agent = f.agent();
    let sibling = f.agent_in("sibling", Some(&f.root()));
    assert_eq!(
        f.wd.set(agent.as_ref(), "child", None).await.unwrap(),
        child
    );
    assert_eq!(f.wd.get(&f.session(agent.as_ref())), child);
    assert_eq!(f.wd.get(&f.session(sibling.as_ref())), f.root());
    assert_eq!(
        f.session(agent.as_ref()).header().cwd.as_deref(),
        Some(f.root().as_str())
    );
    assert_eq!(f.changes(agent.as_ref()), std::slice::from_ref(&child));
    assert_eq!(
        f.take_notices(agent.as_ref()),
        [format!(
            "The working directory changed from {} to {}.",
            quoted(&f.root()),
            quoted(&child)
        )]
    );
    // The notice is a `working-directory` message.
    let splice = f
        .session(agent.as_ref())
        .events()
        .into_iter()
        .find_map(|entry| match entry.event {
            SessionEvent::InboxSpliced(splice) => Some(splice),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        splice.inserted[0].source,
        UserSource::Other(json!({ "kind": "working-directory" }))
    );
}

// "restores directory state from the log"
#[tokio::test]
async fn the_directory_is_restored_from_the_log() {
    let f = fixture().await;
    let child = f.mkdir("child");
    let agent = f.agent();
    f.wd.set(agent.as_ref(), &child, None).await.unwrap();
    let restored = f
        .sessions
        .restore_with(
            SessionHeader::new(SessionId::new("restored"), 1).with_cwd(f.root()),
            0,
            f.session(agent.as_ref()).events(),
        )
        .unwrap();
    assert_eq!(f.wd.get(&restored), child);
}

// "exposes the current directory in wire observations after replay without
// an Agent"
#[tokio::test]
async fn clients_see_the_directory_without_an_agent() {
    let f = fixture().await;
    let moved = f.mkdir("moved");
    let without_origin = f
        .sessions
        .create(SessionId::new("wire-without-origin"))
        .unwrap();
    let view = |session: &Session| {
        f.projections.snapshot(session, None).values["workingDirectory"].clone()
    };
    assert_eq!(view(&without_origin), Value::Null);
    let agent = f.agent();
    assert_eq!(view(&f.session(agent.as_ref())), json!(f.root()));
    f.wd.set(agent.as_ref(), &moved, None).await.unwrap();
    let restored = f
        .sessions
        .restore_with(
            SessionHeader::new(SessionId::new("wire-restored"), 1).with_cwd(f.root()),
            0,
            f.session(agent.as_ref()).events(),
        )
        .unwrap();
    assert_eq!(view(&restored), json!(moved));
    assert_eq!(restored.header().cwd.as_deref(), Some(f.root().as_str()));
}

// "serializes concurrent relative changes and recovers after a rejected
// change"
#[tokio::test]
async fn concurrent_relative_changes_run_in_turn() {
    let f = fixture().await;
    f.mkdir("a/b");
    let agent = f.agent();
    let (a, b) = tokio::join!(
        f.wd.set(agent.as_ref(), "a", None),
        f.wd.set(agent.as_ref(), "b", None),
    );
    assert_eq!((a.unwrap(), b.unwrap()), (f.dir.at("a"), f.dir.at("a/b")));
    let missing = f.wd.set(agent.as_ref(), "missing", None).await.unwrap_err();
    assert_eq!(
        missing.to_string(),
        "working-directory: directory does not exist: \"missing\""
    );
    assert_eq!(
        f.wd.set(agent.as_ref(), "..", None).await.unwrap(),
        f.dir.at("a")
    );
}

// "returns to the original project when the selected directory disappears
// and injects the recovery"
#[tokio::test]
async fn a_vanished_directory_returns_to_the_project() {
    let f = fixture().await;
    let child = f.mkdir("temporary");
    let agent = f.agent();
    f.wd.set(agent.as_ref(), &child, None).await.unwrap();
    f.take_notices(agent.as_ref());
    std::fs::remove_dir(&child).unwrap();
    assert_eq!(f.wd.ensure(agent.as_ref(), None).await.unwrap(), f.root());
    assert_eq!(
        f.take_notices(agent.as_ref()),
        [format!(
            "The working directory {} is unavailable. The working directory is now {}.",
            quoted(&child),
            quoted(&f.root())
        )]
    );
}

// "reports a missing original project and permits an explicit directory
// change to recover"
#[tokio::test]
async fn a_missing_project_is_reported_and_a_change_recovers() {
    let f = fixture().await;
    let origin = f.mkdir("original");
    let agent = f.agent_in("removed-origin", Some(&origin));
    std::fs::remove_dir(&origin).unwrap();
    let error = f.wd.ensure(agent.as_ref(), None).await.unwrap_err();
    assert!(
        error.to_string().contains("directory does not exist"),
        "{error}"
    );
    assert_eq!(
        f.wd.set(agent.as_ref(), &f.root(), None).await.unwrap(),
        f.root()
    );
}

// "rejects files, empty paths, and cancelled changes without changing
// state"
#[tokio::test]
async fn files_empty_paths_and_cancels_change_nothing() {
    let f = fixture().await;
    std::fs::write(f.dir.0.join("file"), "").unwrap();
    let agent = f.agent();
    let file = f.wd.set(agent.as_ref(), "file", None).await.unwrap_err();
    assert!(file.to_string().contains("directory does not exist"));
    let empty = f.wd.set(agent.as_ref(), "", None).await.unwrap_err();
    assert_eq!(empty, WorkingDirectoryError::EmptyPath);
    assert_eq!(empty.to_string(), "working-directory: cd must not be empty");
    let cancelled =
        f.wd.set(agent.as_ref(), &f.root(), Some(&AbortSignal::aborted()))
            .await
            .unwrap_err();
    assert_eq!(cancelled, WorkingDirectoryError::Cancelled);
    assert_eq!(f.wd.get(&f.session(agent.as_ref())), f.root());
    assert!(f.changes(agent.as_ref()).is_empty());
}

// "keeps unchanged reads and changes free of transition notices". The
// session's project is its state already, so nothing is logged either.
#[tokio::test]
async fn an_unchanged_directory_gets_no_notice() {
    let f = fixture().await;
    let agent = f.agent();
    f.wd.ensure(agent.as_ref(), None).await.unwrap();
    f.wd.set(agent.as_ref(), &f.root(), None).await.unwrap();
    assert!(f.take_notices(agent.as_ref()).is_empty());
    assert!(f.changes(agent.as_ref()).is_empty());
}

// "provides required user context instead of a system-prompt directory,
// including recovery during assembly"
#[tokio::test]
async fn the_directory_is_required_context_and_assembly_recovers_it() {
    let f = fixture().await;
    let child = f.mkdir("temporary");
    let agent = f.agent();
    f.wd.set(agent.as_ref(), &child, None).await.unwrap();
    std::fs::remove_dir(&child).unwrap();
    let assembly = f.assemble(agent.as_ref()).await;
    assert!(snapshot(&assembly).contains(&current(&f.root())));
    assert!(!render_prompt(&assembly).unwrap().contains(&f.root()));
    let global = f
        .prompt
        .assemble(&AssembleContext::default())
        .await
        .unwrap();
    assert_eq!(snapshot(&global), "");
}

// "uses and records the configured launch fallback for a Session without
// origin metadata"
#[tokio::test]
async fn a_session_without_an_origin_uses_and_records_the_launch_directory() {
    let f = fixture().await;
    let agent = f.agent_in("without-origin", None);
    let fallback = f.wd.default_directory().to_owned();
    assert_eq!(fallback, std::env::current_dir().unwrap().to_str().unwrap());
    assert_eq!(f.wd.ensure(agent.as_ref(), None).await.unwrap(), fallback);
    assert_eq!(f.changes(agent.as_ref()), [fallback]);
}

// "rejects a relative deployment default before accepting Sessions"
#[tokio::test]
async fn a_relative_default_is_refused() {
    let error = start(
        TempDir::new(),
        Options {
            config: Some(Box::new(|_| WorkingDirectoryConfig {
                default_directory: Some("relative/project".into()),
            })),
            ..Options::default()
        },
    )
    .await
    .err()
    .expect("the plugin refuses it");
    assert!(
        error.contains("working-directory: defaultDirectory must be absolute"),
        "{error}"
    );
}

// "recovers a Session without origin metadata to its configured default"
#[tokio::test]
async fn a_session_without_an_origin_recovers_to_the_configured_default() {
    let dir = TempDir::new();
    let f = start(
        dir,
        Options {
            config: Some(Box::new(|root| WorkingDirectoryConfig {
                default_directory: Some(root.to_owned()),
            })),
            ..Options::default()
        },
    )
    .await
    .unwrap();
    let agent = f.agent_in("fallback-recovery", None);
    let temporary = f.mkdir("temporary");
    f.wd.set(agent.as_ref(), &temporary, None).await.unwrap();
    f.take_notices(agent.as_ref());
    std::fs::remove_dir(&temporary).unwrap();
    assert_eq!(f.wd.ensure(agent.as_ref(), None).await.unwrap(), f.root());
    assert_eq!(f.session(agent.as_ref()).header().cwd, None);
    assert_eq!(
        f.take_notices(agent.as_ref()),
        [format!(
            "The working directory {} is unavailable. The working directory is now {}.",
            quoted(&temporary),
            quoted(&f.root())
        )]
    );
}

/// After the rest of the chain, empties or rewrites the contexts.
struct Rewrite {
    remove: bool,
}

impl AssembleListener for Rewrite {
    fn assemble<'a>(
        &'a self,
        assembly: PromptAssembly,
        _context: &'a AssembleContext,
        next: AssembleNext<'a>,
    ) -> AssembleFuture<'a> {
        Box::pin(async move {
            let mut assembly = next.run(assembly).await?;
            assembly.contexts = if self.remove {
                Vec::new()
            } else {
                vec![AssembledContext::new(
                    "working-directory:current",
                    "outdated directory",
                )]
            };
            Ok(assembly)
        })
    }
}

// "retains required current-directory context after an assembly transform
// can remove it" and "... replace it"
#[tokio::test]
async fn the_context_survives_a_later_listener_removing_or_replacing_it() {
    for remove in [true, false] {
        let f = fixture().await;
        let _ = f.prompt.on_assemble(None, Arc::new(Rewrite { remove }));
        let agent = f.agent();
        let assembly = f.assemble(agent.as_ref()).await;
        assert_eq!(
            assembly.contexts,
            [AssembledContext {
                name: "working-directory:current".into(),
                text: current(&f.root()),
                interpolate: false,
            }]
        );
        assert!(!render_prompt(&assembly).unwrap().contains(&f.root()));
    }
}

// "renders variable-looking directory names literally after context
// removal is false" and "... is true"
#[tokio::test]
async fn a_directory_name_like_a_variable_is_shown_as_it_is() {
    for remove in [false, true] {
        let f = fixture().await;
        let directory = f.mkdir("{{project}}-{{unknown}}-{{malformed-name}}");
        let _ = f
            .prompt
            .variable(None, "project", |_| Some("wrong-directory".into()))
            .unwrap();
        if remove {
            let _ = f
                .prompt
                .on_assemble(None, Arc::new(Rewrite { remove: true }));
        }
        let agent = f.agent();
        f.wd.set(agent.as_ref(), &directory, None).await.unwrap();
        let assembly = f.assemble(agent.as_ref()).await;
        assert!(snapshot(&assembly).contains(&current(&directory)));
        assert!(!snapshot(&assembly).contains("wrong-directory"));
    }
}

// "does not commit a cancelled filesystem lookup and lets the next queued
// change proceed"
#[tokio::test(flavor = "multi_thread")]
async fn a_cancelled_lookup_commits_nothing_and_the_next_change_runs() {
    let f = Arc::new(fixture().await);
    f.mkdir("first");
    f.mkdir("second");
    let agent = f.agent();
    let (entered, release) = f.fs.hold_next();
    let controller = AbortController::new();
    let signal = controller.signal();
    let cancelled = {
        let (f, agent) = (Arc::clone(&f), Arc::clone(&agent));
        tokio::spawn(async move { f.wd.set(agent.as_ref(), "first", Some(&signal)).await })
    };
    entered.notified().await;
    let queued = {
        let (f, agent) = (Arc::clone(&f), Arc::clone(&agent));
        tokio::spawn(async move { f.wd.set(agent.as_ref(), "second", None).await })
    };
    controller.abort();
    assert_eq!(
        cancelled.await.unwrap().unwrap_err(),
        WorkingDirectoryError::Cancelled
    );
    release.notify_one();
    assert_eq!(queued.await.unwrap().unwrap(), f.dir.at("second"));
    assert_eq!(f.changes(agent.as_ref()), [f.dir.at("second")]);
    assert_eq!(f.take_notices(agent.as_ref()).len(), 1);
}

// "aborts and drains pending ensure before disposal completes", and
// "... set ...": stopping the service abandons what is pending, commits
// nothing, and refuses what comes after.
#[tokio::test(flavor = "multi_thread")]
async fn stopping_abandons_pending_work_and_refuses_more() {
    for set in [false, true] {
        let f = Arc::new(fixture().await);
        let agent = f.agent();
        let (entered, release) = f.fs.hold_next();
        let pending = {
            let (f, agent) = (Arc::clone(&f), Arc::clone(&agent));
            tokio::spawn(async move {
                if set {
                    f.wd.set(agent.as_ref(), &f.root(), None).await
                } else {
                    f.wd.ensure(agent.as_ref(), None).await
                }
            })
        };
        entered.notified().await;
        tokio::time::timeout(Duration::from_secs(5), f.wd.stop())
            .await
            .expect("stopping drains");
        let error = pending.await.unwrap().unwrap_err();
        assert_eq!(error, WorkingDirectoryError::Disposed);
        assert_eq!(error.to_string(), "working-directory service disposed");
        release.notify_one();
        assert!(f.changes(agent.as_ref()).is_empty());
        assert!(f.take_notices(agent.as_ref()).is_empty());
        assert_eq!(
            f.wd.ensure(agent.as_ref(), None).await.unwrap_err(),
            WorkingDirectoryError::Disposed
        );
        assert_eq!(f.fs.stats.load(Ordering::SeqCst), 1);
    }
}

/// An agent whose inbox refuses everything.
struct Refusing {
    id: SessionId,
    frames: FrameListeners,
}

impl Agent for Refusing {
    fn id(&self) -> &SessionId {
        &self.id
    }

    fn followup(&self, _: UserMessage) -> Result<(), InboxError> {
        Err(InboxError::Unavailable("inbox unavailable".into()))
    }

    fn steer(&self, _: UserMessage) -> Result<(), InboxError> {
        Err(InboxError::Unavailable("inbox unavailable".into()))
    }

    fn inject(&self, _: UserMessage) -> Result<(), InboxError> {
        Err(InboxError::Unavailable("inbox unavailable".into()))
    }

    fn cancel(&self, _: CancelCause) {}

    fn when_idle(&self) -> WhenIdle<'_> {
        Box::pin(async {})
    }

    fn subscribe(&self, listener: FrameListener) -> FrameSubscription {
        self.frames.subscribe(listener)
    }
}

// "returns committed state and warns when the notice cannot be queued"
#[tokio::test]
async fn a_notice_that_cant_be_queued_is_warned_and_the_change_stands() {
    let f = fixture().await;
    let child = f.mkdir("child");
    let id = SessionId::new("refusing");
    f.sessions
        .create_with(SessionHeader::new(id.clone(), 1).with_cwd(f.root()))
        .unwrap();
    let agent = Refusing {
        id: id.clone(),
        frames: FrameListeners::new(),
    };
    let warnings = Arc::new(Mutex::new(Vec::new()));
    let heard = Arc::clone(&warnings);
    let wd = WorkingDirectory::new(
        WorkingDirectoryConfig::default(),
        Arc::clone(&f.fs) as Arc<dyn FileSystem>,
        Arc::clone(&f.projections),
        Arc::clone(&f.sessions),
    )
    .unwrap()
    .with_warnings(move |warning| heard.lock().unwrap().push(warning.to_owned()));
    assert_eq!(wd.set(&agent, &child, None).await.unwrap(), child);
    let session = f.sessions.get(&id).unwrap();
    assert_eq!(wd.get(&session), child);
    assert_eq!(
        *warnings.lock().unwrap(),
        [format!(
            "working-directory: committed {}, but could not queue its notice: inbox unavailable",
            quoted(&child)
        )]
    );
    // A session restored from that log shows the committed directory.
    let restored = f
        .sessions
        .restore_with(
            SessionHeader::new(SessionId::new("notice-failure-restored"), 1).with_cwd(f.root()),
            0,
            session.events(),
        )
        .unwrap();
    let assembly = f
        .prompt
        .assemble(&AssembleContext::scoped(Scope::new(restored.id().as_str())))
        .await
        .unwrap();
    assert!(snapshot(&assembly).contains(&current(&child)));
}

/// Changes the directory when the first request fails, then asks for the
/// request again.
struct ChangeThenRetry {
    wd: Arc<WorkingDirectory>,
    agents: Arc<dyn Agents>,
    to: String,
    done: AtomicUsize,
}

impl RequestErrorHandler for ChangeThenRetry {
    fn handle<'a>(&'a self, error: &'a RequestError, _next: Next<'a>) -> HookFuture<'a> {
        Box::pin(async move {
            if self.done.fetch_add(1, Ordering::SeqCst) > 0 {
                return Ok(None);
            }
            let agent = self.agents.open(error.session.id().clone())?;
            self.wd.set(agent.as_ref(), &self.to, None).await?;
            Ok(Some(RequestErrorAction::Retry))
        })
    }
}

// admission.spec: "admits current directory ... with optional context":
// a change made after the step's assembly, here between two attempts of
// its request, is what the next attempt shows.
#[tokio::test]
async fn a_request_shows_the_directory_as_it_is_when_it_is_sent() {
    for optional in [false, true] {
        let f = start(
            TempDir::new(),
            Options {
                include_runtime_context: optional,
                replies: vec![
                    Err(ErrorKind::RateLimited),
                    Ok("first reply"),
                    Ok("notice received"),
                ],
                ..Options::default()
            },
        )
        .await
        .unwrap();
        let selected = f.mkdir("selected");
        f.hooks.register(Arc::new(ChangeThenRetry {
            wd: Arc::clone(&f.wd),
            agents: Arc::clone(&f.agents),
            to: selected.clone(),
            done: AtomicUsize::new(0),
        }));
        let agent = f.agent();
        agent
            .followup(UserMessage::text(
                "prompt",
                UserSource::user(),
                "Read the active project.",
            ))
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), agent.when_idle())
            .await
            .unwrap();

        let requests = f.model.requests.lock().unwrap().clone();
        assert!(requests.len() >= 2, "{} requests", requests.len());
        let latest = |request: &Request| {
            request
                .messages
                .iter()
                .rev()
                .map(karaka_model::Message::text)
                .find(|text| text.starts_with("Current runtime context."))
                .unwrap_or_default()
        };
        let retried = latest(&requests[1]);
        assert!(retried.contains(&current(&selected)), "{retried}");
        assert!(!retried.contains(&current(&f.root())), "{retried}");
        let session = f.session(agent.as_ref());
        assert_eq!(f.wd.get(&session), selected);
        assert_eq!(session.header().cwd.as_deref(), Some(f.root().as_str()));
        let prompts = session
            .events()
            .iter()
            .filter(|entry| {
                matches!(&entry.event, SessionEvent::UserMessage { message, .. }
                    if matches!(message.source, UserSource::User(_)))
            })
            .count();
        assert_eq!(prompts, 1);
        let ending = session
            .events()
            .into_iter()
            .rev()
            .find_map(|entry| match entry.event {
                SessionEvent::TurnEnd(end) => Some(end.reason),
                _ => None,
            });
        assert_eq!(ending, Some(TurnEndReason::Completed));
    }
}
