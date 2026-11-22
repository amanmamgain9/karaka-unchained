//! What the backend's tests share: a temporary root, headers, and dsh's
//! one-turn log (`session-persistence/tests/contract.ts`).
#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use karaka_session::{Entry, SessionHeader, SessionId};
use karaka_session_jsonl::{JsonlConfig, JsonlPersistence, generation_path};
use karaka_session_persistence::{CreateOptions, SessionAccess, SessionPersistence};
use serde_json::{Value, json};

/// A fresh directory under the system's temporary directory, removed when
/// dropped. Tests never touch any other sessions.
pub struct TempRoot(PathBuf);

impl TempRoot {
    pub fn new(name: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "karaka-jsonl-test-{name}-{}-{n}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    /// A backend over this root, as another process would start one.
    pub fn backend(&self) -> JsonlPersistence {
        JsonlPersistence::new(JsonlConfig {
            root: self.0.clone(),
        })
        .unwrap()
    }

    /// The current generation's path for a session.
    pub fn log(&self, cwd: Option<&str>, id: &str) -> PathBuf {
        generation_path(&self.0, cwd, id, 4).unwrap()
    }

    /// The path of a session's log in `version`.
    pub fn generation(&self, cwd: Option<&str>, id: &str, version: u64) -> PathBuf {
        generation_path(&self.0, cwd, id, version).unwrap()
    }

    /// Writes `content` as a session's log in `version`, making its
    /// directories.
    pub fn store(&self, cwd: Option<&str>, id: &str, version: u64, content: &str) -> PathBuf {
        let path = self.generation(cwd, id, version);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, content).unwrap();
        path
    }

    /// Every file under the root.
    pub fn files(&self) -> Vec<PathBuf> {
        fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
            for entry in fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(&path, out);
                } else {
                    out.push(path);
                }
            }
        }
        let mut out = Vec::new();
        walk(&self.0, &mut out);
        out
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub fn id(name: &str) -> SessionId {
    SessionId::new(name)
}

/// A header for `name`, created at 1000, in `cwd`. (contract.ts: `meta`.)
pub fn meta(name: &str, cwd: Option<&str>) -> SessionHeader {
    let mut header = SessionHeader::new(id(name), 1000);
    header.cwd = cwd.map(str::to_owned);
    header
}

pub fn entries(rows: Value) -> Vec<Entry> {
    serde_json::from_value(rows).expect("the rows read as events")
}

/// A well-formed one-turn log, seqs 0 to 5. (contract.ts: `oneTurnLog`.)
pub fn one_turn_log() -> Vec<Entry> {
    entries(json!([
        { "type": "turn/start", "seq": 0, "time": 1, "data": { "turn": 1 } },
        { "type": "user/message", "seq": 1, "time": 2, "surfaceOp": "append", "data": {
            "id": "one-turn-user", "role": "user",
            "content": [{ "type": "text", "text": "hi" }], "source": { "kind": "user" }
        } },
        { "type": "step/start", "seq": 2, "time": 3, "data": { "turn": 1, "step": 1 } },
        { "type": "assistant/message", "seq": 3, "time": 4, "surfaceOp": "append", "data": {
            "turn": 1, "step": 1,
            "message": {
                "id": "one-turn-assistant", "role": "assistant",
                "content": [{ "type": "text", "text": "hello" }],
                "source": { "kind": "model", "provider": "mock", "model": "mock" }
            },
            "stream": [
                { "type": "chunk", "time": 3, "chunk": { "type": "block-start", "index": 0, "blockType": "text" } },
                { "type": "text-chunks", "time0": 3, "index": 0, "dt": [], "texts": ["hello"] },
                { "type": "chunk", "time": 4, "chunk": { "type": "block-end", "index": 0, "block": { "type": "text", "text": "hello" } } },
                { "type": "chunk", "time": 4, "chunk": { "type": "finish", "reason": { "kind": "stop" } } }
            ]
        } },
        { "type": "step/end", "seq": 4, "time": 5, "data": { "turn": 1, "step": 1 } },
        { "type": "turn/end", "seq": 5, "time": 6, "data": { "turn": 1, "reason": { "kind": "completed" } } }
    ]))
}

/// A second turn continuing the log at `start`. (contract.ts: `secondTurn`.)
pub fn second_turn(start: u64) -> Vec<Entry> {
    entries(json!([
        { "type": "turn/start", "seq": start, "time": 9, "data": { "turn": 2 } },
        { "type": "turn/end", "seq": start + 1, "time": 10, "data": { "turn": 2, "reason": { "kind": "completed" } } }
    ]))
}

pub fn seqs(entries: &[Entry]) -> Vec<u64> {
    entries.iter().map(|entry| entry.seq.0).collect()
}

/// Creates, appends and closes: one whole log through a write handle.
pub async fn write_log(persistence: &JsonlPersistence, header: SessionHeader, events: Vec<Entry>) {
    let handle = persistence
        .create(header, CreateOptions::default())
        .await
        .unwrap();
    handle.append(events).await.unwrap();
    handle.close().await.unwrap();
}

/// Opens a read handle, reads everything, and closes.
pub async fn read_all(
    persistence: &JsonlPersistence,
    name: &str,
) -> Result<Vec<Entry>, karaka_session_persistence::PersistenceError> {
    let handle = persistence.open(&id(name), SessionAccess::Read).await?;
    let read = handle.read(0, None).await;
    handle.close().await.unwrap();
    read
}

/// The rows of a log file, each as JSON.
pub fn rows(path: &Path) -> Vec<Value> {
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}
