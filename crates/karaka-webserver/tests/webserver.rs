//! The webserver on a real loopback listener. (dsh:
//! `host/webserver/tests/webserver.spec.ts`.)
//!
//! Not ported: the gzip, TLS and index-injection cases, whose features
//! aren't ported (open ends); "reports no origin before binding", since a
//! `WebServer` exists only once bound; and the Loader's failed-fiber
//! cases, which are the plugin's failed start here.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use http::StatusCode;
use karaka_webserver::{
    RouteKind, UpgradeRoute, WebRoute, WebServer, WebServerConfig, bind_url_host, body, check_host,
    is_loopback_host, normalize_bind_address, on_upgrade, pathname,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

fn text(status: StatusCode, body: &'static str) -> karaka_webserver::Response {
    let mut response = karaka_webserver::Response::new(body::full(body));
    *response.status_mut() = status;
    response
}

fn route(kind: RouteKind, path: &str, answer: &'static str) -> WebRoute {
    WebRoute::new(kind, path, move |_| async move {
        Ok(text(StatusCode::OK, answer))
    })
}

async fn server() -> WebServer {
    WebServer::listen(WebServerConfig::new("127.0.0.1", 0).unwrap(), None)
        .await
        .unwrap()
}

/// One request on a connection of its own: the status and the body.
async fn request(port: u16, method: &str, path: &str) -> std::io::Result<(u16, String)> {
    let mut socket = TcpStream::connect(("127.0.0.1", port)).await?;
    socket
        .write_all(
            format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await?;
    let mut raw = Vec::new();
    socket.read_to_end(&mut raw).await?;
    let raw = String::from_utf8_lossy(&raw).to_string();
    let (head, body) = raw
        .split_once("\r\n\r\n")
        .ok_or_else(|| std::io::Error::other(format!("no response: {raw:?}")))?;
    let status = head
        .split(' ')
        .nth(1)
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| std::io::Error::other("no status"))?;
    let body = if head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        dechunk(body)
    } else {
        body.to_owned()
    };
    Ok((status, body))
}

fn dechunk(mut body: &str) -> String {
    let mut out = String::new();
    while let Some((size, rest)) = body.split_once("\r\n") {
        let size = usize::from_str_radix(size.trim(), 16).unwrap_or(0);
        if size == 0 {
            break;
        }
        out.push_str(&rest[..size]);
        body = &rest[size + 2..];
    }
    out
}

/// Sends an upgrade request, and reads the answer's head.
async fn upgrade(port: u16, path: &str) -> (TcpStream, String) {
    let mut socket = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    socket
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: Upgrade\r\nUpgrade: dsh-test\r\n\r\n")
                .as_bytes(),
        )
        .await
        .unwrap();
    let mut head = Vec::new();
    let mut byte = [0; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if socket.read(&mut byte).await.unwrap() == 0 {
            break;
        }
        head.push(byte[0]);
    }
    (socket, String::from_utf8_lossy(&head).to_string())
}

#[test]
fn a_concrete_literal_is_accepted_with_an_ipv6_zone() {
    for host in [
        "127.0.0.1",
        "10.1.2.3",
        "0.1.2.3",
        "::1",
        "fd00::1",
        "::ffff:10.1.2.3",
        "::ffff:0.1.2.3",
        "0:0:0:0:0:0:0:1",
        "fe80::1%lo",
        "::0.0.0.1",
        "::0.0.0.1%lo",
        "::127.0.0.1",
    ] {
        assert_eq!(WebServerConfig::new(host, 0).unwrap().host(), host);
    }
}

#[test]
fn every_wildcard_spelling_is_refused_by_its_value() {
    for host in [
        "0.0.0.0",
        "::",
        "::0",
        "0000::",
        "0::",
        "0:0:0:0:0:0:0:0",
        "::0.0.0.0",
        "::0.0.0.0%lo",
        "::%lo",
        "::ffff:0.0.0.0",
        "::ffff:0:0",
        "::ffff:0000:0000",
        "0:0:0:0:0:ffff:0:0",
        "0:0:0:0:0:ffff:0.0.0.0",
        "::ffff:0.0.0.0%eth0",
    ] {
        assert_eq!(
            check_host(host).unwrap_err(),
            format!(
                "webserver: host \"{host}\" is an unspecified (wildcard) address, which is not supported: binding every interface would expose remote code execution to the network; bind one concrete IPv4 or IPv6 address of a local interface instead"
            )
        );
    }
    for host in [
        "localhost",
        "example.com",
        "*",
        "[::]",
        "[fd00::1]",
        "10.1.2.3/8",
    ] {
        assert_eq!(
            check_host(host).unwrap_err(),
            format!("webserver: host \"{host}\" is not a concrete IPv4 or IPv6 address literal")
        );
    }
}

#[test]
fn loopback_is_read_from_the_parsed_value() {
    for host in [
        "127.0.0.1",
        "127.8.9.10",
        "127.5.5.5",
        "::1",
        "0:0:0:0:0:0:0:1",
        "::ffff:127.0.0.1",
        "::ffff:7f00:1",
        "::1%lo",
        "::0.0.0.1",
        "::0.0.0.1%lo",
    ] {
        assert!(is_loopback_host(host), "{host}");
    }
    for host in [
        "10.1.2.3",
        "::ffff:10.1.2.3",
        "fd00::1",
        "fe80::1%lo",
        "::2",
        "localhost",
        "::127.0.0.1",
        "::127.0.0.1%lo",
        "::0.0.0.2",
    ] {
        assert!(!is_loopback_host(host), "{host}");
    }
}

#[test]
fn a_dotted_quad_tail_reads_as_the_address_it_names() {
    for (host, text) in [
        ("::0.0.0.1", "::1"),
        ("::0.0.0.1%lo", "::1"),
        ("::127.0.0.1", "::7f00:1"),
        ("::1", "::1"),
        ("10.1.2.3", "10.1.2.3"),
        ("::ffff:127.0.0.1", "127.0.0.1"),
        ("::ffff:7f00:1", "127.0.0.1"),
        ("::ffff:10.1.2.3", "10.1.2.3"),
    ] {
        assert_eq!(normalize_bind_address(host).unwrap(), text, "{host}");
    }
    assert_eq!(
        normalize_bind_address("localhost").unwrap_err(),
        "webserver: host \"localhost\" is not a concrete IPv4 or IPv6 address literal"
    );
}

#[test]
fn bind_addresses_are_written_as_url_hosts() {
    for (host, text) in [
        ("127.0.0.1", Some("127.0.0.1")),
        ("::ffff:127.0.0.1%lo", Some("127.0.0.1")),
        ("::1", Some("[::1]")),
        ("::0.0.0.1%lo", Some("[::1]")),
        ("10.1.2.3", Some("10.1.2.3")),
        ("fd00::1", Some("[fd00::1]")),
        ("fe80::1%eth0", None),
        ("::127.0.0.1%lo", None),
    ] {
        assert_eq!(bind_url_host(host).as_deref(), text, "{host}");
    }
    assert_eq!(
        WebServerConfig::new("fe80::1%eth0", 0)
            .unwrap()
            .origin_at(1)
            .unwrap_err(),
        "webserver: bind address \"fe80::1%eth0\" carries an interface zone id, which no URL can express"
    );
}

#[test]
fn the_pathname_is_read_as_a_url_reads_it() {
    for (target, path) in [
        ("/probe", "/probe"),
        ("/api/../admin", "/admin"),
        ("/api/./x", "/api/x"),
        ("/api/%2e%2E/x", "/x"),
        ("/a/b/..", "/a/"),
        ("/a/.", "/a/"),
        ("/..", "/"),
        ("//host/path?q=1", "/path"),
        ("//host", "/"),
        ("/events?stream=mux", "/events"),
        ("http://example.test/full", "/full"),
    ] {
        assert_eq!(pathname(&target.parse().unwrap()), path, "{target}");
    }
}

#[tokio::test]
async fn routes_the_fallback_and_errors_are_dsh_s() {
    let server = server().await;
    let port = server.port();
    assert!(port > 0);
    assert_eq!(server.origin().unwrap(), format!("http://127.0.0.1:{port}"));
    assert_eq!(server.protocol(), "http:");

    // Exact beats prefix, the longest prefix wins, a prefix answers its own
    // path, and a route owns its methods.
    let _probe = server
        .register(route(RouteKind::Exact, "/probe", "EXACT"))
        .unwrap();
    let _api = server
        .register(route(RouteKind::Prefix, "/api", "API"))
        .unwrap();
    let _deep = server
        .register(route(RouteKind::Prefix, "/api/deep", "DEEP"))
        .unwrap();
    assert_eq!(
        request(port, "GET", "/probe").await.unwrap(),
        (200, "EXACT".into())
    );
    assert_eq!(
        request(port, "GET", "/api/anything").await.unwrap(),
        (200, "API".into())
    );
    assert_eq!(
        request(port, "GET", "/api/deep/leaf").await.unwrap(),
        (200, "DEEP".into())
    );
    assert_eq!(
        request(port, "GET", "/api").await.unwrap(),
        (200, "API".into())
    );
    assert_eq!(
        request(port, "GET", "/apis").await.unwrap(),
        (404, String::new())
    );
    assert_eq!(
        request(port, "POST", "/api/anything").await.unwrap(),
        (200, "API".into())
    );

    // The seat: 404 while unclaimed, then the owner answers the rest; one
    // owner only, and releasing it restores the 404.
    assert_eq!(
        request(port, "GET", "/no/such/route").await.unwrap(),
        (404, String::new())
    );
    let fallback = server
        .register_fallback(|request: karaka_webserver::Request| async move {
            // A malformed escape fails, as dsh's decodeURIComponent throws.
            if request.uri().path().contains("%zz") {
                return Err("URIError: URI malformed".into());
            }
            Ok(text(StatusCode::OK, "shell"))
        })
        .unwrap();
    assert_eq!(
        server
            .register_fallback(|_| async { Ok(text(StatusCode::OK, "")) })
            .unwrap_err(),
        "webserver: fallback already registered"
    );
    assert_eq!(
        request(port, "GET", "/no/such/route").await.unwrap(),
        (200, "shell".into())
    );

    // A failed handler answers 400, and the server carries on.
    assert_eq!(
        request(port, "GET", "/%zz").await.unwrap(),
        (400, String::new())
    );
    assert_eq!(
        request(port, "GET", "/probe").await.unwrap(),
        (200, "EXACT".into())
    );

    // A duplicate is refused; removal frees the path.
    assert_eq!(
        server
            .register(route(RouteKind::Exact, "/probe", ""))
            .unwrap_err(),
        "webserver: duplicate exact route \"/probe\""
    );
    assert_eq!(
        server
            .register(route(RouteKind::Prefix, "/api", ""))
            .unwrap_err(),
        "webserver: duplicate prefix route \"/api\""
    );
    let once = server
        .register(route(RouteKind::Exact, "/once", "ONCE"))
        .unwrap();
    assert_eq!(
        request(port, "GET", "/once").await.unwrap(),
        (200, "ONCE".into())
    );
    once.remove();
    assert_eq!(
        request(port, "GET", "/once").await.unwrap(),
        (200, "shell".into())
    );
    let _again = server
        .register(route(RouteKind::Exact, "/once", "AGAIN"))
        .unwrap();
    fallback.remove();
    assert_eq!(
        request(port, "GET", "/no/such/route").await.unwrap(),
        (404, String::new())
    );
    let _fallback = server
        .register_fallback(|_| async { Ok(text(StatusCode::OK, "")) })
        .unwrap();
    server.close().await;
}

#[tokio::test]
async fn upgrades_match_exactly_and_close_with_the_server() {
    let server = server().await;
    let port = server.port();
    let _probe = server
        .register(route(RouteKind::Exact, "/probe", "EXACT"))
        .unwrap();
    let upgraded_closed = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&upgraded_closed);

    /// Sets its flag when the adopted connection is dropped.
    struct OnDrop(Arc<AtomicBool>);
    impl Drop for OnDrop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    let events = server
        .register_upgrade(UpgradeRoute::new(
            "/events",
            move |mut request, upgrades| {
                let flag = Arc::clone(&flag);
                async move {
                    upgrades.adopt(on_upgrade(&mut request), move |io| async move {
                        let _guard = OnDrop(flag);
                        let _io = io;
                        std::future::pending::<()>().await;
                    });
                    let mut response = karaka_webserver::Response::new(body::empty());
                    *response.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
                    response
                        .headers_mut()
                        .insert(http::header::CONNECTION, "Upgrade".parse().unwrap());
                    response
                        .headers_mut()
                        .insert(http::header::UPGRADE, "dsh-test".parse().unwrap());
                    Ok(response)
                }
            },
        ))
        .unwrap();
    assert_eq!(
        server
            .register_upgrade(UpgradeRoute::new("/events", |_, _| async {
                Ok(text(StatusCode::OK, ""))
            }))
            .unwrap_err(),
        "webserver: duplicate upgrade route \"/events\""
    );
    let (mut upgraded, head) = upgrade(port, "/events?stream=mux").await;
    assert!(
        head.starts_with("HTTP/1.1 101 Switching Protocols\r\n"),
        "{head}"
    );
    events.remove();
    let _again = server
        .register_upgrade(UpgradeRoute::new("/events", |_, _| async {
            Ok(text(StatusCode::OK, ""))
        }))
        .unwrap();

    // A failing upgrade handler, and an upgrade no route owns, close their
    // connections; the server carries on.
    let _failing = server
        .register_upgrade(UpgradeRoute::new("/upgrade-error", |_, _| async {
            Err("test upgrade transport failure".into())
        }))
        .unwrap();
    for path in ["/upgrade-error", "/unrouted"] {
        let (_, head) = upgrade(port, path).await;
        assert_eq!(head, "", "{path}");
    }
    assert_eq!(
        request(port, "GET", "/probe").await.unwrap(),
        (200, "EXACT".into())
    );

    // Closing closes the upgraded connection too, and nothing is served
    // after.
    assert!(!upgraded_closed.load(Ordering::SeqCst));
    server.close().await;
    assert!(upgraded_closed.load(Ordering::SeqCst));
    let mut rest = Vec::new();
    let read = tokio::time::timeout(Duration::from_secs(5), upgraded.read_to_end(&mut rest)).await;
    assert!(matches!(read, Ok(Ok(0)) | Ok(Err(_))), "{read:?}");
    assert!(request(port, "GET", "/probe").await.is_err());
}

#[tokio::test]
async fn a_streamed_body_reaches_the_client_as_it_is_written() {
    let server = server().await;
    let port = server.port();
    let release = Arc::new(tokio::sync::Notify::new());
    let wake = Arc::clone(&release);
    let _events = server
        .register(WebRoute::new(RouteKind::Exact, "/sse", move |_| {
            let wake = Arc::clone(&wake);
            async move {
                let (tx, body) = body::channel(4);
                tx.send("data: one\n\n").await?;
                tokio::spawn(async move {
                    wake.notified().await;
                    let _ = tx.send("data: two\n\n").await;
                });
                let mut response = karaka_webserver::Response::new(body);
                response.headers_mut().insert(
                    http::header::CONTENT_TYPE,
                    "text/event-stream".parse().unwrap(),
                );
                Ok(response)
            }
        }))
        .unwrap();
    let mut socket = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    socket
        .write_all(b"GET /sse HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .unwrap();
    // The first event arrives while the body is still open.
    let mut seen = String::new();
    let mut chunk = [0; 1024];
    while !seen.contains("data: one") {
        let read = socket.read(&mut chunk).await.unwrap();
        assert!(read > 0);
        seen.push_str(&String::from_utf8_lossy(&chunk[..read]));
    }
    assert!(!seen.contains("data: two"));
    assert!(seen.contains("text/event-stream"));
    release.notify_one();
    while !seen.contains("data: two") {
        let read = socket.read(&mut chunk).await.unwrap();
        assert!(read > 0);
        seen.push_str(&String::from_utf8_lossy(&chunk[..read]));
    }
    drop(socket);
    server.close().await;
}

#[tokio::test]
async fn a_port_in_use_fails_the_listen() {
    let first = server().await;
    let error = WebServer::listen(
        WebServerConfig::new("127.0.0.1", first.port()).unwrap(),
        None,
    )
    .await
    .unwrap_err();
    assert!(error.starts_with("webserver: listen 127.0.0.1:"), "{error}");
    assert!(error.contains("Address already in use"), "{error}");
    first.close().await;
}
