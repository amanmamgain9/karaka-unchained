//! The tool bridge: a server's tools as Karaka tools, under server-qualified
//! public names, synced per connection generation. (dsh:
//! `mcp-client/src/tools.ts`: `publicToolName`, `syncTools`,
//! `createMcpToolDefinition` and its result projection.)
//!
//! A tool's identity is `(serverName, rawName)`. Its public name, what the
//! model sees, is `mcp__<serverName>__<rawName>`, normalized to the
//! function-name rules; the raw name is only sent on the wire, in
//! `tools/call`.
//!
//! # What the model reads
//!
//! dsh keeps a call's canonical result, `{ content, structuredContent? }`,
//! for programmatic callers, and gives the model a text projection of
//! `content`, with images admitted as attachments where a store and an
//! image-capable route exist. Karaka's results are text: the projection,
//! with every image as dsh's text diagnostic for an image that wasn't
//! admitted. `structuredContent` is checked against the tool's output
//! schema, then not shown, as dsh's text projection doesn't show it.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use karaka_model::ToolSchema;
use karaka_scope::ScopeKey;
use karaka_tools::json_schema::{check_schema, validate};
use karaka_tools::{Tool, ToolFuture, ToolOutput, ToolRegistration, ToolRegistry, ToolRun};
use serde_json::{Map, Value};

use crate::client::{Client, RequestOptions};
use crate::connection::{ConnectionObserver, Level, SyncFuture};
use crate::protocol::McpError;

/// The longest public name. (dsh: `MAX_PUBLIC_NAME_LENGTH`, the DeepSeek
/// function-name contract.)
const MAX_PUBLIC_NAME_LENGTH: usize = 64;

/// The identity hash's hex digits. (dsh: `HASH_LENGTH`.)
const HASH_LENGTH: usize = 12;

/// The model-facing name of the tool `raw_name` of `server_name`:
/// `mcp__<server>__<raw>`, or when replacing characters outside
/// `[A-Za-z0-9_-]` or cutting to 64 changed it, the normalized name cut to
/// 51, `_`, and 12 hex digits of SHA-256 over `server\0raw`, so distinct
/// identities never share a name. (dsh: `publicToolName`.)
pub fn public_tool_name(server_name: &str, raw_name: &str) -> String {
    let joined = format!("mcp__{server_name}__{raw_name}");
    // JavaScript replaces each UTF-16 unit, so a character outside the
    // basic plane becomes two underscores.
    let mut normalized = String::with_capacity(joined.len());
    for character in joined.chars() {
        if character.is_ascii_alphanumeric() || character == '_' || character == '-' {
            normalized.push(character);
        } else {
            for _ in 0..character.len_utf16() {
                normalized.push('_');
            }
        }
    }
    if normalized == joined && normalized.len() <= MAX_PUBLIC_NAME_LENGTH {
        return normalized;
    }
    let digest = ring::digest::digest(
        &ring::digest::SHA256,
        format!("{server_name}\0{raw_name}").as_bytes(),
    );
    let hash: String = digest
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()
        .chars()
        .take(HASH_LENGTH)
        .collect();
    let keep = MAX_PUBLIC_NAME_LENGTH - HASH_LENGTH - 1;
    let cut: String = normalized.chars().take(keep).collect();
    format!("{cut}_{hash}")
}

/// One server tool, as a Karaka tool. (dsh: `createMcpToolDefinition`,
/// with the `call` `syncTools` gives it.)
pub struct McpTool {
    public_name: String,
    raw_name: String,
    description: String,
    input_schema: Value,
    /// The advertised output schema, if any.
    output_schema: Option<Value>,
    /// Whether the tool needs the task extension, which isn't supported.
    task_required: bool,
    client: Client,
    timeout: Duration,
}

impl McpTool {
    /// The tool `definition`, from a `tools/list` page, called through
    /// `client` with `timeout`.
    pub fn new(server_name: &str, definition: &Value, client: Client, timeout: Duration) -> Self {
        let raw_name = definition["name"].as_str().unwrap_or_default().to_owned();
        Self {
            public_name: public_tool_name(server_name, &raw_name),
            description: definition["description"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            input_schema: definition
                .get("inputSchema")
                .cloned()
                .unwrap_or_else(|| Value::Object(Map::new())),
            output_schema: definition.get("outputSchema").cloned(),
            task_required: definition.pointer("/execution/taskSupport")
                == Some(&Value::from("required")),
            raw_name,
            client,
            timeout,
        }
    }

    pub fn public_name(&self) -> &str {
        &self.public_name
    }

    pub fn raw_name(&self) -> &str {
        &self.raw_name
    }

    async fn run(&self, arguments: Value) -> Result<String, String> {
        if self.task_required {
            return Err(format!(
                "Tool \"{}\" requires task-based execution, which this bridge does not support",
                self.raw_name
            ));
        }
        // A model's bare string, number or null becomes `{}`, so the server
        // can say what is missing.
        let arguments = if arguments.is_object() {
            arguments
        } else {
            Value::Object(Map::new())
        };
        let result = self
            .client
            .call_tool(
                &self.raw_name,
                arguments,
                RequestOptions {
                    timeout: Some(self.timeout),
                    cancel: None,
                },
            )
            .await
            .map_err(|error| error.message)?;
        if let Some(problem) = result_problem(&result) {
            return Err(format!(
                "Tool \"{}\" returned an invalid MCP result: {problem}",
                self.raw_name
            ));
        }
        let content = result
            .get("content")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let text = extract_text(&content, &self.raw_name);
        if result.get("isError") == Some(&Value::Bool(true)) {
            return Err(text);
        }
        // What the model reads of an image-bearing result: each image
        // refused, with the reason dsh's admission gives without an
        // attachment store.
        let text = if content.iter().any(|block| block["type"] == "image") {
            image_projection(&content, &self.raw_name)
        } else {
            text
        };
        if let Some(schema) = &self.output_schema {
            // The SDK's `callTool` check.
            let Some(structured) = result.get("structuredContent") else {
                return Err(format!(
                    "Tool {} has an output schema but did not return structured content",
                    self.raw_name
                ));
            };
            // dsh's own output check, for a schema in its subset; any other
            // vocabulary falls back to any JSON, as dsh's
            // `supportedOutputSchema` does.
            if check_schema(schema).is_ok() {
                let violations = validate(schema, structured, "value.structuredContent");
                if !violations.is_empty() {
                    return Err(format!(
                        "tool \"{}\" returned invalid output: {}",
                        self.public_name,
                        violations.join("; ")
                    ));
                }
            }
        }
        Ok(text)
    }
}

impl Tool for McpTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.public_name.clone(),
            description: self.description.clone(),
            parameters: self.input_schema.to_string(),
        }
    }

    /// The server's schema goes to the model as the server wrote it, and
    /// the server checks the arguments, as dsh's MCP definitions are raw,
    /// unchecked `ToolDefinition`s.
    fn registry_checks_arguments(&self) -> bool {
        false
    }

    /// Dropping the call cancels it: the client tells the server with
    /// `notifications/cancelled`.
    fn execute<'a>(&'a self, arguments: Value, _run: ToolRun) -> ToolFuture<'a> {
        Box::pin(async move {
            match self.run(arguments).await {
                Ok(text) => ToolOutput::ok(text),
                // dsh's tool layer shows a thrown error as `Error: <message>`.
                Err(message) => ToolOutput::error(format!("Error: {message}")),
            }
        })
    }
}

/// Why `result` isn't a `CallToolResult`, if it isn't. (dsh: the SDK's
/// `CallToolResult` schema, which it validates before projecting.)
fn result_problem(result: &Value) -> Option<String> {
    let Some(object) = result.as_object() else {
        return Some("expected an object".into());
    };
    let content = match object.get("content") {
        None => return None,
        Some(Value::Array(content)) => content,
        Some(_) => return Some("content: expected an array".into()),
    };
    for (index, block) in content.iter().enumerate() {
        let Some(block) = block.as_object() else {
            return Some(format!("content.{index}: expected an object"));
        };
        let has = |key: &str| block.get(key).is_some_and(Value::is_string);
        let fits = match block.get("type").and_then(Value::as_str) {
            Some("text") => has("text"),
            Some("image" | "audio") => has("data") && has("mimeType"),
            Some("resource_link") => has("name") && has("uri"),
            Some("resource") => block.get("resource").is_some_and(Value::is_object),
            _ => false,
        };
        if !fits {
            return Some(format!("content.{index}: not a content block"));
        }
    }
    None
}

/// The media types an image may be admitted as. (dsh: `IMAGE_MEDIA_TYPES`.)
const IMAGE_MEDIA_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/webp", "image/gif"];

/// Whether `data` is canonical RFC 4648 base64: no whitespace, no URL-safe
/// letters, padding only at the end, and unused bits zero. (dsh:
/// `CANONICAL_BASE64` and its round trip.)
fn canonical_base64(data: &str) -> bool {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = data.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return false;
    }
    let padding = bytes.iter().rev().take_while(|&&byte| byte == b'=').count();
    if padding > 2 {
        return false;
    }
    let body = &bytes[..bytes.len() - padding];
    let Some(values) = body
        .iter()
        .map(|byte| ALPHABET.iter().position(|letter| letter == byte))
        .collect::<Option<Vec<usize>>>()
    else {
        return false;
    };
    // The last character's bits beyond the data must be zero.
    match (padding, values.last()) {
        (1, Some(last)) => last & 0b11 == 0,
        (2, Some(last)) => last & 0b1111 == 0,
        _ => true,
    }
}

/// Why one image block can't be decoded, if it can't. (dsh: `decodeImage`.)
fn image_problem(block: &Map<String, Value>) -> Option<&'static str> {
    let media = block
        .get("mimeType")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !IMAGE_MEDIA_TYPES.contains(&media) {
        return Some("the declared media type is not PNG, JPEG, WebP, or GIF");
    }
    let data = block
        .get("data")
        .and_then(Value::as_str)
        .unwrap_or_default();
    (!canonical_base64(data)).then_some("the image data is not canonical base64")
}

/// An image-bearing result as the model reads it, without an attachment
/// store: every image refused, with its own decoding problem, or "another
/// image in the same result was invalid", or, when all decode, "no
/// attachment store is mounted". (dsh: `prepareImageProjection`, failing
/// at `resolveImageAdmission`.)
fn image_projection(content: &[Value], tool_name: &str) -> String {
    let problems: Vec<Option<&str>> = content
        .iter()
        .map(|value| {
            value
                .as_object()
                .filter(|block| block.get("type") == Some(&Value::from("image")))
                .and_then(image_problem)
        })
        .collect();
    let invalid = problems.iter().any(Option::is_some);
    project(content, tool_name, |block, index| {
        let reason = if invalid {
            problems[index].unwrap_or("another image in the same result was invalid")
        } else {
            "no attachment store is mounted"
        };
        image_diagnostic(block, reason)
    })
}

/// An image that wasn't admitted, as text. (dsh: `imageDiagnostic`.)
fn image_diagnostic(block: &Map<String, Value>, reason: &str) -> String {
    let media = block
        .get("mimeType")
        .and_then(Value::as_str)
        .unwrap_or("unknown media type");
    format!(
        "[image unavailable: {media}; {reason}; raw image data remains available to programmatic callers]"
    )
}

/// A result's content as the text the model reads. Text-like runs join
/// with `\n`; an image is dsh's text diagnostic for an image this result
/// couldn't admit, since Karaka's results hold no images. (dsh:
/// `extractText` and `projectContent` with the default image projector.)
pub fn extract_text(content: &[Value], tool_name: &str) -> String {
    project(content, tool_name, |block, _| {
        image_diagnostic(
            block,
            "this result was not admitted to durable model context",
        )
    })
}

/// The ordered projection, each image through `image`. (dsh:
/// `projectContent`.)
fn project(
    content: &[Value],
    tool_name: &str,
    image: impl Fn(&Map<String, Value>, usize) -> String,
) -> String {
    let mut blocks: Vec<String> = Vec::new();
    let mut text: Vec<String> = Vec::new();
    let flush = |text: &mut Vec<String>, blocks: &mut Vec<String>| {
        if !text.is_empty() {
            blocks.push(text.join("\n"));
            text.clear();
        }
    };
    for (index, value) in content.iter().enumerate() {
        let Some(block) = value.as_object() else {
            text.push("[unsupported MCP content block: expected an object]".into());
            continue;
        };
        let string = |key: &str| block.get(key).and_then(Value::as_str);
        match string("type").unwrap_or_default() {
            "text" => {
                if let Some(value) = string("text") {
                    text.push(value.to_owned());
                }
            }
            "image" => {
                flush(&mut text, &mut blocks);
                blocks.push(image(block, index));
            }
            "resource_link" => match (string("name"), string("uri")) {
                (Some(name), Some(uri)) => text.push(format!("Resource link: {name} ({uri})")),
                _ => text.push(
                    "[resource link unavailable: the MCP block is missing its name or URI]".into(),
                ),
            },
            "audio" => text.push(format!(
                "[audio result unsupported: {}; raw audio data remains available to programmatic callers]",
                string("mimeType").unwrap_or("unknown media type")
            )),
            "resource" => text.push(
                "[embedded resource unsupported; raw resource data remains available to programmatic callers]"
                    .into(),
            ),
            other => text.push(format!("[unsupported MCP content type: {other}]")),
        }
    }
    flush(&mut text, &mut blocks);
    if blocks.is_empty() {
        return format!("({tool_name} returned no model-visible content)");
    }
    blocks.join("\n")
}

/// Where a bridge's lines go.
pub type Log = Arc<dyn Fn(Level, &str) + Send + Sync>;

/// One server's tools in the registry: each generation's sync replaces the
/// previous one's, and giving up or disposal takes them all back. (dsh:
/// `syncTools` and the disposers the supervisor keeps.)
pub struct ToolBridge {
    server_name: String,
    scope: Option<ScopeKey>,
    tools: Arc<ToolRegistry>,
    timeout: Duration,
    registrations: Mutex<Vec<ToolRegistration>>,
    log: Log,
}

impl ToolBridge {
    pub fn new(
        server_name: impl Into<String>,
        scope: Option<ScopeKey>,
        tools: Arc<ToolRegistry>,
        timeout: Duration,
        log: Log,
    ) -> Self {
        Self {
            server_name: server_name.into(),
            scope,
            tools,
            timeout,
            registrations: Mutex::new(Vec::new()),
            log,
        }
    }

    /// The public names this server has registered now.
    pub fn registered(&self) -> Vec<String> {
        self.lock()
            .iter()
            .map(|registration| registration.name().to_owned())
            .collect()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<ToolRegistration>> {
        self.registrations
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// The two phases: build the next generation without touching the
    /// registry, failing on a raw name listed twice; then swap, rolling the
    /// whole generation back if a foreign tool sits on one of its names.
    async fn sync_tools(&self, client: &Client, strict: bool) -> Result<(), McpError> {
        let label = format!("mcp-client({})", self.server_name);
        let listed = if client.server_capabilities().get("tools").is_none() {
            Vec::new()
        } else {
            client.list_all_tools(RequestOptions::default()).await?
        };
        let mut next: Vec<McpTool> = Vec::new();
        for definition in &listed {
            let tool = McpTool::new(&self.server_name, definition, client.clone(), self.timeout);
            if next
                .iter()
                .any(|known| known.public_name == tool.public_name)
            {
                return Err(McpError::other(format!(
                    "{label}: server listed tool \"{}\" more than once — invalid tool list",
                    tool.raw_name
                )));
            }
            next.push(tool);
        }
        let mut registrations = self.lock();
        for previous in registrations.drain(..) {
            previous.remove();
        }
        let mut registered = Vec::new();
        for tool in next {
            match self.tools.register_in(self.scope.as_ref(), Arc::new(tool)) {
                Ok(registration) => registered.push(registration),
                Err(error) => {
                    for registration in registered {
                        registration.remove();
                    }
                    (self.log)(
                        Level::Error,
                        &format!(
                            "{label}: tool registration failed, no tools registered: Error: {error}"
                        ),
                    );
                    if strict {
                        return Err(McpError::other(error.to_string()));
                    }
                    return Ok(());
                }
            }
        }
        *registrations = registered;
        Ok(())
    }
}

impl ConnectionObserver for ToolBridge {
    fn sync<'a>(&'a self, client: &'a Client, strict: bool) -> SyncFuture<'a> {
        Box::pin(self.sync_tools(client, strict))
    }

    fn clear(&self) {
        for registration in self.lock().drain(..) {
            registration.remove();
        }
    }

    fn log(&self, level: Level, message: &str) {
        (self.log)(level, message);
    }
}
