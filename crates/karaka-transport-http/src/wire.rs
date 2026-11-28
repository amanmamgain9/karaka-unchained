//! The wire: the request bodies the routes take, checked as `@karaka-ai/sdk`'s
//! strict Zod schemas check them, and the session events they send, as
//! `projectWireEvent` projects them. (Karaka: `@karaka-ai/sdk`
//! `protocol.ts`, and `transport-http/src/index.ts`.)
//!
//! A body that fails a schema is `BAD_REQUEST`, which the routes expose as
//! `Invalid request`, as a `ZodError` is exposed, so the checks only need
//! to agree on what passes.

use karaka_session::Entry;
use serde_json::{Map, Value, json};

use crate::error::AppError;

/// The default route prefix. (sdk: `KARAKA_APPLICATION_API_PATH`.)
pub const KARAKA_APPLICATION_API_PATH: &str = "/v1";

/// The image types a prompt may carry. (sdk: `ApplicationChatContentSchema`.)
const IMAGE_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/webp", "image/gif"];

fn invalid() -> AppError {
    AppError::bad_request("request body does not match its schema")
}

/// A mapping with only `keys`: `z.strictObject`.
fn strict<'a>(value: &'a Value, keys: &[&str]) -> Result<&'a Map<String, Value>, AppError> {
    let Value::Object(map) = value else {
        return Err(invalid());
    };
    if map.keys().any(|key| !keys.contains(&key.as_str())) {
        return Err(invalid());
    }
    Ok(map)
}

/// A non-empty string: `z.string().min(1)`.
fn text(map: &Map<String, Value>, key: &str) -> Result<String, AppError> {
    match map.get(key) {
        Some(Value::String(text)) if !text.is_empty() => Ok(text.clone()),
        _ => Err(invalid()),
    }
}

/// An optional non-empty string: absent, or `z.string().min(1)`.
fn optional_text(map: &Map<String, Value>, key: &str) -> Result<Option<String>, AppError> {
    match map.get(key) {
        None => Ok(None),
        Some(_) => text(map, key).map(Some),
    }
}

/// The trusted tenant and user an application server names. (sdk:
/// `ApplicationIdentitySchema`.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    pub tenant_id: String,
    pub user_id: String,
}

const IDENTITY: [&str; 2] = ["tenantId", "userId"];

fn identity(map: &Map<String, Value>) -> Result<Identity, AppError> {
    Ok(Identity {
        tenant_id: text(map, "tenantId")?,
        user_id: text(map, "userId")?,
    })
}

fn keys(extra: &[&'static str]) -> Vec<&'static str> {
    IDENTITY.iter().chain(extra).copied().collect()
}

/// (sdk: `ApplicationCreateChatRequestSchema`.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreateChat {
    pub identity: Identity,
    pub agent_id: String,
    pub chat_id: String,
}

impl CreateChat {
    /// # Errors
    ///
    /// The schema refuses it.
    pub fn parse(value: &Value) -> Result<Self, AppError> {
        let map = strict(value, &keys(&["agentId", "chatId"]))?;
        Ok(Self {
            identity: identity(map)?,
            agent_id: text(map, "agentId")?,
            chat_id: text(map, "chatId")?,
        })
    }
}

/// One part of a prompt. (sdk: `ApplicationChatContentSchema`.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Content {
    Text(String),
    Image {
        media_type: String,
        data: String,
        name: Option<String>,
    },
}

impl Content {
    fn parse(value: &Value) -> Result<Self, AppError> {
        match value.get("type").and_then(Value::as_str) {
            Some("text") => {
                let map = strict(value, &["type", "text"])?;
                Ok(Self::Text(text(map, "text")?))
            }
            Some("image") => {
                let map = strict(value, &["type", "mediaType", "data", "name"])?;
                let media_type = match map.get("mediaType") {
                    Some(Value::String(kind)) if IMAGE_TYPES.contains(&kind.as_str()) => {
                        kind.clone()
                    }
                    _ => return Err(invalid()),
                };
                Ok(Self::Image {
                    media_type,
                    data: text(map, "data")?,
                    name: optional_text(map, "name")?,
                })
            }
            _ => Err(invalid()),
        }
    }
}

/// (sdk: `ApplicationPromptRequestSchema`.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Prompt {
    pub identity: Identity,
    pub request_id: String,
    pub content: Vec<Content>,
}

impl Prompt {
    /// # Errors
    ///
    /// The schema refuses it.
    pub fn parse(value: &Value) -> Result<Self, AppError> {
        let map = strict(value, &keys(&["requestId", "content"]))?;
        let content = match map.get("content") {
            Some(Value::Array(parts)) if !parts.is_empty() => parts
                .iter()
                .map(Content::parse)
                .collect::<Result<Vec<_>, _>>()?,
            _ => return Err(invalid()),
        };
        Ok(Self {
            identity: identity(map)?,
            request_id: text(map, "requestId")?,
            content,
        })
    }
}

/// (sdk: `ApplicationAddressRequestSchema`.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Address {
    pub identity: Identity,
    pub cursor: Option<i64>,
}

/// The largest integer `z.number().int()` takes, `Number.MAX_SAFE_INTEGER`.
const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

impl Address {
    /// # Errors
    ///
    /// The schema refuses it.
    pub fn parse(value: &Value) -> Result<Self, AppError> {
        let map = strict(value, &keys(&["cursor"]))?;
        let cursor = match map.get("cursor") {
            None => None,
            Some(Value::Number(number)) => {
                let float = number.as_f64().ok_or_else(invalid)?;
                if float.fract() != 0.0 || float < -1.0 || float > MAX_SAFE_INTEGER {
                    return Err(invalid());
                }
                Some(float as i64)
            }
            Some(_) => return Err(invalid()),
        };
        Ok(Self {
            identity: identity(map)?,
            cursor,
        })
    }
}

/// (sdk: `ApplicationModelRequestSchema`.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelRequest {
    pub identity: Identity,
    pub provider: String,
    pub model: String,
    pub reasoning_effort: Option<String>,
}

impl ModelRequest {
    /// # Errors
    ///
    /// The schema refuses it.
    pub fn parse(value: &Value) -> Result<Self, AppError> {
        let map = strict(value, &keys(&["provider", "model", "reasoningEffort"]))?;
        Ok(Self {
            identity: identity(map)?,
            provider: text(map, "provider")?,
            model: text(map, "model")?,
            reasoning_effort: optional_text(map, "reasoningEffort")?,
        })
    }
}

/// One answer. (sdk: `ApplicationRespondRequestSchema`'s items.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnswerItem {
    pub id: String,
    pub selected: Vec<String>,
    pub custom: Option<String>,
}

/// (sdk: `ApplicationRespondRequestSchema`.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Respond {
    pub identity: Identity,
    pub interaction_id: String,
    pub answers: Vec<AnswerItem>,
}

impl Respond {
    /// # Errors
    ///
    /// The schema refuses it.
    pub fn parse(value: &Value) -> Result<Self, AppError> {
        let map = strict(value, &keys(&["interactionId", "answers"]))?;
        let answers = strict(map.get("answers").ok_or_else(invalid)?, &["answers"])?;
        let Some(Value::Array(items)) = answers.get("answers") else {
            return Err(invalid());
        };
        let answers = items
            .iter()
            .map(|item| {
                let item = strict(item, &["id", "selected", "custom"])?;
                let selected = match item.get("selected") {
                    Some(Value::Array(labels)) => labels
                        .iter()
                        .map(|label| label.as_str().map(str::to_owned).ok_or_else(invalid))
                        .collect::<Result<Vec<_>, _>>()?,
                    _ => return Err(invalid()),
                };
                let custom = match item.get("custom") {
                    None => None,
                    Some(Value::String(custom)) => Some(custom.clone()),
                    Some(_) => return Err(invalid()),
                };
                Ok(AnswerItem {
                    id: text(item, "id")?,
                    selected,
                    custom,
                })
            })
            .collect::<Result<Vec<_>, AppError>>()?;
        Ok(Self {
            identity: identity(map)?,
            interaction_id: text(map, "interactionId")?,
            answers,
        })
    }
}

/// A logged event in its stored JSON. (dsh v4's envelope.)
fn envelope(entry: &Entry) -> Value {
    serde_json::to_value(entry).unwrap_or(Value::Null)
}

/// The wire events of one session event: none for those with no place in
/// an application's transcript. (Karaka: `projectWireEvent`.)
pub fn project(entry: &Entry) -> Vec<Value> {
    let event = envelope(entry);
    let cursor = entry.seq.0;
    let data = &event["data"];
    let projected = match event["type"].as_str() {
        Some("user/message") if data["source"]["kind"] == "user" => {
            json!({ "type": "user-message", "cursor": cursor, "content": data })
        }
        Some("assistant/message") => {
            json!({ "type": "assistant-message", "cursor": cursor, "content": data["message"] })
        }
        Some("tool/call") => json!({
            "type": "tool-call",
            "cursor": cursor,
            "callId": data["callId"],
            "name": data["name"],
            "arguments": data["arguments"],
        }),
        Some("tool/result") => json!({
            "type": "tool-result",
            "cursor": cursor,
            "callId": data["message"]["source"]["callId"],
            "content": data["message"],
        }),
        Some("turn/end") => {
            json!({ "type": "turn-end", "cursor": cursor, "reason": data["reason"] })
        }
        _ => return Vec::new(),
    };
    vec![projected]
}

/// One SSE frame of `event`. (Karaka: `writeJsonEvent`.)
pub fn sse(event: &Value) -> String {
    format!("data: {event}\n\n")
}
