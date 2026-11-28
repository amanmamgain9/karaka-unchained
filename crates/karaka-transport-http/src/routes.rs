//! The routes: the startup gate, server authentication, then each
//! operation, its body checked and its failure exposed in Karaka's fixed
//! words. (Karaka: `handleRequest`, `route` and `stream` in
//! `transport-http/src/index.ts`, and `http.ts`.)

use std::collections::HashSet;
use std::sync::Arc;

use bytes::Bytes;
use http::header::{AUTHORIZATION, CACHE_CONTROL, CONNECTION, CONTENT_TYPE};
use http::{HeaderValue, Method, StatusCode};
use http_body_util::BodyExt;
use karaka_agent::ModelSelection;
use karaka_identity::{ApplicationId, ApplicationOwner, TenantId, UserId};
use karaka_server_auth::ServerAuth;
use karaka_session::SessionId;
use karaka_startup::{KarakaStartup, STARTING_BODY, STARTING_CONTENT_TYPE};
use karaka_user_questions::{AskUserQuestionAnswer, AskUserQuestionAnswerItem};
use karaka_webserver::{Request, Response, body, pathname};
use serde_json::{Value, json};

use crate::controller::{Address, ApplicationChats, Follow, FollowFrame};
use crate::error::{AppError, expose};
use crate::hub::Hub;
use crate::questions::Interactions;
use crate::wire::{self, CreateChat, ModelRequest, Prompt, Respond, project, sse};

/// What the routes use.
pub struct Routes {
    pub base: String,
    pub max_body_bytes: usize,
    pub chats: Arc<ApplicationChats>,
    pub auth: Arc<dyn ServerAuth>,
    pub startup: Arc<KarakaStartup>,
    pub interactions: Arc<Interactions>,
    pub hub: Arc<Hub>,
}

/// A JSON response. (Karaka: `json`.)
fn json_response(status: u16, value: &Value) -> Response {
    let mut response = Response::new(body::full(value.to_string()));
    *response.status_mut() =
        StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static(STARTING_CONTENT_TYPE),
    );
    response
}

fn error_response(error: &AppError) -> Response {
    let exposed = expose(error);
    json_response(
        exposed.status,
        &json!({ "code": exposed.code, "message": exposed.message }),
    )
}

/// The body as one JSON object, at most `max` bytes. (Karaka:
/// `readObject`.)
async fn read_object<B>(body: B, max: usize) -> Result<Value, AppError>
where
    B: http_body::Body<Data = Bytes> + Unpin,
    B::Error: std::fmt::Display,
{
    let mut body = body;
    let mut bytes: Vec<u8> = Vec::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|error| AppError::internal(error.to_string()))?;
        if let Ok(data) = frame.into_data() {
            if bytes.len() + data.len() > max {
                return Err(AppError::bad_request(format!(
                    "request body exceeds {max} bytes"
                )));
            }
            bytes.extend_from_slice(&data);
        }
    }
    let text = String::from_utf8_lossy(&bytes);
    let parsed: Value =
        serde_json::from_str(&text).map_err(|error| AppError::bad_request(error.to_string()))?;
    if !parsed.is_object() {
        return Err(AppError::bad_request("request body must be an object"));
    }
    Ok(parsed)
}

/// `decodeURIComponent`: every `%XX` decoded, the result UTF-8.
fn decode_uri_component(text: &str) -> Result<String, AppError> {
    let malformed = || AppError::internal("URIError: URI malformed");
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = text.get(index + 1..index + 3).ok_or_else(malformed)?;
            let byte = u8::from_str_radix(hex, 16).map_err(|_| malformed())?;
            out.push(byte);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).map_err(|_| malformed())
}

fn owner(application: &ApplicationId, identity: &wire::Identity) -> ApplicationOwner {
    ApplicationOwner {
        application_id: application.clone(),
        tenant_id: TenantId::new(&identity.tenant_id),
        user_id: UserId::new(&identity.user_id),
    }
}

const OPERATIONS: [&str; 6] = [
    "messages",
    "stream",
    "history",
    "cancel",
    "model",
    "responses",
];

impl Routes {
    /// Answers one request under the route prefix. (Karaka: the prefix
    /// route's handler, then `handleRequest`.)
    pub async fn handle(self: Arc<Self>, request: Request) -> Response {
        if !self.startup.is_ready() {
            let mut response = Response::new(body::full(STARTING_BODY));
            *response.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
            response.headers_mut().insert(
                CONTENT_TYPE,
                HeaderValue::from_static(STARTING_CONTENT_TYPE),
            );
            return response;
        }
        let authorization = request
            .headers()
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let authenticated = match self.auth.authenticate(authorization.as_deref()).await {
            Ok(Some(authenticated)) => authenticated,
            Ok(None) => {
                return json_response(
                    401,
                    &json!({ "code": "UNAUTHORIZED", "message": "Invalid server credential" }),
                );
            }
            Err(message) => return error_response(&AppError::internal(message)),
        };
        match self.route(&authenticated.application_id, request).await {
            Ok(response) => response,
            Err(error) => error_response(&error),
        }
    }

    async fn route(
        &self,
        application: &ApplicationId,
        request: Request,
    ) -> Result<Response, AppError> {
        let path = pathname(request.uri());
        let relative = path.get(self.base.len()..).unwrap_or_default().to_owned();
        let method = request.method().clone();
        let chats = &self.chats;
        if method == Method::GET && relative == "/agents" {
            return Ok(json_response(200, &chats.list_agents()));
        }
        let max = self.max_body_bytes;
        if method == Method::POST && relative == "/chats" {
            let body = CreateChat::parse(&read_object(request.into_body(), max).await?)?;
            let address = Address {
                chat_id: SessionId::new(&body.chat_id),
                owner: owner(application, &body.identity),
            };
            return Ok(json_response(
                201,
                &chats.create(&address, &body.agent_id).await?,
            ));
        }
        let matched = relative.strip_prefix("/chats/").and_then(|rest| {
            let (chat, operation) = rest.split_once('/')?;
            (!chat.is_empty() && OPERATIONS.contains(&operation))
                .then(|| (chat.to_owned(), operation.to_owned()))
        });
        let (Some((chat, operation)), true) = (matched, method == Method::POST) else {
            return Ok(json_response(
                404,
                &json!({ "code": "NOT_FOUND", "message": "Route not found" }),
            ));
        };
        let chat_id = decode_uri_component(&chat)?;
        let chat = SessionId::new(&chat_id);
        let object = read_object(request.into_body(), max).await?;
        match operation.as_str() {
            "messages" => {
                let body = Prompt::parse(&object)?;
                let address = Address {
                    chat_id: chat,
                    owner: owner(application, &body.identity),
                };
                let result = chats
                    .prompt(&address, &body.request_id, &body.content)
                    .await?;
                Ok(json_response(
                    202,
                    &json!({
                        "chatId": chat_id,
                        "requestId": body.request_id,
                        "accepted": result["accepted"],
                        "duplicate": result["duplicate"],
                    }),
                ))
            }
            "history" => {
                let body = wire::Address::parse(&object)?;
                let address = Address {
                    chat_id: chat,
                    owner: owner(application, &body.identity),
                };
                let events: Vec<Value> = chats
                    .events(&address)
                    .await?
                    .iter()
                    .flat_map(project)
                    .collect();
                Ok(json_response(
                    200,
                    &json!({ "chatId": chat_id, "events": events }),
                ))
            }
            "cancel" => {
                let body = wire::Address::parse(&object)?;
                let address = Address {
                    chat_id: chat,
                    owner: owner(application, &body.identity),
                };
                Ok(json_response(200, &chats.cancel(&address).await?))
            }
            "model" => {
                let body = ModelRequest::parse(&object)?;
                let address = Address {
                    chat_id: chat,
                    owner: owner(application, &body.identity),
                };
                let requested = ModelSelection {
                    provider: body.provider,
                    model: body.model,
                    reasoning_effort: body.reasoning_effort,
                };
                Ok(json_response(
                    200,
                    &chats.select_model(&address, requested).await?,
                ))
            }
            "responses" => {
                let body = Respond::parse(&object)?;
                let owner = owner(application, &body.identity);
                let answer = AskUserQuestionAnswer {
                    answers: body
                        .answers
                        .into_iter()
                        .map(|item| AskUserQuestionAnswerItem {
                            id: item.id,
                            selected: item.selected,
                            custom: item.custom,
                        })
                        .collect(),
                };
                self.interactions
                    .respond(&chat, &owner, &body.interaction_id, answer)?;
                Ok(json_response(200, &json!({ "accepted": true })))
            }
            _ => {
                let body = wire::Address::parse(&object)?;
                let address = Address {
                    chat_id: chat,
                    owner: owner(application, &body.identity),
                };
                self.stream(address, body.cursor).await
            }
        }
    }

    /// The chat's SSE follow: the snapshot's events then its cursor, then
    /// each durable event and text delta, with interactions once the
    /// stream has passed their cursor. (Karaka: `stream`.)
    async fn stream(&self, address: Address, cursor: Option<i64>) -> Result<Response, AppError> {
        let follow = self.chats.follow(&address).await?;
        if cursor.is_some_and(|cursor| cursor > follow.cursor) {
            return Err(AppError::bad_request(
                "stream cursor is past the durable chat cursor",
            ));
        }
        let interactions = self.hub.interactions(&address.chat_id);
        let replay = self.interactions.replay(&address.chat_id, &address.owner);
        let (sender, stream_body) = body::channel(16);
        let mut response = Response::new(stream_body);
        let headers = response.headers_mut();
        headers.insert(
            CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream; charset=utf-8"),
        );
        headers.insert(
            CACHE_CONTROL,
            HeaderValue::from_static("no-cache, no-transform"),
        );
        headers.insert(CONNECTION, HeaderValue::from_static("keep-alive"));
        tokio::spawn(write_stream(follow, cursor, interactions, replay, sender));
        Ok(response)
    }
}

/// Writes the follow until it ends, the peer goes, or it fails, which ends
/// the stream with an error event. (Karaka: `stream`'s loop, with
/// `drainBufferedInteractions` and `writeFollowFrame`.)
async fn write_stream(
    mut follow: Follow,
    cursor: Option<i64>,
    mut interactions: crate::hub::Subscription<Value>,
    replay: Vec<Value>,
    sender: body::BodySender,
) {
    let send = |event: &Value| sender.send(Bytes::from(sse(event)));
    let after = |seq: i64| cursor.is_none_or(|cursor| seq > cursor);
    for record in &follow.records {
        if !after(record.seq.0 as i64) {
            continue;
        }
        for event in project(record) {
            if send(&event).await.is_err() {
                return;
            }
        }
    }
    if send(&json!({ "type": "snapshot", "cursor": follow.cursor }))
        .await
        .is_err()
    {
        return;
    }
    let mut durable = follow.cursor;
    let mut buffered: Vec<Value> = Vec::new();
    let mut queued: HashSet<String> = HashSet::new();
    let mut queue = |event: Value, buffered: &mut Vec<Value>| {
        let id = event["interactionId"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        if queued.insert(id) {
            buffered.push(event);
        }
    };
    for event in replay {
        queue(event, &mut buffered);
    }
    loop {
        if drain(&mut buffered, durable, &send).await.is_err() {
            return;
        }
        let frame = {
            let next = follow.next();
            tokio::pin!(next);
            loop {
                tokio::select! {
                    frame = &mut next => break frame,
                    Some(event) = interactions.recv() => {
                        queue(event, &mut buffered);
                        if drain(&mut buffered, durable, &send).await.is_err() {
                            return;
                        }
                    }
                    () = sender.closed() => return,
                }
            }
        };
        match frame {
            Ok(Some(FollowFrame::TextDelta { cursor: at, text })) => {
                let event = json!({ "type": "text-delta", "cursor": at, "text": text });
                if send(&event).await.is_err() {
                    return;
                }
            }
            Ok(Some(FollowFrame::Event(entry))) => {
                durable = entry.seq.0 as i64;
                if after(durable) {
                    for event in project(&entry) {
                        if send(&event).await.is_err() {
                            return;
                        }
                    }
                }
            }
            Ok(None) => return,
            Err(error) => {
                let exposed = expose(&error);
                let _ = send(
                    &json!({ "type": "error", "code": exposed.code, "message": exposed.message }),
                )
                .await;
                return;
            }
        }
    }
}

/// Sends each buffered interaction its stream has passed, oldest first.
/// (Karaka: `drainBufferedInteractions`.)
async fn drain<F, Fut>(
    buffered: &mut Vec<Value>,
    durable: i64,
    send: &F,
) -> Result<(), body::Closed>
where
    F: Fn(&Value) -> Fut,
    Fut: std::future::Future<Output = Result<(), body::Closed>>,
{
    while let Some(index) = buffered
        .iter()
        .position(|event| event["cursor"].as_i64().unwrap_or(i64::MAX) <= durable)
    {
        let event = buffered.remove(index);
        send(&event).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::decode_uri_component;

    #[test]
    fn a_path_segment_decodes_as_decode_uri_component_does() {
        assert_eq!(decode_uri_component("chat").unwrap(), "chat");
        assert_eq!(decode_uri_component("a%2Fb%20c").unwrap(), "a/b c");
        assert_eq!(decode_uri_component("%C3%A9").unwrap(), "é");
        for bad in ["%FF", "%zz", "%", "%2"] {
            assert!(decode_uri_component(bad).is_err(), "{bad}");
        }
    }
}
