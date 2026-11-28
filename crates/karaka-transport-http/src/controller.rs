//! The application operations over Karaka's agents, sessions and identity.
//! (Karaka: `ApplicationChatController`, `transport-http/src/application.ts`.)
//!
//! Every operation on one chat runs under the identity's chat lock. A
//! chat is created under the preset the application names, bound before
//! its agent opens, and acknowledged once durable; later operations
//! restore or authorize its owner first, and resume a cold chat with its
//! recorded preset.

use std::collections::HashSet;
use std::future::Future;
use std::sync::{Arc, Mutex, PoisonError};

use karaka_agent::{Agent, Agents, ModelSelection};
use karaka_agent_default_model::{AgentDefaultModel, list_providers, resolve_call_config};
use karaka_agent_presets::AgentPresets;
use karaka_browser_auth::BrowserCaller;
use karaka_identity::{ApplicationOwner, KarakaIdentity, Reservation, same_owner};
use karaka_model::ModelRegistry;
use karaka_session::{
    CancelCause, ContentBlock, Entry, Passthrough, Session, SessionEvent, SessionHeader, SessionId,
    SessionStore, UserMessage, UserRole, UserSource,
};
use karaka_session_persistence::DurableSessions;
use karaka_session_query::SessionQuery;
use serde_json::{Value, json};

use crate::error::AppError;
use crate::hub::{Hub, Live, Subscription};
use crate::ids::random_uuid;
use crate::state::application_state;
use crate::wire::Content;

/// A chat and its trusted owner, and the verified browser caller when a
/// browser asked. (Karaka: `Address`.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Address {
    pub chat_id: SessionId,
    pub owner: ApplicationOwner,
    pub browser: Option<BrowserCaller>,
}

impl Address {
    /// An application server's address, with no browser.
    pub fn new(chat_id: SessionId, owner: ApplicationOwner) -> Self {
        Self {
            chat_id,
            owner,
            browser: None,
        }
    }
}

/// The ports the operations use.
#[derive(Clone)]
pub struct ControllerPorts {
    pub identity: Arc<KarakaIdentity>,
    pub agents: Arc<dyn Agents>,
    pub store: Arc<SessionStore>,
    pub durable: Arc<DurableSessions>,
    pub query: Arc<SessionQuery>,
    /// `None` without presets: none are listed, and no chat can be made.
    pub presets: Option<Arc<AgentPresets>>,
    pub default_model: Arc<AgentDefaultModel>,
    pub models: Arc<ModelRegistry>,
}

/// (Karaka: `ApplicationChatController`.)
pub struct ApplicationChats {
    ports: ControllerPorts,
    hub: Arc<Hub>,
    /// The chats whose agents this controller opened. (Karaka: `handles`.)
    handles: Mutex<HashSet<SessionId>>,
    /// The presets a browser may reach. (Karaka: `browserAgentIds`.)
    browser_presets: Mutex<Vec<String>>,
}

/// What a follow yields after its snapshot. (Karaka: `FollowFrame`'s
/// `event` and `text-delta`.)
#[derive(Clone, Debug, PartialEq)]
pub enum FollowFrame {
    Event(Box<Entry>),
    TextDelta { cursor: i64, text: String },
}

/// A chat's history, then its live items. (Karaka: `follow`.)
pub struct Follow {
    /// The chat's header when the snapshot was read.
    pub header: SessionHeader,
    /// The last durable seq in the snapshot, or -1.
    pub cursor: i64,
    /// The snapshot's events.
    pub records: Vec<Entry>,
    chats: Arc<ApplicationChats>,
    address: Address,
    live: Subscription<Live>,
}

impl Follow {
    /// The next frame, `None` once nothing more can come.
    ///
    /// # Errors
    ///
    /// The chat's owner changed (`forbidden`), or the stream lost an event:
    /// `Application event stream lost sequence continuity`.
    pub async fn next(&mut self) -> Result<Option<FollowFrame>, AppError> {
        loop {
            let Some(item) = self.live.recv().await else {
                return Ok(None);
            };
            match item {
                Live::TextDelta { cursor, text } => {
                    if cursor < self.cursor {
                        continue;
                    }
                    self.authorize().await?;
                    return Ok(Some(FollowFrame::TextDelta { cursor, text }));
                }
                Live::Event(entry) => {
                    let seq = entry.seq.0 as i64;
                    if seq <= self.cursor {
                        continue;
                    }
                    if seq != self.cursor + 1 {
                        return Err(AppError::internal(
                            "Application event stream lost sequence continuity",
                        ));
                    }
                    self.authorize().await?;
                    self.cursor = seq;
                    return Ok(Some(FollowFrame::Event(entry)));
                }
            }
        }
    }

    async fn authorize(&self) -> Result<(), AppError> {
        self.chats
            .ports
            .identity
            .authorize(&self.address.chat_id, &self.address.owner)
            .await?;
        Ok(())
    }
}

fn selection_json(selected: &ModelSelection) -> Value {
    let mut value = json!({ "provider": selected.provider, "model": selected.model });
    if let Some(effort) = &selected.reasoning_effort {
        value["reasoningEffort"] = json!(effort);
    }
    value
}

impl ApplicationChats {
    pub fn new(ports: ControllerPorts, hub: Arc<Hub>) -> Arc<Self> {
        Arc::new(Self {
            ports,
            hub,
            handles: Mutex::default(),
            browser_presets: Mutex::default(),
        })
    }

    /// Runs `operation` under chat `id`'s lock. (Karaka: `run`.)
    async fn run<T>(
        &self,
        id: &SessionId,
        operation: impl Future<Output = Result<T, AppError>>,
    ) -> Result<T, AppError> {
        self.ports.identity.with_chat_lock(id, operation).await?
    }

    /// The presets a browser may use; an empty list without browser
    /// access.
    pub fn set_browser_presets(&self, presets: Vec<String>) {
        *self
            .browser_presets
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = presets;
    }

    /// Checks a browser caller's signed chat and owner, and that the chat
    /// is a root of an allowed preset, without recording ownership.
    /// (Karaka: `authorizeBrowser`.)
    ///
    /// # Errors
    ///
    /// `CHAT_FORBIDDEN`, in Karaka's words, or `session/conflict` for a
    /// chat under another preset than `agent_id`.
    pub async fn authorize_browser(
        &self,
        address: &Address,
        agent_id: Option<&str>,
        caller: &BrowserCaller,
    ) -> Result<Option<SessionHeader>, AppError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_millis() as u64);
        let forbidden = |message: &str| AppError::coded("CHAT_FORBIDDEN", message);
        if caller.expires_at <= now
            || caller.chat_id != address.chat_id
            || !same_owner(&caller.owner, &address.owner)
        {
            return Err(forbidden("Browser credential does not authorize this chat"));
        }
        if let Some(owner) = self.ports.identity.owner_of_id(&address.chat_id).await?
            && !same_owner(&owner, &address.owner)
        {
            return Err(forbidden("Chat is owned by another caller"));
        }
        let allowed_presets = self
            .browser_presets
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let allowed =
            |id: Option<&str>| id.is_some_and(|id| allowed_presets.iter().any(|known| known == id));
        if agent_id.is_some() && !allowed(agent_id) {
            return Err(forbidden("Browser Agent preset is unavailable"));
        }
        let stored = self
            .ports
            .durable
            .persistence()
            .stat(&address.chat_id)
            .await
            .map_err(|error| AppError::internal(error.to_string()))?
            .map(|snapshot| snapshot.header);
        let live = self
            .ports
            .store
            .get(&address.chat_id)
            .map(|session| session.header().clone());
        for header in [&stored, &live].into_iter().flatten() {
            if header.parent_session.is_some() || !allowed(header.agent_preset.as_deref()) {
                return Err(forbidden("Browser target is not an allowed root chat"));
            }
            if agent_id.is_some() && header.agent_preset.as_deref() != agent_id {
                return Err(conflict());
            }
        }
        Ok(stored.or(live))
    }

    /// The usable presets: id, name (the id when it has none) and
    /// description. (Karaka: `listAgents`.)
    pub fn list_agents(&self) -> Value {
        let Some(presets) = &self.ports.presets else {
            return json!([]);
        };
        Value::Array(
            presets
                .list()
                .into_iter()
                .filter(|preset| preset.broken.is_none())
                .map(|preset| {
                    let mut row = json!({
                        "id": preset.id,
                        "name": preset.name.clone().unwrap_or_else(|| preset.id.clone()),
                    });
                    if let Some(description) = preset.description {
                        row["description"] = json!(description);
                    }
                    row
                })
                .collect(),
        )
    }

    /// The header stored, else live. (Karaka: `headerOf`.)
    async fn header_of(&self, id: &SessionId) -> Result<Option<SessionHeader>, AppError> {
        let stored = self
            .ports
            .durable
            .persistence()
            .stat(id)
            .await
            .map_err(|error| AppError::internal(error.to_string()))?;
        Ok(stored.map(|snapshot| snapshot.header).or_else(|| {
            self.ports
                .store
                .get(id)
                .map(|session| session.header().clone())
        }))
    }

    /// Authorizes a chat with a known owner, or restores a stored one to
    /// this owner; a new chat passes. (Karaka: `prepare`.)
    async fn prepare(&self, address: &Address, agent_id: Option<&str>) -> Result<(), AppError> {
        if let Some(caller) = &address.browser {
            self.authorize_browser(address, agent_id, caller).await?;
        }
        let identity = &self.ports.identity;
        if identity.owner_of_id(&address.chat_id).await?.is_some() {
            identity.authorize(&address.chat_id, &address.owner).await?;
            return Ok(());
        }
        let Some(header) = self.header_of(&address.chat_id).await? else {
            return Ok(());
        };
        if let Some(agent_id) = agent_id
            && header.agent_preset.as_deref() != Some(agent_id)
        {
            return Err(conflict());
        }
        identity.restore(&address.chat_id, &address.owner).await?;
        Ok(())
    }

    fn session(&self, id: &SessionId) -> Result<Session, AppError> {
        self.ports
            .store
            .get(id)
            .ok_or_else(|| AppError::internal("the chat's session isn't live"))
    }

    fn open_agent(&self, id: &SessionId) -> Result<Arc<dyn Agent>, AppError> {
        let agent = self
            .ports
            .agents
            .open(id.clone())
            .map_err(|error| match error {
                karaka_agent::OpenError::Compose(error) => {
                    AppError::coded(error.code, error.message)
                }
                other => AppError::internal(other.to_string()),
            })?;
        self.handles
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id.clone());
        self.install_selection(&agent)?;
        Ok(agent)
    }

    /// The agent's selection: the pending one, else the last used, else
    /// the default; one naming no registered route falls to the default.
    /// (Karaka: `installSelection`.)
    fn install_selection(&self, agent: &Arc<dyn Agent>) -> Result<(), AppError> {
        let state = application_state(&self.session(agent.id())?.events());
        let routes = list_providers(&self.ports.models);
        let selection = [state.pending, state.last_used]
            .into_iter()
            .flatten()
            .find(|selection| routes.contains(&selection.provider))
            .unwrap_or_else(|| self.ports.default_model.current_selection());
        agent.select_model(selection);
        Ok(())
    }

    /// The chat's agent, live or resumed with its recorded preset. (Karaka:
    /// `activate`.)
    async fn activate(&self, address: &Address) -> Result<Arc<dyn Agent>, AppError> {
        self.prepare(address, None).await?;
        let identity = &self.ports.identity;
        let header = identity.authorize(&address.chat_id, &address.owner).await?;
        if let Some(live) = self.ports.agents.get(&address.chat_id) {
            identity
                .authorize_session(&self.session(&address.chat_id)?, &address.owner)
                .await?;
            let ours = self
                .handles
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .contains(&address.chat_id);
            if !ours {
                return Err(AppError::coded(
                    "session/conflict",
                    "Agent is owned by another controller",
                ));
            }
            return Ok(live);
        }
        if self.ports.presets.is_none() || header.agent_preset.is_none() {
            return Err(AppError::internal(
                "Application Session has no resolvable Agent preset",
            ));
        }
        let session = self
            .ports
            .durable
            .open(address.chat_id.clone(), None)
            .await
            .map_err(|error| AppError::internal(error.to_string()))?;
        identity.authorize_session(&session, &address.owner).await?;
        self.open_agent(&address.chat_id)
    }

    /// Creates or reopens a chat, acknowledged once durable. (Karaka:
    /// `create`.)
    ///
    /// # Errors
    ///
    /// Another preset already (`session/conflict`), another owner, an
    /// unknown preset, or no presets.
    pub async fn create(&self, address: &Address, agent_id: &str) -> Result<Value, AppError> {
        self.run(&address.chat_id, async {
            self.prepare(address, Some(agent_id)).await?;
            if let Some(header) = self.header_of(&address.chat_id).await?
                && header.agent_preset.as_deref() != Some(agent_id)
            {
                return Err(conflict());
            }
            let identity = &self.ports.identity;
            let reserved = identity.reserve(&address.chat_id, &address.owner).await?;
            let agent = if reserved == Reservation::Existing {
                self.activate(address).await?
            } else {
                let Some(presets) = &self.ports.presets else {
                    return Err(AppError::internal(
                        "Application creation requires Agent presets",
                    ));
                };
                let preset = presets
                    .usable(Some(agent_id))
                    .map_err(|error| match error.code() {
                        Some(code) => AppError::coded(code, error.to_string()),
                        None => AppError::internal(error.to_string()),
                    })?
                    .to_owned();
                let session = self
                    .ports
                    .durable
                    .open_with(address.chat_id.clone(), None, |mut header| {
                        header.agent_preset = Some(preset);
                        header
                    })
                    .await
                    .map_err(|error| AppError::internal(error.to_string()))?;
                identity.bind(&session, &address.owner).await?;
                self.open_agent(&address.chat_id)?
            };
            identity
                .mark_ready(&self.session(agent.id())?, &address.owner)
                .await?;
            Ok(json!({ "chatId": address.chat_id.as_str(), "agentId": agent_id }))
        })
        .await
    }

    /// Admits a prompt once per request id, acknowledged once durable.
    /// (Karaka: `prompt`.)
    ///
    /// # Errors
    ///
    /// The selected provider has no model (`session/model-unavailable`), or
    /// the prompt has an image, which Karaka can't store yet
    /// (`session/attachment-invalid`).
    pub async fn prompt(
        &self,
        address: &Address,
        request_id: &str,
        content: &[Content],
    ) -> Result<Value, AppError> {
        self.run(&address.chat_id, async {
            let agent = self.activate(address).await?;
            let session = self.session(agent.id())?;
            let duplicate = application_state(&session.events())
                .request_ids
                .iter()
                .any(|id| id == request_id);
            if !duplicate {
                let selection = agent
                    .model_selection()
                    .unwrap_or_else(|| self.ports.default_model.current_selection());
                if !list_providers(&self.ports.models).contains(&selection.provider) {
                    return Err(AppError::coded(
                        "session/model-unavailable",
                        "Selected model provider is unavailable",
                    ));
                }
                if content
                    .iter()
                    .any(|part| matches!(part, Content::Image { .. }))
                {
                    return Err(AppError::coded(
                        "session/attachment-invalid",
                        "Image attachments are unavailable",
                    ));
                }
                let content = content
                    .iter()
                    .filter_map(|part| match part {
                        Content::Text(text) => Some(ContentBlock::text(text.clone())),
                        Content::Image { .. } => None,
                    })
                    .collect();
                let message = UserMessage {
                    id: random_uuid(),
                    role: UserRole::User,
                    content,
                    source: UserSource::prompt(request_id),
                    extra: Default::default(),
                };
                agent
                    .followup(message)
                    .map_err(|error| AppError::internal(error.to_string()))?;
            }
            self.ports
                .identity
                .mark_ready(&session, &address.owner)
                .await?;
            Ok(json!({ "accepted": true, "duplicate": duplicate }))
        })
        .await
    }

    /// Stops the running turn and keeps the queue. (Karaka: `cancel`.)
    ///
    /// # Errors
    ///
    /// As `activate`.
    pub async fn cancel(&self, address: &Address) -> Result<Value, AppError> {
        self.run(&address.chat_id, async {
            let agent = self.activate(address).await?;
            agent.interrupt(CancelCause::User);
            Ok(json!({ "accepted": true }))
        })
        .await
    }

    /// Checks and records a model selection, which the agent's next
    /// requests use. (Karaka: `selectModel`.)
    ///
    /// # Errors
    ///
    /// The provider has no model, or the effort isn't one Karaka sends.
    pub async fn select_model(
        &self,
        address: &Address,
        requested: ModelSelection,
    ) -> Result<Value, AppError> {
        self.run(&address.chat_id, async {
            let agent = self.activate(address).await?;
            let selected = resolve_call_config(&self.ports.models, requested)
                .map_err(|error| AppError::coded(error.code, error.message))?;
            let session = self.session(agent.id())?;
            session
                .append(SessionEvent::Passthrough(Passthrough {
                    event_type: "model/selection".into(),
                    data: selection_json(&selected),
                    surface_op: None,
                    source_event_seqs: None,
                }))
                .map_err(|error| AppError::internal(error.to_string()))?;
            if agent.model_selection().is_none() {
                return Err(AppError::internal(
                    "Application Agent has no model selection",
                ));
            }
            agent.select_model(selected.clone());
            self.ports
                .identity
                .mark_ready(&session, &address.owner)
                .await?;
            Ok(json!({ "selected": selection_json(&selected) }))
        })
        .await
    }

    /// The chat's durable events, once its owner is authorized. (Karaka:
    /// `events`.)
    ///
    /// # Errors
    ///
    /// Another owner, or a chat that doesn't exist.
    pub async fn events(&self, address: &Address) -> Result<Vec<Entry>, AppError> {
        self.run(&address.chat_id, self.prepare(address, None))
            .await?;
        self.ports
            .identity
            .authorize(&address.chat_id, &address.owner)
            .await?;
        let observed = self
            .ports
            .query
            .observe_session(&address.chat_id)
            .await
            .map_err(|error| AppError::coded(error.code, error.message))?;
        Ok(observed.events)
    }

    /// Subscribes, then reads the snapshot, so every later event is
    /// delivered once. (Karaka: `follow`.)
    ///
    /// # Errors
    ///
    /// As [`events`](Self::events).
    pub async fn follow(self: &Arc<Self>, address: &Address) -> Result<Follow, AppError> {
        let live = self.hub.follow(&address.chat_id);
        self.run(&address.chat_id, self.prepare(address, None))
            .await?;
        let header = self
            .ports
            .identity
            .authorize(&address.chat_id, &address.owner)
            .await?;
        let records = self.events(address).await?;
        let cursor = records.last().map_or(-1, |entry| entry.seq.0 as i64);
        Ok(Follow {
            header,
            cursor,
            records,
            chats: Arc::clone(self),
            address: address.clone(),
            live,
        })
    }
}

fn conflict() -> AppError {
    AppError::coded("session/conflict", "Chat already uses another Agent preset")
}
