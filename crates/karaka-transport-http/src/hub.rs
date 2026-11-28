//! Live delivery: every session's appended events and every agent's text
//! deltas, fanned out to the follows of each chat, and each chat's
//! interactions to its open streams. (Karaka: the `session/event` and
//! `agent/assistant-stream` listeners of `follow`, and `subscribers` in
//! `index.ts`.)

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use karaka_session::{Appended, Entry, FlushFuture, SessionId, SessionListener, SessionStore};
use serde_json::Value;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

/// One live item of a chat.
#[derive(Clone, Debug)]
pub enum Live {
    /// A durable event, as it is appended.
    Event(Box<Entry>),
    /// Assistant text as it streams, at the log's cursor then.
    TextDelta { cursor: i64, text: String },
}

type Senders<T> = Mutex<HashMap<SessionId, Vec<(u64, UnboundedSender<T>)>>>;

/// The fan-out.
pub struct Hub {
    store: Weak<SessionStore>,
    live: Senders<Live>,
    interactions: Senders<Value>,
    next: Mutex<u64>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Hub {
    pub fn new(store: &Arc<SessionStore>) -> Arc<Self> {
        Arc::new(Self {
            store: Arc::downgrade(store),
            live: Mutex::default(),
            interactions: Mutex::default(),
            next: Mutex::new(0),
        })
    }

    fn id(&self) -> u64 {
        let mut next = lock(&self.next);
        *next += 1;
        *next
    }

    /// Hears chat `chat`'s live items until dropped.
    pub fn follow(self: &Arc<Self>, chat: &SessionId) -> Subscription<Live> {
        let (sender, receiver) = unbounded_channel();
        let id = self.id();
        lock(&self.live)
            .entry(chat.clone())
            .or_default()
            .push((id, sender));
        Subscription {
            hub: Arc::downgrade(self),
            chat: chat.clone(),
            id,
            receiver,
            interactions: false,
        }
    }

    /// Hears chat `chat`'s interactions until dropped.
    pub fn interactions(self: &Arc<Self>, chat: &SessionId) -> Subscription<Value> {
        let (sender, receiver) = unbounded_channel();
        let id = self.id();
        lock(&self.interactions)
            .entry(chat.clone())
            .or_default()
            .push((id, sender));
        Subscription {
            hub: Arc::downgrade(self),
            chat: chat.clone(),
            id,
            receiver,
            interactions: true,
        }
    }

    fn send_live(&self, chat: &SessionId, item: &Live) {
        if let Some(senders) = lock(&self.live).get(chat) {
            for (_, sender) in senders {
                let _ = sender.send(item.clone());
            }
        }
    }

    /// Assistant text streamed for chat `chat`. (Karaka: the
    /// `agent/assistant-stream` listener, its cursor `session.seq - 1`.)
    pub fn text_delta(&self, chat: &SessionId, text: &str) {
        if !lock(&self.live).contains_key(chat) {
            return;
        }
        let cursor = self
            .store
            .upgrade()
            .and_then(|store| store.get(chat))
            .map_or(-1, |session| session.len() as i64 - 1);
        self.send_live(
            chat,
            &Live::TextDelta {
                cursor,
                text: text.to_owned(),
            },
        );
    }

    /// An interaction for chat `chat`'s open streams. (Karaka: `publish`.)
    pub fn publish(&self, chat: &SessionId, event: &Value) {
        if let Some(senders) = lock(&self.interactions).get(chat) {
            for (_, sender) in senders {
                let _ = sender.send(event.clone());
            }
        }
    }
}

impl SessionListener for Hub {
    fn event(&self, session: &Appended<'_>, entry: &Entry) {
        self.send_live(session.id(), &Live::Event(Box::new(entry.clone())));
    }

    fn flush(&self, _: &SessionId) -> Option<FlushFuture> {
        None
    }
}

/// A subscription, removed when dropped.
pub struct Subscription<T> {
    hub: Weak<Hub>,
    chat: SessionId,
    id: u64,
    receiver: UnboundedReceiver<T>,
    interactions: bool,
}

impl<T> Subscription<T> {
    pub async fn recv(&mut self) -> Option<T> {
        self.receiver.recv().await
    }
}

impl<T> Drop for Subscription<T> {
    fn drop(&mut self) {
        let Some(hub) = self.hub.upgrade() else {
            return;
        };
        if self.interactions {
            remove(&mut lock(&hub.interactions), &self.chat, self.id);
        } else {
            remove(&mut lock(&hub.live), &self.chat, self.id);
        }
    }
}

fn remove<T>(
    map: &mut HashMap<SessionId, Vec<(u64, UnboundedSender<T>)>>,
    chat: &SessionId,
    id: u64,
) {
    if let Some(senders) = map.get_mut(chat) {
        senders.retain(|(known, _)| *known != id);
        if senders.is_empty() {
            map.remove(chat);
        }
    }
}
