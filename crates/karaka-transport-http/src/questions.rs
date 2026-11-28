//! The transport as a question answerer: a question from an owned chat's
//! agent waits for the application to answer it through `responses`,
//! offered on the chat's open streams, and replayed to streams opened
//! while it waits. (Karaka: the `user-questions/request` listener and
//! `pending` in `transport-http/src/index.ts`.)

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use karaka_identity::{ApplicationOwner, KarakaIdentity, same_owner};
use karaka_session::{SessionId, SessionStore};
use karaka_user_questions::{
    AnswerFuture, AnswerNext, AskUserQuestionAnswer, QuestionAnswerer, QuestionError,
    QuestionRequest,
};
use serde_json::{Value, json};
use tokio::sync::oneshot;

use crate::error::AppError;
use crate::hub::Hub;
use crate::ids::random_uuid;

struct Pending {
    id: String,
    chat: SessionId,
    owner: ApplicationOwner,
    questions: Value,
    cursor: i64,
    resolve: Option<oneshot::Sender<AskUserQuestionAnswer>>,
}

impl Pending {
    fn event(&self) -> Value {
        json!({
            "type": "interaction-required",
            "cursor": self.cursor,
            "interactionId": self.id,
            "questions": self.questions,
        })
    }
}

/// The interactions waiting on an application's answer, in the order they
/// came. (Karaka: `pending`.)
pub struct Interactions {
    pending: Mutex<Vec<Pending>>,
    hub: Arc<Hub>,
}

impl Interactions {
    pub fn new(hub: Arc<Hub>) -> Arc<Self> {
        Arc::new(Self {
            pending: Mutex::default(),
            hub,
        })
    }

    fn lock(&self) -> MutexGuard<'_, Vec<Pending>> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The pending interactions of `chat` for `owner`, to replay.
    pub fn replay(&self, chat: &SessionId, owner: &ApplicationOwner) -> Vec<Value> {
        self.lock()
            .iter()
            .filter(|pending| pending.chat == *chat && same_owner(&pending.owner, owner))
            .map(Pending::event)
            .collect()
    }

    /// Answers interaction `id`, once.
    ///
    /// # Errors
    ///
    /// It isn't pending for this chat and owner, in Karaka's words, exposed
    /// as `CHAT_FORBIDDEN`.
    pub fn respond(
        &self,
        chat: &SessionId,
        owner: &ApplicationOwner,
        id: &str,
        answer: AskUserQuestionAnswer,
    ) -> Result<(), AppError> {
        let mut pending = self.lock();
        let found = pending.iter().position(|interaction| {
            interaction.id == id
                && interaction.chat == *chat
                && same_owner(&interaction.owner, owner)
        });
        let Some(index) = found else {
            return Err(AppError::coded(
                "CHAT_FORBIDDEN",
                "Interaction is not pending for this chat",
            ));
        };
        let mut interaction = pending.remove(index);
        if let Some(resolve) = interaction.resolve.take() {
            let _ = resolve.send(answer);
        }
        Ok(())
    }

    fn remove(&self, id: &str) {
        self.lock().retain(|interaction| interaction.id != id);
    }
}

/// Takes an interaction out when its wait ends however it ends.
struct Withdraw<'a> {
    interactions: &'a Interactions,
    id: String,
}

impl Drop for Withdraw<'_> {
    fn drop(&mut self) {
        self.interactions.remove(&self.id);
    }
}

/// A question cancelled before its answer. (Karaka: the signal's reason,
/// or `Interaction was cancelled`.)
#[derive(Debug)]
struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Interaction was cancelled")
    }
}

impl std::error::Error for Cancelled {}

/// The answerer the transport registers. A question from no agent, or from
/// a chat with no application owner, goes on down the chain.
pub struct ApplicationAnswerer {
    pub interactions: Arc<Interactions>,
    pub identity: Arc<KarakaIdentity>,
    pub store: Arc<SessionStore>,
}

impl QuestionAnswerer for ApplicationAnswerer {
    fn answer<'a>(
        &'a self,
        request: &'a QuestionRequest,
        next: AnswerNext<'a>,
    ) -> AnswerFuture<'a> {
        Box::pin(async move {
            let Some(agent) = &request.agent else {
                return next.run().await;
            };
            let Some(session) = self.store.get(agent.id()) else {
                return next.run().await;
            };
            let owner = self
                .identity
                .owner_of_session(&session)
                .await
                .map_err(QuestionError::other)?;
            let Some(owner) = owner else {
                return next.run().await;
            };
            let id = random_uuid();
            let (resolve, answered) = oneshot::channel();
            let pending = Pending {
                id: id.clone(),
                chat: agent.id().clone(),
                owner,
                questions: serde_json::to_value(&request.questions).unwrap_or(Value::Null),
                cursor: session.len() as i64 - 1,
                resolve: Some(resolve),
            };
            let event = pending.event();
            self.interactions.lock().push(pending);
            let _withdraw = Withdraw {
                interactions: &self.interactions,
                id,
            };
            self.interactions.hub.publish(agent.id(), &event);
            let signal = request.signal.clone();
            tokio::select! {
                answer = answered => answer.map_err(|_| QuestionError::other(Cancelled)),
                () = async {
                    match &signal {
                        Some(signal) => signal.wait().await,
                        None => std::future::pending().await,
                    }
                } => Err(QuestionError::other(Cancelled)),
            }
        })
    }
}
