//! An application tool's input check. (Karaka:
//! `mcp-application/tests/tools.spec.ts`.)
//!
//! The tools here have a client that never connected, so a call that gets
//! past the checks fails with `Not connected`: that it got there is what
//! Karaka's spec checks with its spy on the wrapped `execute`.

use std::time::Duration;

use karaka_mcp_application::prepare_application_tool;
use karaka_mcp_client::{Client, ClientOptions, HttpParams, HttpTransport, McpTool};
use karaka_model::ToolCall;
use karaka_tools::ToolRegistry;
use serde_json::{Value, json};

fn tool(definition: Value) -> McpTool {
    let client = Client::new(
        Box::new(HttpTransport::new(HttpParams {
            url: "http://127.0.0.1:9/mcp".into(),
            headers: Vec::new(),
        })),
        ClientOptions::default(),
    );
    McpTool::new("app", &definition, client, Duration::from_secs(60))
}

fn parameters(tool: &dyn karaka_tools::Tool) -> Value {
    serde_json::from_str(&tool.schema().parameters).unwrap()
}

async fn invoke(tools: &ToolRegistry, arguments: &str) -> String {
    tools
        .execute(
            &ToolCall {
                id: "application-call".into(),
                name: "mcp__app__read".into(),
                arguments: arguments.into(),
            },
            None,
        )
        .await
        .content
}

#[test]
fn unsupported_input_schemas_are_refused() {
    for schema in [
        json!({ "type": "string" }),
        json!({ "type": "object", "patternProperties": {} }),
    ] {
        let error =
            prepare_application_tool(tool(json!({ "name": "read", "inputSchema": schema })))
                .err()
                .unwrap();
        assert!(error.starts_with("unsupported JSON schema: "), "{error}");
    }
}

#[tokio::test]
async fn the_dialect_goes_and_invalid_input_is_refused_before_the_call() {
    let prepared = prepare_application_tool(tool(json!({
        "name": "read",
        "inputSchema": {
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": { "id": { "type": "integer" } },
            "required": ["id"],
            "additionalProperties": false,
        },
    })))
    .unwrap();
    assert!(parameters(prepared.as_ref()).get("$schema").is_none());
    assert!(prepared.registry_checks_arguments());
    let tools = ToolRegistry::new();
    let _kept = tools.register(prepared).unwrap();
    for arguments in [
        "null",
        "\"item\"",
        "[]",
        r#"{"id":"wrong"}"#,
        r#"{"id":1,"unexpected":true}"#,
    ] {
        let content = invoke(&tools, arguments).await;
        assert!(
            content.starts_with("Error: invalid arguments: "),
            "{arguments}: {content}"
        );
    }
    assert_eq!(invoke(&tools, r#"{"id":1}"#).await, "Error: Not connected");
}

#[tokio::test]
async fn a_tool_needing_tasks_is_refused_before_its_input_is_read() {
    for support in ["required", "optional"] {
        let prepared = prepare_application_tool(tool(json!({
            "name": "read",
            "execution": { "taskSupport": support },
            "inputSchema": {
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "properties": { "id": { "type": "integer" } },
                "required": ["id"],
            },
        })))
        .unwrap();
        assert!(parameters(prepared.as_ref()).get("$schema").is_none());
        let tools = ToolRegistry::new();
        let _kept = tools.register(prepared).unwrap();
        let content = invoke(&tools, "{}").await;
        if support == "required" {
            assert!(
                content.contains("requires task-based execution"),
                "{content}"
            );
        } else {
            assert!(
                content.starts_with("Error: invalid arguments: "),
                "{content}"
            );
        }
    }
}
