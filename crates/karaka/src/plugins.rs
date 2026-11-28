//! `KARAKA_PLUGINS`: the global plugins, read from a YAML list of rows
//! shaped like a Cordis profile's, as Play This Life's
//! `karaka.cordis.yml` declares them:
//!
//! ```yaml
//! - id: application-catalog
//!   config:
//!     servers: [play_this_life]
//! - name: '@karaka-ai/server-auth'
//!   config:
//!     applications:
//!       - { id: play-this-life, chatCredential: PTL_CHAT, toolCredential: PTL_TOOLS }
//! - id: play-this-life-tools
//!   name: '@karaka-ai/mcp-application'
//!   config:
//!     applicationId: play-this-life
//!     serverName: play_this_life
//!     url: http://127.0.0.1:3002/api/karaka/tools
//!     allow: [mcp__play_this_life__readTasks]
//! ```
//!
//! A row's plugin is its `name`, or its `id` when it has none, as a
//! profile's rows patching a bundle's name theirs by `id`. A row with
//! `disabled: true` is skipped. The plugins, by short name or package:
//!
//! - `mcp-client` (`@deepseek-ai/dsh-mcp-client`): one MCP server every
//!   session sees, with dsh's config, as in a preset's row;
//! - `application-catalog` (`@karaka-ai/mcp-application/catalog`):
//!   `servers`, the application endpoints expected;
//! - `server-auth` (`@karaka-ai/server-auth`): the shared-bearer provider,
//!   `applications` with `id`, `chatCredential` and `toolCredential`, each
//!   credential an environment variable's name, read at every use;
//! - `mcp-application` (`@karaka-ai/mcp-application`): one application's
//!   endpoint, with Karaka's config.
//!
//! Anything else, an unknown key included, fails the start with a message
//! that says where, and so do rows that don't fit together: an endpoint
//! without `server-auth`, or one its catalog doesn't list.

use std::path::Path;
use std::sync::Arc;

use karaka_credentials::EnvCredentials;
use karaka_mcp_application::{ApplicationCatalog, ApplicationConfig};
use karaka_mcp_client::McpServerConfig;
use karaka_server_auth::{BearerConfig, BearerServerAuth};
use karaka_transport_http::{TransportConfig, normalize_base};
use serde_json::{Map, Value};
use yaml_rust2::YamlLoader;

use crate::presets::{boolean, json, object, string, strings};

/// The global plugins a file asks for.
#[derive(Debug, Default)]
pub struct GlobalPlugins {
    /// Every `mcp-client` row's server.
    pub mcp_servers: Vec<McpServerConfig>,
    /// The `application-catalog` row's servers; none without one.
    pub catalog: Vec<String>,
    /// The `server-auth` row's applications.
    pub server_auth: Option<BearerConfig>,
    /// Every `mcp-application` row's endpoint.
    pub applications: Vec<ApplicationConfig>,
    /// The `application-http` row's routes, and the warnings to show for
    /// what of it isn't mounted yet.
    pub transport: Option<(TransportConfig, Vec<String>)>,
    /// The file, for errors that name it.
    pub at: String,
}

/// The global plugins in the file at `path`.
///
/// # Errors
///
/// A file that doesn't read or parse, or rows that don't fit, said with
/// where they are.
pub fn load(path: &Path) -> Result<GlobalPlugins, String> {
    let at = format!("KARAKA_PLUGINS ({})", path.display());
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("{at}: couldn't read it: {error}"))?;
    parse(&text, &at)
}

/// [`load`] for the file's `text`; `at` names the file in errors.
pub fn parse(text: &str, at: &str) -> Result<GlobalPlugins, String> {
    let documents =
        YamlLoader::load_from_str(text).map_err(|error| format!("{at}: not YAML: {error}"))?;
    let rows = match documents.first().map(json) {
        Some(Value::Array(rows)) => rows,
        None | Some(Value::Null) => Vec::new(),
        _ => return Err(format!("{at}: must be a list of plugin rows")),
    };
    let mut plugins = GlobalPlugins {
        at: at.to_owned(),
        ..GlobalPlugins::default()
    };
    let mut transport_at: Option<String> = None;
    // Where each single row and each endpoint was, for the checks across
    // rows.
    let mut catalog_at: Option<String> = None;
    let mut auth_at: Option<String> = None;
    let mut endpoints_at: Vec<String> = Vec::new();
    let empty = Value::Object(Map::new());
    for (index, row) in rows.iter().enumerate() {
        let place = format!("{at}: plugin {}", index + 1);
        // `inject` names a Cordis row's services; the kernel orders plugins
        // by what they require, so it is read and ignored.
        let row = object(row, &place, &["id", "name", "disabled", "inject", "config"])?;
        let name = match string(row, "name", &place)? {
            Some(name) => name,
            None => string(row, "id", &place)?.ok_or(format!("{place} needs a \"name\""))?,
        };
        if boolean(row, "disabled", &place)? == Some(true) {
            continue;
        }
        let config = match row.get("config") {
            None | Some(Value::Null) => &empty,
            Some(config) => config,
        };
        let place = format!("{place} ({name})");
        match name.as_str() {
            "mcp-client" | "@deepseek-ai/dsh-mcp-client" => plugins
                .mcp_servers
                .push(McpServerConfig::from_json(config, &place)?),
            "application-catalog" | "@karaka-ai/mcp-application/catalog" => {
                if catalog_at.is_some() {
                    return Err(format!(
                        "{place}: only one application-catalog row may be given"
                    ));
                }
                let map = object(config, &place, &["servers"])?;
                let servers = strings(map, "servers", &place)?
                    .ok_or(format!("{place} needs \"servers\", a list"))?;
                ApplicationCatalog::new(servers.clone())
                    .map_err(|error| format!("{place}: {error}"))?;
                plugins.catalog = servers;
                catalog_at = Some(place);
            }
            "server-auth" | "@karaka-ai/server-auth" => {
                if auth_at.is_some() {
                    return Err(format!("{place}: only one server-auth row may be given"));
                }
                let config = BearerConfig::from_json(config, &place)?;
                // Its ids and references, checked now rather than at start.
                BearerServerAuth::new(&config, Arc::new(EnvCredentials::new()))
                    .map_err(|error| format!("{place}: {error}"))?;
                plugins.server_auth = Some(config);
                auth_at = Some(place);
            }
            "mcp-application" | "@karaka-ai/mcp-application" => {
                plugins
                    .applications
                    .push(ApplicationConfig::from_json(config, &place)?);
                endpoints_at.push(place);
            }
            "application-http" | "@karaka-ai/transport-http" => {
                if transport_at.is_some() {
                    return Err(format!(
                        "{place}: only one application-http row may be given"
                    ));
                }
                plugins.transport = Some(transport(config, &place)?);
                transport_at = Some(place);
            }
            other => {
                return Err(format!(
                    "{place}: unknown plugin \"{other}\"; the global plugins may be mcp-client, \
                     application-catalog, server-auth, mcp-application and application-http"
                ));
            }
        }
    }
    check_applications(&plugins, catalog_at.as_deref(), &endpoints_at)?;
    if let Some(at) = &transport_at
        && plugins.server_auth.is_none()
    {
        return Err(format!(
            "{at}: needs a server-auth row, for the application servers' credentials"
        ));
    }
    Ok(plugins)
}

/// Reads an `application-http` row: Karaka's `handleQuestions`, `path`
/// and `maxBodyBytes`, and its browser fields, checked as Karaka checks
/// them, then left unmounted with a warning, as browser access is HTTP
/// ingress step 4.
fn transport(config: &Value, at: &str) -> Result<(TransportConfig, Vec<String>), String> {
    const BROWSER: [&str; 6] = [
        "browserPath",
        "browserOrigins",
        "browserAgentIds",
        "browserMethods",
        "browserEvents",
        "browserHeartbeatMs",
    ];
    let keys: Vec<&str> = ["handleQuestions", "path", "maxBodyBytes"]
        .into_iter()
        .chain(BROWSER)
        .collect();
    let map = object(config, at, &keys)?;
    let mut transport = TransportConfig::default();
    if let Some(handle) = boolean(map, "handleQuestions", at)? {
        transport.handle_questions = handle;
    }
    if let Some(path) = string(map, "path", at)? {
        normalize_base(&path).map_err(|error| format!("{at}: {error}"))?;
        transport.path = path;
    }
    match map.get("maxBodyBytes") {
        None | Some(Value::Null) => {}
        Some(value) => {
            transport.max_body_bytes = value
                .as_u64()
                .and_then(|bytes| usize::try_from(bytes).ok())
                .ok_or(format!("{at}: \"maxBodyBytes\" must be a whole number"))?;
        }
    }
    let mut warnings = Vec::new();
    if let Some(browser) = string(map, "browserPath", at)? {
        let origins = strings(map, "browserOrigins", at)?.unwrap_or_default();
        if origins.is_empty() {
            return Err(format!("{at}: Browser transport requires explicit origins"));
        }
        let presets = strings(map, "browserAgentIds", at)?.unwrap_or_default();
        if presets.is_empty() || presets.iter().any(|id| id.trim().is_empty()) {
            return Err(format!(
                "{at}: Browser transport requires explicit Agent presets"
            ));
        }
        normalize_base(&browser).map_err(|error| format!("{at}: {error}"))?;
        let base = &transport.path;
        if browser == *base
            || browser.starts_with(&format!("{base}/"))
            || base.starts_with(&format!("{browser}/"))
        {
            return Err(format!("{at}: Browser and backend paths must be disjoint"));
        }
        warnings.push(format!(
            "{at}: browser access at {browser} isn't mounted: it comes with HTTP ingress step 4"
        ));
    }
    Ok((transport, warnings))
}

/// That every endpoint has credentials and a place in the catalog, and
/// every catalog server an endpoint, so no preset waits on a server that
/// never settles.
fn check_applications(
    plugins: &GlobalPlugins,
    catalog_at: Option<&str>,
    endpoints_at: &[String],
) -> Result<(), String> {
    for (index, (endpoint, at)) in plugins.applications.iter().zip(endpoints_at).enumerate() {
        let server = &endpoint.server_name;
        let Some(auth) = &plugins.server_auth else {
            return Err(format!(
                "{at}: needs a server-auth row, for the endpoint's credentials"
            ));
        };
        if !auth
            .applications
            .iter()
            .any(|application| application.id == endpoint.application_id)
        {
            return Err(format!(
                "{at}: application \"{}\" isn't one of the server-auth row's applications",
                endpoint.application_id
            ));
        }
        if !plugins.catalog.contains(server) {
            return Err(match catalog_at {
                Some(_) => format!(
                    "{at}: server \"{server}\" isn't one of the application-catalog row's servers"
                ),
                None => {
                    format!("{at}: needs an application-catalog row listing server \"{server}\"")
                }
            });
        }
        if plugins.applications[..index]
            .iter()
            .any(|earlier| earlier.server_name == *server)
        {
            return Err(format!(
                "{at}: server \"{server}\" has an mcp-application row already"
            ));
        }
    }
    if let Some(catalog_at) = catalog_at
        && let Some(server) = plugins.catalog.iter().find(|server| {
            !plugins
                .applications
                .iter()
                .any(|endpoint| endpoint.server_name == **server)
        })
    {
        return Err(format!(
            "{catalog_at}: server \"{server}\" has no mcp-application row"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::parse;

    fn refused(text: &str) -> String {
        match parse(text, "plugins.yml") {
            Ok(_) => panic!("expected {text:?} to be refused"),
            Err(message) => message,
        }
    }

    const AUTH: &str = "- name: server-auth\n  config:\n    applications: [{ id: app, chatCredential: APP_CHAT, toolCredential: APP_TOOLS }]\n";
    const CATALOG: &str = "- id: application-catalog\n  config: { servers: [app] }\n";
    const ENDPOINT: &str = "- id: app-tools\n  name: '@karaka-ai/mcp-application'\n  config: { applicationId: app, serverName: app, url: 'http://127.0.0.1:1/mcp', allow: [mcp__app__read] }\n";

    #[test]
    fn rows_shaped_like_a_profile_parse() {
        let text = format!(
            "{CATALOG}{AUTH}{ENDPOINT}- name: mcp-client\n  config: {{ transport: stdio, serverName: tools, command: sh }}\n- id: server-auth\n  disabled: true\n- id: browser-auth\n  disabled: true\n"
        );
        let plugins = parse(&text, "plugins.yml").unwrap();
        assert_eq!(plugins.catalog, ["app"]);
        assert_eq!(plugins.applications.len(), 1);
        assert_eq!(plugins.applications[0].allow, ["mcp__app__read"]);
        assert_eq!(plugins.mcp_servers.len(), 1);
        assert!(plugins.server_auth.is_some());
        assert!(parse("", "plugins.yml").unwrap().applications.is_empty());
    }

    #[test]
    fn mistakes_are_said_with_where_they_are() {
        for (text, message) in [
            ("{}".to_owned(), "plugins.yml: must be a list of plugin rows"),
            ("- config: {}".to_owned(), "plugins.yml: plugin 1 needs a \"name\""),
            (
                "- name: shell".to_owned(),
                "plugins.yml: plugin 1 (shell): unknown plugin \"shell\"; the global plugins may be mcp-client, application-catalog, server-auth, mcp-application and application-http",
            ),
            (
                "- name: mcp-client\n  colour: red".to_owned(),
                "plugins.yml: plugin 1: unknown key \"colour\"; it may have id, name, disabled, inject, config",
            ),
            (
                "- name: application-catalog".to_owned(),
                "plugins.yml: plugin 1 (application-catalog) needs \"servers\", a list",
            ),
            (
                "- name: application-catalog\n  config: { servers: [a, a] }".to_owned(),
                "plugins.yml: plugin 1 (application-catalog): Application catalog server names must be unique",
            ),
            (
                "- name: server-auth\n  config: { applications: [{ id: app, chatCredential: 'not a ref', toolCredential: T }] }".to_owned(),
                "plugins.yml: plugin 1 (server-auth): credential ref \"not a ref\" must match /^[A-Za-z_][A-Za-z0-9_]*$/",
            ),
            (
                "- name: mcp-application\n  config: { serverName: app, url: x }".to_owned(),
                "plugins.yml: plugin 1 (mcp-application) needs \"applicationId\"",
            ),
            (
                format!("{CATALOG}{ENDPOINT}"),
                "plugins.yml: plugin 2 (@karaka-ai/mcp-application): needs a server-auth row, for the endpoint's credentials",
            ),
            (
                format!("{AUTH}{ENDPOINT}"),
                "plugins.yml: plugin 2 (@karaka-ai/mcp-application): needs an application-catalog row listing server \"app\"",
            ),
            (
                format!("{AUTH}- name: application-catalog\n  config: {{ servers: [other] }}\n{ENDPOINT}"),
                "plugins.yml: plugin 3 (@karaka-ai/mcp-application): server \"app\" isn't one of the application-catalog row's servers",
            ),
            (
                format!("{AUTH}{CATALOG}"),
                "plugins.yml: plugin 2 (application-catalog): server \"app\" has no mcp-application row",
            ),
            (
                format!("{CATALOG}{AUTH}{ENDPOINT}{ENDPOINT}"),
                "plugins.yml: plugin 4 (@karaka-ai/mcp-application): server \"app\" has an mcp-application row already",
            ),
            (
                format!("{CATALOG}{CATALOG}"),
                "plugins.yml: plugin 2 (application-catalog): only one application-catalog row may be given",
            ),
            (
                format!("{CATALOG}- name: server-auth\n  config:\n    applications: [{{ id: other, chatCredential: C, toolCredential: T }}]\n{ENDPOINT}"),
                "plugins.yml: plugin 3 (@karaka-ai/mcp-application): application \"app\" isn't one of the server-auth row's applications",
            ),
        ] {
            assert_eq!(refused(&text), message, "{text}");
        }
    }
}
