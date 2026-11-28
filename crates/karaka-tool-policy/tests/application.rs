//! The policy's split of application names: global names restricted,
//! application names granted through the application policy. (Karaka:
//! `mcp-application/tests/tool-policy.spec.ts`, and the specs of
//! `catalog.spec.ts` that mount the policy.) Then a real endpoint: a
//! preset allowing its application tool, which reaches only the sessions
//! its application owns.
//!
//! Not ported: the halves of Karaka's specs that unload the policy's
//! plugin and see its grants go; the kernel unloads no plugin.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;

use karaka_agent::Agents;
use karaka_agent_loop::{AgentLoopPlugin, LoopConfig};
use karaka_agent_presets::{
    AgentPresets, AgentPresetsConfig, AgentPresetsPlugin, PresetDefinition,
};
use karaka_credentials::{CredentialsPlugin, EnvCredentials};
use karaka_identity::{
    ApplicationId, ApplicationOwner, InMemorySessionOwners, SessionOwnersPlugin, TenantId, UserId,
};
use karaka_kernel::{BoxError, DynPlugin, Plugin, PluginContext, PortId, Runtime};
use karaka_mcp_application::{
    ApplicationCatalog, ApplicationCatalogPlugin, ApplicationConfig, ApplicationPolicies,
    ApplicationToolPolicy, CatalogEndpoint, McpApplicationPlugin,
};
use karaka_model::{ModelsPlugin, ToolCall, ToolSchema};
use karaka_scope::{PresetId, ScopeChain, ScopeKey};
use karaka_server_auth::{ApplicationCredentialConfig, BearerConfig, ServerAuthPlugin};
use karaka_session::{SessionId, SessionsPlugin};
use karaka_session_projection::SessionProjectionsPlugin;
use karaka_system_prompt::{SystemPromptConfig, SystemPromptPlugin};
use karaka_tool_policy::{ToolMode, ToolPolicy};
use karaka_tools::{Tool, ToolFuture, ToolOutput, ToolRegistry, ToolRun, ToolsPlugin};
use serde_json::{Value, json};

const NAME: &str = "mcp__app__read";

struct Named(&'static str);

impl Tool for Named {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.0.into(),
            description: format!("tool {}", self.0),
            parameters: r#"{"type":"object","properties":{}}"#.into(),
        }
    }

    fn execute<'a>(&'a self, _: Value, _: ToolRun) -> ToolFuture<'a> {
        Box::pin(async move { ToolOutput::ok(self.0) })
    }
}

/// Registers `read` and `write` globally.
struct GlobalTools;

impl Plugin for GlobalTools {
    fn name(&self) -> &str {
        "global-tools"
    }

    fn requires(&self) -> Vec<PortId> {
        vec![PortId::of::<ToolRegistry>()]
    }

    async fn setup(&self, ctx: &mut PluginContext<'_>) -> Result<(), BoxError> {
        let tools = ctx.get::<ToolRegistry>();
        for name in ["read", "write"] {
            let _ = tools.register(Arc::new(Named(name)))?;
        }
        Ok(())
    }
}

/// Stands in for an endpoint: claims `app`, contributes the application
/// name, and settles, keeping the claim for the test.
struct Endpoint(Arc<Mutex<Option<CatalogEndpoint>>>);

impl Plugin for Endpoint {
    fn name(&self) -> &str {
        "endpoint"
    }

    fn requires(&self) -> Vec<PortId> {
        vec![PortId::of::<ApplicationCatalog>()]
    }

    async fn setup(&self, ctx: &mut PluginContext<'_>) -> Result<(), BoxError> {
        let catalog = ctx.get::<ApplicationCatalog>();
        let endpoint = catalog.endpoint("app")?;
        let _ = catalog.register_tool(NAME);
        endpoint.settle();
        *self.0.lock().unwrap() = Some(endpoint);
        Ok(())
    }
}

fn list(names: &[&str]) -> Option<Vec<String>> {
    Some(names.iter().map(|name| (*name).to_owned()).collect())
}

fn policy(allow: Option<&[&str]>, deny: Option<&[&str]>) -> PresetDefinition {
    PresetDefinition::new(
        "main",
        vec![Box::new(ToolPolicy {
            mode: ToolMode::Native,
            allow: allow.and_then(list),
            deny: deny.and_then(list),
        })],
    )
}

fn host(middle: Vec<Box<dyn DynPlugin>>, preset: PresetDefinition) -> Vec<Box<dyn DynPlugin>> {
    let mut plugins: Vec<Box<dyn DynPlugin>> = vec![
        Box::new(SessionsPlugin),
        Box::new(SessionProjectionsPlugin),
        Box::new(SystemPromptPlugin::new(SystemPromptConfig::default())),
        Box::new(ToolsPlugin),
        Box::new(ModelsPlugin),
        Box::new(AgentLoopPlugin::new(LoopConfig::new("mock", "mock-model"))),
        Box::new(GlobalTools),
    ];
    plugins.extend(middle);
    plugins.push(Box::new(
        AgentPresetsPlugin::new(AgentPresetsConfig::new("main")).preset(preset),
    ));
    plugins
}

/// A runtime whose catalog expects `app`, settled by [`Endpoint`].
async fn settled(preset: PresetDefinition) -> (Runtime, Arc<Mutex<Option<CatalogEndpoint>>>) {
    let endpoint = Arc::default();
    let runtime = Runtime::start(host(
        vec![
            Box::new(ApplicationCatalogPlugin::new(vec!["app".into()])),
            Box::new(Endpoint(Arc::clone(&endpoint))),
        ],
        preset,
    ))
    .await
    .unwrap();
    (runtime, endpoint)
}

fn chain(session: Option<&str>) -> ScopeChain {
    ScopeChain::new(Some(PresetId::new("main")), session.map(SessionId::new))
}

fn visible(runtime: &Runtime, chain: &ScopeChain) -> Vec<String> {
    let mut names: Vec<String> = runtime
        .get::<ToolRegistry>()
        .schemas_for(chain)
        .into_iter()
        .map(|schema| schema.name)
        .collect();
    names.sort();
    names
}

fn broken(runtime: &Runtime) -> Option<String> {
    runtime
        .get::<AgentPresets>()
        .resolve(Some("main"))
        .unwrap()
        .broken
}

#[tokio::test]
async fn omitted_lists_file_nothing() {
    let (runtime, _) = settled(policy(None, None)).await;
    assert_eq!(visible(&runtime, &chain(None)), ["read", "write"]);
    assert!(
        !runtime
            .get::<ApplicationPolicies>()
            .allows(&chain(None), NAME)
    );
}

#[tokio::test]
async fn global_names_are_restricted_and_application_names_granted() {
    let (runtime, _) = settled(policy(Some(&["read", NAME]), None)).await;
    assert_eq!(broken(&runtime), None);
    assert_eq!(visible(&runtime, &chain(None)), ["read"]);
    assert!(
        runtime
            .get::<ApplicationPolicies>()
            .allows(&chain(None), NAME)
    );
}

#[tokio::test]
async fn a_deny_alone_masks_and_overrides_an_application_grant() {
    let (runtime, _) = settled(policy(None, Some(&["write", NAME]))).await;
    assert_eq!(visible(&runtime, &chain(None)), ["read"]);
    let policies = runtime.get::<ApplicationPolicies>();
    let _grant = policies
        .register(
            Some(&ScopeKey::Session(SessionId::new("chat"))),
            ApplicationToolPolicy {
                allow: list(&[NAME]),
                deny: None,
            },
        )
        .unwrap();
    assert!(!policies.allows(&chain(Some("chat")), NAME));
    assert!(policies.allows(&ScopeChain::session(SessionId::new("chat")), NAME));
}

#[tokio::test]
async fn an_endpoint_leaving_keeps_the_global_restriction() {
    let (runtime, endpoint) = settled(policy(Some(&["read", NAME]), None)).await;
    assert_eq!(visible(&runtime, &chain(None)), ["read"]);
    endpoint.lock().unwrap().take().unwrap().dispose();
    assert_eq!(visible(&runtime, &chain(None)), ["read"]);
}

#[tokio::test]
async fn an_unsettled_catalog_breaks_the_preset() {
    let runtime = Runtime::start(host(
        vec![Box::new(ApplicationCatalogPlugin::new(vec!["app".into()]))],
        policy(Some(&["read", NAME]), None),
    ))
    .await
    .unwrap();
    assert_eq!(
        broken(&runtime).as_deref(),
        Some(
            "tool-policy (tool-policy): tool-policy: application catalog servers have not settled: \"app\""
        )
    );
}

#[tokio::test]
async fn with_no_applications_an_unknown_name_still_breaks_the_preset() {
    let runtime = Runtime::start(host(
        vec![Box::new(ApplicationCatalogPlugin::default())],
        policy(Some(&[]), None),
    ))
    .await
    .unwrap();
    assert_eq!(broken(&runtime), None);
    let runtime = Runtime::start(host(
        vec![Box::new(ApplicationCatalogPlugin::default())],
        policy(Some(&["mcp__missing__read"]), None),
    ))
    .await
    .unwrap();
    assert!(
        broken(&runtime)
            .unwrap()
            .contains("names unknown global tool \"mcp__missing__read\"")
    );
}

// A real endpoint: a loopback MCP server speaking the legacy handshake.

#[derive(Default)]
struct Server {
    calls: Mutex<Vec<(Value, Option<String>)>>,
}

fn read_request(socket: &mut TcpStream) -> Option<(String, HashMap<String, String>, String)> {
    let mut bytes = Vec::new();
    let mut chunk = [0; 4096];
    loop {
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&bytes[..end]).to_string();
            let mut lines = head.lines();
            let method = lines.next()?.split(' ').next()?.to_owned();
            let headers: HashMap<String, String> = lines
                .filter_map(|line| line.split_once(':'))
                .map(|(name, value)| (name.trim().to_lowercase(), value.trim().to_owned()))
                .collect();
            let length: usize = headers
                .get("content-length")
                .map_or(0, |value| value.parse().unwrap());
            while bytes.len() < end + 4 + length {
                let read = socket.read(&mut chunk).ok()?;
                if read == 0 {
                    return None;
                }
                bytes.extend_from_slice(&chunk[..read]);
            }
            let body = String::from_utf8_lossy(&bytes[end + 4..end + 4 + length]).to_string();
            return Some((method, headers, body));
        }
        let read = socket.read(&mut chunk).ok()?;
        if read == 0 {
            return None;
        }
        bytes.extend_from_slice(&chunk[..read]);
    }
}

fn answer(server: &Server, socket: &mut TcpStream) {
    let Some((method, headers, body)) = read_request(socket) else {
        return;
    };
    let respond = |socket: &mut TcpStream, status: &str, body: &str| {
        let _ = socket.write_all(
            format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        );
    };
    if method == "GET" {
        return respond(socket, "405 Method Not Allowed", "");
    }
    let message: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    let id = message["id"].clone();
    let reply = |result: Value| json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string();
    match message["method"].as_str().unwrap_or_default() {
        "server/discover" => respond(
            socket,
            "200 OK",
            &json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": "Method not found" } }).to_string(),
        ),
        "initialize" => respond(
            socket,
            "200 OK",
            &reply(json!({
                "protocolVersion": "2025-06-18",
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "application-fixture", "version": "1" },
            })),
        ),
        "tools/list" => respond(
            socket,
            "200 OK",
            &reply(json!({ "tools": [{ "name": "read", "inputSchema": { "type": "object" } }] })),
        ),
        "tools/call" => {
            server
                .calls
                .lock()
                .unwrap()
                .push((message["params"].clone(), headers.get("authorization").cloned()));
            respond(
                socket,
                "200 OK",
                &reply(json!({ "content": [{ "type": "text", "text": "accepted" }] })),
            );
        }
        _ => respond(socket, "202 Accepted", ""),
    }
}

fn serve(server: Arc<Server>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    thread::spawn(move || {
        for socket in listener.incoming() {
            let Ok(mut socket) = socket else { return };
            let server = Arc::clone(&server);
            thread::spawn(move || answer(&server, &mut socket));
        }
    });
    format!("http://{address}/mcp")
}

#[tokio::test]
async fn a_preset_allows_its_application_tools_to_the_sessions_its_application_owns() {
    let server = Arc::new(Server::default());
    let url = serve(Arc::clone(&server));
    let owners = Arc::new(InMemorySessionOwners::new());
    // The tool credential resolves from a lookup of the test's own, never
    // the real environment.
    let credentials = EnvCredentials::with_lookup(Arc::new(|name| {
        (name == "APP_TOOLS").then(|| "tool-secret".to_owned())
    }));
    let runtime = Runtime::start(host(
        vec![
            Box::new(SessionOwnersPlugin::with(Arc::clone(&owners))),
            Box::new(CredentialsPlugin::with(Arc::new(credentials))),
            Box::new(ServerAuthPlugin::bearer(BearerConfig {
                applications: vec![ApplicationCredentialConfig {
                    id: "app".into(),
                    chat_credential: "APP_CHAT".into(),
                    tool_credential: "APP_TOOLS".into(),
                }],
            })),
            Box::new(ApplicationCatalogPlugin::new(vec!["app".into()])),
            Box::new(McpApplicationPlugin::new(ApplicationConfig {
                allow: vec![NAME.into()],
                reconnect: Some(json!({ "enabled": false })),
                ..ApplicationConfig::new("app", "app", url)
            })),
        ],
        policy(Some(&["read", NAME]), None),
    ))
    .await
    .unwrap();
    assert_eq!(broken(&runtime), None);
    owners.set(
        SessionId::new("owned"),
        ApplicationOwner {
            application_id: ApplicationId::new("app"),
            tenant_id: TenantId::new("tenant"),
            user_id: UserId::new("user"),
        },
    );
    let agents = runtime.get::<dyn Agents>();
    let owned = agents.open(SessionId::new("owned")).unwrap();
    let foreign = agents.open(SessionId::new("foreign")).unwrap();
    assert_eq!(owned.scope(), chain(Some("owned")));
    assert_eq!(visible(&runtime, &owned.scope()), [NAME, "read"]);
    assert_eq!(visible(&runtime, &foreign.scope()), ["read"]);
    let output = runtime
        .get::<ToolRegistry>()
        .execute(
            &ToolCall {
                id: "call-1".into(),
                name: NAME.into(),
                arguments: "{}".into(),
            },
            Some(owned),
        )
        .await;
    assert_eq!(output, ToolOutput::ok("accepted"));
    let calls = server.calls.lock().unwrap().clone();
    assert_eq!(
        calls,
        [(
            json!({
                "name": "read",
                "arguments": {},
                "_meta": { "karaka": {
                    "applicationId": "app", "tenantId": "tenant", "userId": "user", "chatId": "owned",
                } },
            }),
            Some("Bearer tool-secret".to_owned()),
        )]
    );
}
