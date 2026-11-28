//! Application catalogs through real agent scopes, the tool registry and
//! HTTP MCP framing. (Karaka: `mcp-application/tests/application.spec.ts`.)
//!
//! Not ported:
//! - "drains in-flight discovery during disposal without publishing a late
//!   ready catalog": Cordis disposes a plugin while it starts; the kernel
//!   starts each plugin to the end, and unloads none.
//! - "removes an Agent catalog when its scope disposes": a Karaka agent
//!   lives as long as the runtime.
//!
//! Karaka's fixture checks `error.info.code`; a Karaka result is text, so
//! `UNKNOWN_TOOL` is the registry's `unknown tool` text, and
//! `INVALID_ARGS` its `invalid arguments` text.

mod common;

use std::sync::Arc;

use karaka_agent::Agent;
use karaka_identity::{
    ApplicationId, ApplicationOwner, InMemorySessionOwners, SessionOwners, TenantId, UserId,
};
use karaka_mcp_application::{
    Application, ApplicationCatalog, ApplicationConfig, ApplicationPolicies, ApplicationPorts,
    ApplicationToolPolicy, PolicyRegistration, start_application,
};
use karaka_mcp_client::McpStartError;
use karaka_model::{ToolCall, ToolSchema};
use karaka_scope::ScopeKey;
use karaka_server_auth::{AuthFuture, AuthenticatedApplication, ServerAuth};
use karaka_session::SessionId;
use karaka_tools::{Tool, ToolFuture, ToolOutput, ToolRegistry, ToolRun};
use serde_json::{Value, json};

use common::{Fixture, Registry, serve};

const NAME: &str = "mcp__app__read";

/// Trusted answers, as Karaka's fixture gives them.
struct Auth;

impl ServerAuth for Auth {
    fn authenticate<'a>(
        &'a self,
        _: Option<&'a str>,
    ) -> AuthFuture<'a, Option<AuthenticatedApplication>> {
        Box::pin(async { Ok(None) })
    }

    fn authorize_tools<'a>(&'a self, _: &'a ApplicationId) -> AuthFuture<'a, String> {
        Box::pin(async { Ok("Bearer application".to_owned()) })
    }
}

struct Harness {
    fixture: Arc<Fixture>,
    url: String,
    tools: Arc<ToolRegistry>,
    owners: Arc<InMemorySessionOwners>,
    catalog: Arc<ApplicationCatalog>,
    policies: Arc<ApplicationPolicies>,
    agents: Arc<Registry>,
    /// Kept for the test's life.
    kept: Vec<PolicyRegistration>,
}

fn owner(application: &str) -> ApplicationOwner {
    ApplicationOwner {
        application_id: ApplicationId::new(application),
        tenant_id: TenantId::new("tenant"),
        user_id: UserId::new("user"),
    }
}

fn harness() -> Harness {
    let fixture = Arc::new(Fixture::default());
    *fixture.tools.lock().unwrap() =
        vec![json!({ "name": "read", "inputSchema": { "type": "object" } })];
    let url = serve(Arc::clone(&fixture));
    Harness {
        fixture,
        url,
        tools: Arc::new(ToolRegistry::new()),
        owners: Arc::new(InMemorySessionOwners::new()),
        catalog: ApplicationCatalog::new(vec!["app".into()]).unwrap(),
        policies: ApplicationPolicies::new(),
        agents: Arc::default(),
        kept: Vec::new(),
    }
}

impl Harness {
    /// Opens the agent of `id`, owned by `owner`, its own scope allowing
    /// the tool.
    fn agent(&mut self, id: &str, owner: Option<ApplicationOwner>) -> Arc<dyn Agent> {
        if let Some(owner) = owner {
            self.owners.set(SessionId::new(id), owner);
        }
        self.kept.push(
            self.policies
                .register(
                    Some(&ScopeKey::Session(SessionId::new(id))),
                    ApplicationToolPolicy {
                        allow: Some(vec![NAME.into()]),
                        deny: None,
                    },
                )
                .unwrap(),
        );
        self.agents.enter(id, None)
    }

    fn config(&self) -> ApplicationConfig {
        ApplicationConfig {
            allow: vec![NAME.into()],
            reconnect: Some(json!({ "enabled": false })),
            ..ApplicationConfig::new("app", "app", &self.url)
        }
    }

    async fn mount(&self, config: ApplicationConfig) -> Result<Application, McpStartError> {
        start_application(
            ApplicationPorts {
                tools: Arc::clone(&self.tools),
                owners: Arc::clone(&self.owners) as Arc<dyn SessionOwners>,
                auth: Arc::new(Auth),
                catalog: Arc::clone(&self.catalog),
                policies: Arc::clone(&self.policies),
                agents: Arc::clone(&self.agents) as _,
                opened: Arc::clone(&self.agents.opened),
                log: None,
            },
            config,
        )
        .await
    }

    async fn invoke(&self, agent: Option<&Arc<dyn Agent>>, arguments: &str) -> ToolOutput {
        self.tools
            .execute(
                &ToolCall {
                    id: "application-call".into(),
                    name: NAME.into(),
                    arguments: arguments.into(),
                },
                agent.cloned(),
            )
            .await
    }
}

fn unknown(output: &ToolOutput) -> bool {
    output.is_error && output.content == format!("Error: unknown tool \"{NAME}\"")
}

fn meta(chat: &str) -> Value {
    json!({
        "name": "read",
        "arguments": {},
        "_meta": { "karaka": {
            "applicationId": "app", "tenantId": "tenant", "userId": "user", "chatId": chat,
        } },
    })
}

#[tokio::test]
async fn owned_agents_before_and_after_the_start_are_admitted_and_others_refused() {
    let mut h = harness();
    let first = h.agent("first", Some(owner("app")));
    let foreign = h.agent("foreign", Some(owner("other")));
    let _application = h.mount(h.config()).await.unwrap();
    let second = h.agent("second", Some(owner("app")));
    assert!(!h.invoke(Some(&first), "{}").await.is_error);
    assert!(!h.invoke(Some(&second), "{}").await.is_error);
    assert!(unknown(&h.invoke(Some(&foreign), "{}").await));
    assert!(unknown(&h.invoke(None, "{}").await));
    // Each call carries its owner and chat, in Karaka's key order.
    let calls = h.fixture.calls();
    assert_eq!(calls, [meta("first"), meta("second")]);
    assert_eq!(
        serde_json::to_string(&calls[0]["_meta"]).unwrap(),
        r#"{"karaka":{"applicationId":"app","tenantId":"tenant","userId":"user","chatId":"first"}}"#
    );
    // Every request carried the application's authorization.
    assert!(
        h.fixture
            .headers_of("tools/call")
            .iter()
            .all(|headers| headers["authorization"] == "Bearer application")
    );
}

#[tokio::test]
async fn a_cached_registration_is_authorized_again_at_dispatch() {
    let mut h = harness();
    let scoped = h.agent("owned", Some(owner("app")));
    let _application = h.mount(h.config()).await.unwrap();
    h.owners.set(SessionId::new("owned"), owner("revoked"));
    let output = h.invoke(Some(&scoped), "{}").await;
    assert!(output.is_error);
    assert_eq!(
        output.content,
        "Error: Application tool owner does not match endpoint"
    );
    assert!(h.fixture.calls().is_empty());
}

#[tokio::test]
async fn registrations_follow_policy_changes_and_the_endpoint_disposal() {
    let mut h = harness();
    let scoped = h.agent("owned", Some(owner("app")));
    let application = h.mount(h.config()).await.unwrap();
    let denied = h
        .policies
        .register(
            Some(&ScopeKey::Session(SessionId::new("owned"))),
            ApplicationToolPolicy {
                allow: None,
                deny: Some(vec![NAME.into()]),
            },
        )
        .unwrap();
    assert!(unknown(&h.invoke(Some(&scoped), "{}").await));
    denied.remove().unwrap();
    assert!(!h.invoke(Some(&scoped), "{}").await.is_error);
    application.dispose().await;
    assert!(unknown(&h.invoke(Some(&scoped), "{}").await));
    assert!(h.catalog.names().is_empty());
}

#[tokio::test]
async fn the_endpoints_own_lists_restrict_it() {
    for (allow, deny) in [
        (Vec::new(), Vec::new()),
        (vec![NAME.to_owned()], vec![NAME.to_owned()]),
    ] {
        let mut h = harness();
        let scoped = h.agent("owned", Some(owner("app")));
        let _application = h
            .mount(ApplicationConfig {
                allow,
                deny,
                ..h.config()
            })
            .await
            .unwrap();
        assert!(unknown(&h.invoke(Some(&scoped), "{}").await));
        assert!(h.fixture.calls().is_empty());
    }
}

#[tokio::test]
async fn an_agent_attached_twice_keeps_one_catalog() {
    let mut h = harness();
    let scoped = h.agent("owned", Some(owner("app")));
    let _application = h.mount(h.config()).await.unwrap();
    h.agents.opened.opened(&scoped);
    assert!(!h.invoke(Some(&scoped), "{}").await.is_error);
    assert_eq!(h.fixture.calls().len(), 1);
}

#[tokio::test]
async fn a_retained_tool_is_authorized_again_when_called() {
    let mut h = harness();
    let scoped = h.agent("owned", Some(owner("app")));
    let _application = h.mount(h.config()).await.unwrap();
    let tool = h.tools.get(NAME, &scoped.scope()).unwrap();
    let run = |agent: Option<Arc<dyn Agent>>| ToolRun {
        call_id: "retained".into(),
        name: NAME.into(),
        agent,
    };
    let output = tool.execute(json!({}), run(None)).await;
    assert_eq!(output.content, "Error: Application tool requires an Agent");
    let _denied = h
        .policies
        .register(
            Some(&ScopeKey::Session(SessionId::new("owned"))),
            ApplicationToolPolicy {
                allow: None,
                deny: Some(vec![NAME.into()]),
            },
        )
        .unwrap();
    let output = tool
        .execute(json!({}), run(Some(Arc::clone(&scoped))))
        .await;
    assert_eq!(output.content, "Error: Application tool is not allowed");
    assert!(h.fixture.calls().is_empty());
}

/// A tool of the agent's own, holding the application tool's name.
struct Existing;

impl Tool for Existing {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: NAME.into(),
            description: "Existing tool".into(),
            parameters: r#"{"type":"object"}"#.into(),
        }
    }

    fn execute<'a>(&'a self, _: Value, _: ToolRun) -> ToolFuture<'a> {
        Box::pin(async { ToolOutput::ok("existing") })
    }
}

#[tokio::test]
async fn a_name_an_agent_already_owns_rolls_the_catalog_back() {
    let mut h = harness();
    let scoped = h.agent("owned", Some(owner("app")));
    let _existing = h
        .tools
        .register_in(
            Some(&ScopeKey::Session(SessionId::new("owned"))),
            Arc::new(Existing),
        )
        .unwrap();
    let error = h.mount(h.config()).await.err().unwrap();
    assert_eq!(error.message, "Application MCP app could not start");
    assert!(h.catalog.names().is_empty());
    let kept = h.tools.get(NAME, &scoped.scope()).unwrap();
    assert_eq!(kept.schema().description, "Existing tool");
    // The server's claim was given up, so another start may claim it.
    assert!(h.catalog.endpoint("app").is_ok());
}

#[tokio::test]
async fn invalid_arguments_are_refused_before_dispatch() {
    let mut h = harness();
    *h.fixture.tools.lock().unwrap() = vec![json!({
        "name": "read",
        "inputSchema": {
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": { "id": { "type": "integer" } },
            "required": ["id"],
        },
    })];
    let scoped = h.agent("owned", Some(owner("app")));
    let _application = h.mount(h.config()).await.unwrap();
    let parameters: Value = serde_json::from_str(
        &h.tools
            .get(NAME, &scoped.scope())
            .unwrap()
            .schema()
            .parameters,
    )
    .unwrap();
    assert!(parameters.get("$schema").is_none());
    let output = h.invoke(Some(&scoped), "{}").await;
    assert!(output.is_error);
    assert!(
        output.content.starts_with("Error: invalid arguments: "),
        "{}",
        output.content
    );
    assert!(h.fixture.calls().is_empty());
}

#[tokio::test]
async fn a_failed_first_catalog_fails_the_start_unless_told_not_to() {
    let h = harness();
    *h.fixture.tools.lock().unwrap() = vec![
        json!({ "name": "read", "inputSchema": { "type": "object", "patternProperties": {} } }),
    ];
    let error = h.mount(h.config()).await.err().unwrap();
    assert_eq!(error.message, "Application MCP app could not start");
    assert!(
        error.to_string().contains("unsupported JSON schema"),
        "{error}"
    );
    assert_eq!(h.catalog.tools(), None);
    let application = h
        .mount(ApplicationConfig {
            fail_on_startup_error: false,
            ..h.config()
        })
        .await;
    assert!(application.is_ok());
    assert_eq!(h.catalog.tools(), Some(Default::default()));
}

#[tokio::test]
async fn a_non_http_endpoint_or_a_bad_name_fails_the_start() {
    let h = harness();
    let error = h
        .mount(ApplicationConfig {
            url: "file:///tmp/mcp".into(),
            fail_on_startup_error: false,
            ..h.config()
        })
        .await
        .err()
        .unwrap();
    assert_eq!(
        error.to_string(),
        "Application MCP app could not start: MCP endpoint must use HTTP or HTTPS"
    );
    let error = h
        .mount(ApplicationConfig {
            server_name: "has space".into(),
            ..h.config()
        })
        .await
        .err()
        .unwrap();
    assert_eq!(
        error.message,
        "karaka-mcp-application: \"serverName\" must match /^[A-Za-z0-9_-]{1,32}$/, not \"has space\""
    );
    let error = h
        .mount(ApplicationConfig {
            server_name: "other".into(),
            ..h.config()
        })
        .await
        .err()
        .unwrap();
    assert_eq!(
        error.message,
        "Application catalog does not configure server \"other\""
    );
}
