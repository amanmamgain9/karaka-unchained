//! The server: named routes, upgrade routes and one fallback, matched by
//! path, over one HTTP/1.1 listener. (dsh: `WebServer` in
//! `host/webserver/src/index.ts`.)

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock, Weak};
use std::time::Duration;

use http::header::{CONNECTION, UPGRADE};
use http::{Method, StatusCode};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::upgrade::{OnUpgrade, Upgraded};
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::{JoinHandle, JoinSet};

use crate::BoxError;
use crate::address::{bind_url_host, check_host, json_string, socket_address};
use crate::body::{Body, empty};

/// A request, its body unread.
pub type Request = http::Request<hyper::body::Incoming>;

/// A response.
pub type Response = http::Response<Body>;

/// What a handler resolves to. An error answers 400 with no body, as dsh's
/// per-request guard does, or cuts the connection once the body is under
/// way.
pub type HandlerFuture = Pin<Box<dyn Future<Output = Result<Response, BoxError>> + Send>>;

/// Owns a request's whole response, which may stay open, as an SSE
/// stream's does. (dsh: `WebRoute.handler`.)
pub type Handler = Arc<dyn Fn(Request) -> HandlerFuture + Send + Sync>;

/// Owns an upgrade's negotiation: it answers `101 Switching Protocols` and
/// hands the connection to [`Upgrades::adopt`], or answers anything else
/// to refuse. An error cuts the connection. (dsh: `WebUpgradeRoute.handler`.)
pub type UpgradeHandler = Arc<dyn Fn(Request, Upgrades) -> HandlerFuture + Send + Sync>;

/// Told each warning the server logs. (dsh: `ctx.logger.warn`.)
pub type Log = Arc<dyn Fn(&str) + Send + Sync>;

/// How a route's path matches. (dsh: `WebRouteKind`.)
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RouteKind {
    /// The pathname exactly.
    Exact,
    /// The path itself and anything under it, `p` and `p/…`.
    Prefix,
}

impl RouteKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::Prefix => "prefix",
        }
    }
}

/// One named route. (dsh: `WebRoute`.)
#[derive(Clone)]
pub struct WebRoute {
    pub kind: RouteKind,
    /// Absolute, with no trailing slash.
    pub path: String,
    pub handler: Handler,
}

impl WebRoute {
    /// A route whose handler is `handler`.
    pub fn new<F, Fut>(kind: RouteKind, path: impl Into<String>, handler: F) -> Self
    where
        F: Fn(Request) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Response, BoxError>> + Send + 'static,
    {
        Self {
            kind,
            path: path.into(),
            handler: Arc::new(move |request| Box::pin(handler(request))),
        }
    }
}

/// One exact-path upgrade route. (dsh: `WebUpgradeRoute`.)
#[derive(Clone)]
pub struct UpgradeRoute {
    pub path: String,
    pub handler: UpgradeHandler,
}

impl UpgradeRoute {
    pub fn new<F, Fut>(path: impl Into<String>, handler: F) -> Self
    where
        F: Fn(Request, Upgrades) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Response, BoxError>> + Send + 'static,
    {
        Self {
            path: path.into(),
            handler: Arc::new(move |request, upgrades| Box::pin(handler(request, upgrades))),
        }
    }
}

/// Where to listen. (dsh: `Config`'s `host` and `port`.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebServerConfig {
    host: String,
    port: u16,
}

impl WebServerConfig {
    /// `host` is one concrete IPv4 or IPv6 literal of a local interface,
    /// with an optional `%zone` on IPv6; `port` 0 asks the OS for one.
    ///
    /// # Errors
    ///
    /// A host that isn't a literal, or is the unspecified address, in dsh's
    /// words.
    pub fn new(host: impl Into<String>, port: u16) -> Result<Self, String> {
        let host = host.into();
        check_host(&host)?;
        Ok(Self { host, port })
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// The origin of a listener of this config bound on `port`. (dsh:
    /// `getOrigin`, which reads the bound port.)
    ///
    /// # Errors
    ///
    /// The host has a zone no URL can express, in dsh's words.
    pub fn origin_at(&self, port: u16) -> Result<String, String> {
        let host = bind_url_host(&self.host).ok_or_else(|| {
            format!(
                "webserver: bind address {} carries an interface zone id, which no URL can express",
                json_string(&self.host)
            )
        })?;
        Ok(format!("http://{host}:{port}"))
    }
}

#[derive(Default)]
struct Routes {
    exact: HashMap<String, (u64, Handler)>,
    prefixes: HashMap<String, (u64, Handler)>,
    upgrades: HashMap<String, (u64, UpgradeHandler)>,
    fallback: Option<(u64, Handler)>,
    next: u64,
}

struct Shared {
    routes: RwLock<Routes>,
    /// Every connection's task, and every adopted upgrade's.
    tasks: Mutex<JoinSet<()>>,
    /// Becomes `true` on close.
    stop: watch::Sender<bool>,
    log: Option<Log>,
}

impl Shared {
    fn routes(&self) -> std::sync::RwLockReadGuard<'_, Routes> {
        self.routes.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn routes_mut(&self) -> std::sync::RwLockWriteGuard<'_, Routes> {
        self.routes.write().unwrap_or_else(PoisonError::into_inner)
    }

    fn tasks(&self) -> MutexGuard<'_, JoinSet<()>> {
        self.tasks.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn warn(&self, message: &str) {
        if let Some(log) = &self.log {
            log(&format!("webserver: {message}"));
        }
    }

    /// Exact, then the longest prefix, then the fallback. (dsh: `match`.)
    fn find(&self, path: &str) -> Option<Handler> {
        let routes = self.routes();
        if let Some((_, handler)) = routes.exact.get(path) {
            return Some(Arc::clone(handler));
        }
        routes
            .prefixes
            .iter()
            .filter(|(prefix, _)| {
                path == prefix.as_str()
                    || path
                        .strip_prefix(prefix.as_str())
                        .is_some_and(|rest| rest.starts_with('/'))
            })
            .max_by_key(|(prefix, _)| prefix.len())
            .map(|(_, (_, handler))| Arc::clone(handler))
            .or_else(|| {
                routes
                    .fallback
                    .as_ref()
                    .map(|(_, handler)| Arc::clone(handler))
            })
    }

    async fn handle(self: Arc<Self>, request: Request) -> Result<Response, BoxError> {
        let path = pathname(request.uri());
        if wants_upgrade(&request) {
            let handler = self
                .routes()
                .upgrades
                .get(&path)
                .map(|(_, handler)| Arc::clone(handler));
            // An upgrade no route owns has its connection closed.
            let Some(handler) = handler else {
                return Err("no upgrade route".into());
            };
            let upgrades = Upgrades {
                shared: Arc::downgrade(&self),
            };
            return handler(request, upgrades).await.inspect_err(|error| {
                self.warn(&error.to_string());
            });
        }
        let Some(handler) = self.find(&path) else {
            return Ok(status(StatusCode::NOT_FOUND));
        };
        match handler(request).await {
            Ok(response) => Ok(response),
            Err(error) => {
                self.warn(&error.to_string());
                Ok(status(StatusCode::BAD_REQUEST))
            }
        }
    }
}

/// A response of `code` with no body. (dsh: `writeHead(code); end()`.)
fn status(code: StatusCode) -> Response {
    let mut response = Response::new(empty());
    *response.status_mut() = code;
    response
}

/// Whether the request asks to switch protocols, as `node:http` decides
/// to emit `upgrade`: an `Upgrade` header and `Connection: upgrade`.
fn wants_upgrade(request: &Request) -> bool {
    request.method() != Method::CONNECT
        && request.headers().contains_key(UPGRADE)
        && request
            .headers()
            .get_all(CONNECTION)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(','))
            .any(|token| token.trim().eq_ignore_ascii_case("upgrade"))
}

/// The request's pathname, as `new URL(req.url, 'http://x').pathname`
/// reads it: a `//host` start is an authority, and `.` and `..` segments,
/// `%2e` spellings included, are resolved.
pub fn pathname(uri: &http::Uri) -> String {
    let mut path = uri.path();
    if let Some(rest) = path.strip_prefix("//") {
        path = rest.find('/').map_or("/", |at| &rest[at..]);
    }
    let dot = |segment: &str| segment == "." || segment.eq_ignore_ascii_case("%2e");
    let dots = |segment: &str| {
        matches!(
            segment.to_ascii_lowercase().as_str(),
            ".." | ".%2e" | "%2e." | "%2e%2e"
        )
    };
    let segments: Vec<&str> = path.split('/').skip(1).collect();
    let mut kept: Vec<&str> = Vec::new();
    for (index, segment) in segments.iter().enumerate() {
        let last = index + 1 == segments.len();
        if dots(segment) {
            kept.pop();
            if last {
                kept.push("");
            }
        } else if dot(segment) {
            if last {
                kept.push("");
            }
        } else {
            kept.push(segment);
        }
    }
    format!("/{}", kept.join("/"))
}

/// Hands upgraded connections to the server, which closes them when it
/// closes. (dsh: `upgradedSockets`.)
#[derive(Clone)]
pub struct Upgrades {
    shared: Weak<Shared>,
}

impl Upgrades {
    /// Runs `serve` on the connection once `upgrade` completes, as a task
    /// the server aborts on close, which drops and so closes the
    /// connection. The response that switched protocols must be returned
    /// for the upgrade to complete.
    pub fn adopt<F, Fut>(&self, upgrade: OnUpgrade, serve: F)
    where
        F: FnOnce(TokioIo<Upgraded>) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let Some(shared) = self.shared.upgrade() else {
            return;
        };
        if *shared.stop.borrow() {
            return;
        }
        let warn = Arc::downgrade(&shared);
        shared.tasks().spawn(async move {
            match upgrade.await {
                Ok(upgraded) => serve(TokioIo::new(upgraded)).await,
                Err(error) => {
                    if let Some(shared) = warn.upgrade() {
                        shared.warn(&error.to_string());
                    }
                }
            }
        });
    }
}

/// A registered route, until removed.
#[must_use = "the route stays registered until removed"]
pub struct RouteRegistration {
    shared: Weak<Shared>,
    id: u64,
    slot: Slot,
}

enum Slot {
    Named(RouteKind, String),
    Upgrade(String),
    Fallback,
}

impl RouteRegistration {
    /// Takes the route out; its path may be registered again. (dsh: the
    /// disposer `register` returns.)
    pub fn remove(self) {
        let Some(shared) = self.shared.upgrade() else {
            return;
        };
        let mut routes = shared.routes_mut();
        let id = self.id;
        match self.slot {
            Slot::Named(RouteKind::Exact, path) => {
                if routes
                    .exact
                    .get(&path)
                    .is_some_and(|(known, _)| *known == id)
                {
                    routes.exact.remove(&path);
                }
            }
            Slot::Named(RouteKind::Prefix, path) => {
                if routes
                    .prefixes
                    .get(&path)
                    .is_some_and(|(known, _)| *known == id)
                {
                    routes.prefixes.remove(&path);
                }
            }
            Slot::Upgrade(path) => {
                if routes
                    .upgrades
                    .get(&path)
                    .is_some_and(|(known, _)| *known == id)
                {
                    routes.upgrades.remove(&path);
                }
            }
            Slot::Fallback => {
                if routes
                    .fallback
                    .as_ref()
                    .is_some_and(|(known, _)| *known == id)
                {
                    routes.fallback = None;
                }
            }
        }
    }
}

impl std::fmt::Debug for RouteRegistration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RouteRegistration")
            .field("id", &self.id)
            .finish()
    }
}

/// The listening server: the port the routes are mounted on. (dsh: the
/// `webServer` service.)
pub struct WebServer {
    config: WebServerConfig,
    port: u16,
    shared: Arc<Shared>,
    accepting: Mutex<Option<JoinHandle<()>>>,
}

/// How long a connection may take to send a request's headers, as
/// `node:http`'s `headersTimeout` default.
const HEADERS_TIMEOUT: Duration = Duration::from_secs(60);

impl WebServer {
    /// Binds the listener and starts serving: everything 404 until routes
    /// are registered. Call it in a Tokio runtime.
    ///
    /// # Errors
    ///
    /// The address can't be bound, such as a port in use.
    pub async fn listen(config: WebServerConfig, log: Option<Log>) -> Result<Self, String> {
        let address = socket_address(&config.host, config.port)?;
        let listener = TcpListener::bind(address)
            .await
            .map_err(|error| format!("webserver: listen {address}: {error}"))?;
        let port = listener
            .local_addr()
            .map_err(|error| format!("webserver: listen {address}: {error}"))?
            .port();
        let shared = Arc::new(Shared {
            routes: RwLock::default(),
            tasks: Mutex::new(JoinSet::new()),
            stop: watch::channel(false).0,
            log,
        });
        let accepting = tokio::spawn(accept(listener, Arc::clone(&shared)));
        Ok(Self {
            config,
            port,
            shared,
            accepting: Mutex::new(Some(accepting)),
        })
    }

    /// The bound port, the OS's choice when the config asked for 0.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The configured bind address.
    pub fn host(&self) -> &str {
        &self.config.host
    }

    /// Always `http:`: TLS isn't ported. (dsh: `protocol`.)
    pub fn protocol(&self) -> &'static str {
        "http:"
    }

    /// The root URL local clients reach the listener at, such as
    /// `http://127.0.0.1:8787`. (dsh: `getOrigin`.)
    ///
    /// # Errors
    ///
    /// The bind address has a zone no URL can express, in dsh's words.
    pub fn origin(&self) -> Result<String, String> {
        self.config.origin_at(self.port)
    }

    fn registration(&self, id: u64, slot: Slot) -> RouteRegistration {
        RouteRegistration {
            shared: Arc::downgrade(&self.shared),
            id,
            slot,
        }
    }

    /// Registers a named route. (dsh: `register`.)
    ///
    /// # Errors
    ///
    /// The kind and path are taken, in dsh's words.
    pub fn register(&self, route: WebRoute) -> Result<RouteRegistration, String> {
        let mut routes = self.shared.routes_mut();
        routes.next += 1;
        let id = routes.next;
        let table = match route.kind {
            RouteKind::Exact => &mut routes.exact,
            RouteKind::Prefix => &mut routes.prefixes,
        };
        if table.contains_key(&route.path) {
            return Err(format!(
                "webserver: duplicate {} route \"{}\"",
                route.kind.as_str(),
                route.path
            ));
        }
        table.insert(route.path.clone(), (id, route.handler));
        drop(routes);
        Ok(self.registration(id, Slot::Named(route.kind, route.path)))
    }

    /// Registers an exact-path upgrade route. (dsh: `registerUpgrade`.)
    ///
    /// # Errors
    ///
    /// The path is taken, in dsh's words.
    pub fn register_upgrade(&self, route: UpgradeRoute) -> Result<RouteRegistration, String> {
        let mut routes = self.shared.routes_mut();
        if routes.upgrades.contains_key(&route.path) {
            return Err(format!(
                "webserver: duplicate upgrade route \"{}\"",
                route.path
            ));
        }
        routes.next += 1;
        let id = routes.next;
        routes
            .upgrades
            .insert(route.path.clone(), (id, route.handler));
        drop(routes);
        Ok(self.registration(id, Slot::Upgrade(route.path)))
    }

    /// Claims the one handler for every request no named route matches.
    /// (dsh: `registerFallback`.)
    ///
    /// # Errors
    ///
    /// The seat is taken, in dsh's words.
    pub fn register_fallback<F, Fut>(&self, handler: F) -> Result<RouteRegistration, String>
    where
        F: Fn(Request) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Response, BoxError>> + Send + 'static,
    {
        let mut routes = self.shared.routes_mut();
        if routes.fallback.is_some() {
            return Err("webserver: fallback already registered".into());
        }
        routes.next += 1;
        let id = routes.next;
        let handler: Handler = Arc::new(move |request| Box::pin(handler(request)));
        routes.fallback = Some((id, handler));
        drop(routes);
        Ok(self.registration(id, Slot::Fallback))
    }

    /// Stops listening, and closes every connection, upgraded ones
    /// included, before it resolves, as dsh's disposal does. A response in
    /// flight is cut, as `closeAllConnections` cuts it.
    pub async fn close(&self) {
        self.shared.stop.send_replace(true);
        let accepting = self
            .accepting
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(accepting) = accepting {
            let _ = accepting.await;
        }
        let mut tasks = std::mem::take(&mut *self.shared.tasks());
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
    }
}

impl std::fmt::Debug for WebServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebServer")
            .field("host", &self.config.host)
            .field("port", &self.port)
            .finish_non_exhaustive()
    }
}

/// Accepts until the server closes, each connection a task of its own.
async fn accept(listener: TcpListener, shared: Arc<Shared>) {
    let mut stop = shared.stop.subscribe();
    loop {
        let accepted = tokio::select! {
            accepted = listener.accept() => accepted,
            _ = stop.wait_for(|stop| *stop) => return,
        };
        let stream = match accepted {
            Ok((stream, _)) => stream,
            Err(error) => {
                shared.warn(&error.to_string());
                continue;
            }
        };
        let serving = Arc::clone(&shared);
        let mut closed = shared.stop.subscribe();
        let mut tasks = shared.tasks();
        // Finished connections are reaped as new ones come.
        while tasks.try_join_next().is_some() {}
        tasks.spawn(async move {
            let service = service_fn(move |request| Arc::clone(&serving).handle(request));
            let connection = http1::Builder::new()
                .timer(TokioTimer::new())
                .header_read_timeout(HEADERS_TIMEOUT)
                .serve_connection(TokioIo::new(stream), service)
                .with_upgrades();
            tokio::select! {
                _ = connection => {}
                _ = closed.wait_for(|stop| *stop) => {}
            }
        });
    }
}
