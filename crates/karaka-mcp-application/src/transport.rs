//! The application's HTTP transport: credentials resolved for every
//! request, so they rotate without reconnecting, and no redirects.
//! (Karaka: `mcp-application/src/transport.ts`, `createTransport`.)

use std::sync::Arc;

use karaka_mcp_client::{
    HeaderSource, HttpOptions, HttpParams, HttpTransport, Transport, TransportFactory,
};

/// A factory of transports to `url`, each request carrying `headers`, then
/// what `credentials` resolves over them. A credential-bearing request must
/// never follow a redirect to another origin, so none is followed.
///
/// # Errors
///
/// `url` isn't one, or isn't HTTP or HTTPS, in Karaka's words; checked
/// before any credential is asked for.
pub fn application_transport(
    url: &str,
    headers: Vec<(String, String)>,
    credentials: HeaderSource,
) -> Result<TransportFactory, String> {
    // `new URL` throws `TypeError: Invalid URL`.
    let endpoint = reqwest::Url::parse(url).map_err(|_| "Invalid URL".to_owned())?;
    if endpoint.scheme() != "http" && endpoint.scheme() != "https" {
        return Err("MCP endpoint must use HTTP or HTTPS".into());
    }
    let params = HttpParams {
        url: url.to_owned(),
        headers,
    };
    let options = HttpOptions {
        headers: Some(credentials),
        refuse_redirects: true,
    };
    Ok(Arc::new(move || -> Box<dyn Transport> {
        Box::new(HttpTransport::with_options(params.clone(), options.clone()))
    }))
}
