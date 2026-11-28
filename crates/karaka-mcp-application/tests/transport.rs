//! Credential rotation and redirect refusal, through real loopback HTTP.
//! (Karaka: `mcp-application/tests/transport.spec.ts`.)
//!
//! Not ported: `transport-fetch.spec.ts`, which checks that the SDK's
//! `fetch` callback asks for credentials without inventing an
//! `AbortSignal`; a Rust header source takes none.

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use karaka_mcp_application::application_transport;
use karaka_mcp_client::{Client, ClientOptions, HeaderSource, RequestOptions};

use common::{Fixture, redirect, serve};

fn statics() -> Vec<(String, String)> {
    vec![
        ("authorization".into(), "Bearer stale".into()),
        ("x-static".into(), "retained".into()),
    ]
}

fn source(credential: Arc<Mutex<String>>) -> HeaderSource {
    Arc::new(move || {
        let value = credential.lock().unwrap().clone();
        Box::pin(async move { Ok(vec![("authorization".to_owned(), value)]) })
    })
}

#[tokio::test]
async fn credentials_are_resolved_per_request_over_stale_static_ones() {
    let fixture = Arc::new(Fixture::default());
    let url = serve(Arc::clone(&fixture));
    let credential = Arc::new(Mutex::new("Bearer first".to_owned()));
    let factory = application_transport(&url, statics(), source(Arc::clone(&credential))).unwrap();
    let client = Client::new(factory(), ClientOptions::default());
    client.connect().await.unwrap();
    client
        .list_all_tools(RequestOptions::default())
        .await
        .unwrap();
    *credential.lock().unwrap() = "Bearer rotated".into();
    client
        .list_all_tools(RequestOptions::default())
        .await
        .unwrap();
    let observed: Vec<(String, String)> = fixture
        .headers_of("tools/list")
        .into_iter()
        .map(|headers| {
            (
                headers["authorization"].clone(),
                headers["x-static"].clone(),
            )
        })
        .collect();
    assert_eq!(
        observed,
        [
            ("Bearer first".to_owned(), "retained".to_owned()),
            ("Bearer rotated".to_owned(), "retained".to_owned()),
        ]
    );
    let _ = client.close().await;
}

#[tokio::test]
async fn a_credential_bearing_redirect_is_refused_without_contacting_its_target() {
    let target = Arc::new(Fixture::default());
    let url = redirect(serve(Arc::clone(&target)));
    let factory = application_transport(
        &url,
        statics(),
        source(Arc::new(Mutex::new("Bearer private".into()))),
    )
    .unwrap();
    let client = Client::new(factory(), ClientOptions::default());
    assert!(client.connect().await.is_err());
    assert!(target.seen.lock().unwrap().is_empty());
}

#[test]
fn a_non_http_endpoint_is_refused_before_credentials_are_asked_for() {
    for url in ["file:///tmp/mcp", "ftp://example.test/mcp"] {
        let requested = Arc::new(AtomicBool::new(false));
        let asked = Arc::clone(&requested);
        let source: HeaderSource = Arc::new(move || {
            asked.store(true, Ordering::SeqCst);
            Box::pin(async { Ok(Vec::new()) })
        });
        assert_eq!(
            application_transport(url, Vec::new(), source)
                .err()
                .unwrap(),
            "MCP endpoint must use HTTP or HTTPS"
        );
        assert!(!requested.load(Ordering::SeqCst));
    }
    let source: HeaderSource = Arc::new(|| Box::pin(async { Ok(Vec::new()) }));
    assert_eq!(
        application_transport("not a url", Vec::new(), source)
            .err()
            .unwrap(),
        "Invalid URL"
    );
}
