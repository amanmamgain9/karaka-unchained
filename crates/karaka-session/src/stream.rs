//! The compact `stream` records an `assistant/message` or an
//! `assistant/attempt` embeds: the exact timed chunks of one model call.
//! (dsh: `AssistantStreamAccumulator` and `expandAssistantStream` in
//! `llm/src/assistant-stream.ts`.)
//!
//! # The records
//!
//! Each chunk is one of dsh's `StreamChunk`s, as JSON, with the time it
//! arrived in milliseconds since the Unix epoch, as dsh's `Date.now()`
//! gives it and as [`Time`] counts. Deltas in a row are packed into runs,
//! keeping every delta's boundary and time:
//!
//! ```json
//! { "type": "text-chunks", "time0": 1000, "index": 0, "dt": [6], "texts": ["hel", "lo"] }
//! { "type": "reasoning-chunks", "time0": 1000, "index": 0, "dt": [], "texts": ["think"] }
//! { "type": "tool-call-chunks", "time0": 1008, "index": 1, "dt": [3],
//!   "id": "call-1", "name": "run_code", "args": ["{", "}"] }
//! { "type": "chunk", "time": 1020, "chunk": { "type": "finish", "reason": { "kind": "stop" } } }
//! ```
//!
//! A delta joins the run before it when it is the same kind, for the same
//! block, and its time is a safe gap from the last one. A tool call's deltas
//! must also have the same id and the same name, or no name. Everything else
//! is a raw `chunk` record, kept exactly as it was: block starts and ends,
//! usage, the finish, and tool-call deltas with an empty id or an empty
//! name.
//!
//! # Chunks as JSON
//!
//! The records hold dsh's JSON, not Karaka's types, so a record a dsh adapter
//! wrote, with fields or kinds of chunk Karaka doesn't model, reads and
//! writes back unchanged. [`chunk_json`] writes a Karaka [`Event`] as dsh's
//! chunk, and [`chunk_event`] reads one back where Karaka has a type for it.
//!
//! # Checking
//!
//! dsh's session keeps embedded streams opaque, checking only that `stream`
//! is a list (`core/session/src/index.ts:281-293`), which the type here
//! guarantees. Its storage verifier expands every stream and checks it
//! (`session-persistence-jsonl/src/generation.ts:553-577`); a durable
//! backend does that with [`expand_stream`].

use std::fmt;

use karaka_model::{Block, BlockType, Event, FinishReason, ToolCall};
use serde_json::{Map, Value, json};

use crate::data::Failure;
use crate::event::Time;
use crate::message::{ContentBlock, Extra, ReasoningBlock, TokenUsage, ToolCallBlock};

/// JavaScript's largest safe integer, which dsh requires of every time,
/// gap and index.
const MAX_SAFE: i64 = 9_007_199_254_740_991;

/// One chunk and the time it arrived. (dsh: `TimedStreamChunk`.)
#[derive(Clone, Debug, PartialEq)]
pub struct TimedChunk {
    pub time: Time,
    /// dsh's `StreamChunk`, as JSON.
    pub chunk: Value,
}

/// Why a chunk or a record was refused, in dsh's words.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamError(String);

impl fmt::Display for StreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for StreamError {}

fn error(message: impl Into<String>) -> StreamError {
    StreamError(message.into())
}

/// Packs one call's chunks into compact records as they arrive.
/// (dsh: `AssistantStreamAccumulator`.)
#[derive(Clone, Debug, Default)]
pub struct StreamAccumulator {
    records: Vec<Record>,
}

#[derive(Clone, Debug)]
enum Record {
    Run {
        kind: RunKind,
        time0: i64,
        index: u64,
        dt: Vec<i64>,
        /// A tool call's id and name: `name` is `None` when the deltas had
        /// no `name` field at all.
        id: String,
        name: Option<String>,
        members: Vec<String>,
        last_time: i64,
    },
    Raw {
        time: i64,
        chunk: Value,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RunKind {
    Text,
    Reasoning,
    ToolCall,
}

impl RunKind {
    fn record_type(self) -> &'static str {
        match self {
            Self::Text => "text-chunks",
            Self::Reasoning => "reasoning-chunks",
            Self::ToolCall => "tool-call-chunks",
        }
    }
}

impl StreamAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds one chunk, `chunk` being dsh's JSON for it, as it arrived at
    /// `time`. (dsh: `push`.) Refuses a time that isn't a safe integer, and a
    /// delta whose index or fields aren't what dsh requires.
    pub fn push(&mut self, time: Time, chunk: Value) -> Result<(), StreamError> {
        let time = safe_time(time.0)?;
        let kind = match chunk.get("type").and_then(Value::as_str) {
            Some("text-delta") => RunKind::Text,
            Some("reasoning-delta") => RunKind::Reasoning,
            Some("tool-call-delta") => RunKind::ToolCall,
            Some(_) => {
                self.records.push(Record::Raw { time, chunk });
                return Ok(());
            }
            None => return Err(error("Assistant stream chunk must have a type")),
        };
        let label = chunk["type"].as_str().unwrap_or_default().to_owned();
        let index = safe_index(&chunk["index"], &label)?;
        let (id, name, member) = match kind {
            RunKind::Text | RunKind::Reasoning => {
                let text = chunk["text"]
                    .as_str()
                    .ok_or_else(|| error(format!("{label} text must be a string")))?;
                (String::new(), None, text.to_owned())
            }
            RunKind::ToolCall => {
                let id = chunk["id"]
                    .as_str()
                    .ok_or_else(|| error("tool-call-delta id must be a string"))?;
                let name = match chunk.get("name") {
                    None => None,
                    Some(Value::String(name)) => Some(name.clone()),
                    Some(_) => return Err(error("tool-call-delta name must be a string")),
                };
                let arguments = chunk["argumentsDelta"]
                    .as_str()
                    .ok_or_else(|| error("tool-call-delta argumentsDelta must be a string"))?;
                // An empty id or name can't be packed: a run's id and name
                // must be non-empty. The chunk is kept as it is.
                if id.is_empty() || name.as_deref() == Some("") {
                    self.records.push(Record::Raw { time, chunk });
                    return Ok(());
                }
                (id.to_owned(), name, arguments.to_owned())
            }
        };
        if let Some(Record::Run {
            kind: previous_kind,
            index: previous_index,
            dt,
            id: previous_id,
            name: previous_name,
            members,
            last_time,
            ..
        }) = self.records.last_mut()
            && *previous_kind == kind
            && *previous_index == index
            && *previous_id == id
            && *previous_name == name
            && let Some(gap) = safe_gap(*last_time, time)
        {
            dt.push(gap);
            members.push(member);
            *last_time = time;
            return Ok(());
        }
        self.records.push(Record::Run {
            kind,
            time0: time,
            index,
            dt: Vec::new(),
            id,
            name,
            members: vec![member],
            last_time: time,
        });
        Ok(())
    }

    /// The records so far, as JSON for the event's `stream`. (dsh:
    /// `snapshot`.)
    pub fn snapshot(&self) -> Vec<Value> {
        self.records.iter().map(record_json).collect()
    }
}

fn record_json(record: &Record) -> Value {
    match record {
        Record::Raw { time, chunk } => json!({ "type": "chunk", "time": time, "chunk": chunk }),
        Record::Run {
            kind,
            time0,
            index,
            dt,
            id,
            name,
            members,
            ..
        } => {
            let mut out = Map::new();
            out.insert("type".into(), json!(kind.record_type()));
            out.insert("time0".into(), json!(time0));
            out.insert("index".into(), json!(index));
            out.insert("dt".into(), json!(dt));
            if *kind == RunKind::ToolCall {
                out.insert("id".into(), json!(id));
                if let Some(name) = name {
                    out.insert("name".into(), json!(name));
                }
                out.insert("args".into(), json!(members));
            } else {
                out.insert("texts".into(), json!(members));
            }
            Value::Object(out)
        }
    }
}

/// The exact timed chunks a stream's records hold, every delta's boundary
/// kept. Each record is checked as dsh checks it (`validateRecord`), and the
/// first that fails is refused. (dsh: `expandAssistantStream`.)
pub fn expand_stream(records: &[Value]) -> Result<Vec<TimedChunk>, StreamError> {
    let mut chunks = Vec::new();
    for record in records {
        let object = record
            .as_object()
            .ok_or_else(|| error("Assistant stream record must be an object"))?;
        let record_type = object.get("type").and_then(Value::as_str);
        match record_type {
            Some(label @ ("text-chunks" | "reasoning-chunks")) => {
                exact_keys(object, &["type", "time0", "index", "dt", "texts"], label)?;
                let texts = strings(&object["texts"], &format!("{label} texts"))?;
                if texts.is_empty() {
                    return Err(error(format!("{label} texts must be non-empty")));
                }
                let (index, times) = run_times(object, texts.len(), label)?;
                let delta = if label == "text-chunks" {
                    "text-delta"
                } else {
                    "reasoning-delta"
                };
                for (text, time) in texts.into_iter().zip(times) {
                    chunks.push(TimedChunk {
                        time: Time(time),
                        chunk: json!({ "type": delta, "index": index, "text": text }),
                    });
                }
            }
            Some(label @ "tool-call-chunks") => {
                let has_name = object.contains_key("name");
                let keys: &[&str] = if has_name {
                    &["type", "time0", "index", "dt", "id", "name", "args"]
                } else {
                    &["type", "time0", "index", "dt", "id", "args"]
                };
                exact_keys(object, keys, label)?;
                let args = strings(&object["args"], "tool-call-chunks args")?;
                if args.is_empty() {
                    return Err(error("tool-call-chunks args must be non-empty"));
                }
                let id = object["id"]
                    .as_str()
                    .filter(|id| !id.is_empty())
                    .ok_or_else(|| error("tool-call-chunks id must be a non-empty string"))?;
                let name = match object.get("name") {
                    None => None,
                    Some(Value::String(name)) if !name.is_empty() => Some(name.as_str()),
                    Some(_) => {
                        return Err(error("tool-call-chunks name must be a non-empty string"));
                    }
                };
                let (index, times) = run_times(object, args.len(), label)?;
                for (arguments, time) in args.into_iter().zip(times) {
                    let mut chunk = Map::new();
                    chunk.insert("type".into(), json!("tool-call-delta"));
                    chunk.insert("index".into(), json!(index));
                    chunk.insert("id".into(), json!(id));
                    if let Some(name) = name {
                        chunk.insert("name".into(), json!(name));
                    }
                    chunk.insert("argumentsDelta".into(), json!(arguments));
                    chunks.push(TimedChunk {
                        time: Time(time),
                        chunk: Value::Object(chunk),
                    });
                }
            }
            Some("chunk") => {
                exact_keys(object, &["type", "time", "chunk"], "chunk")?;
                let time = safe_time_value(&object["time"])?;
                if !object["chunk"].is_object() {
                    return Err(error(
                        "Assistant stream raw chunk must be a lossless JSON object",
                    ));
                }
                chunks.push(TimedChunk {
                    time: Time(time),
                    chunk: object["chunk"].clone(),
                });
            }
            _ => {
                let shown = object
                    .get("type")
                    .map_or("undefined".into(), Value::to_string);
                return Err(error(format!(
                    "Unsupported Assistant stream record {shown}"
                )));
            }
        }
    }
    Ok(chunks)
}

/// The index and the member times of a run, checked as dsh's `validateRun`
/// checks them.
fn run_times(
    object: &Map<String, Value>,
    members: usize,
    label: &str,
) -> Result<(u64, Vec<i64>), StreamError> {
    let time0 = safe_time_value(&object["time0"])?;
    let index = safe_index(&object["index"], label)?;
    let dt: Vec<i64> = object["dt"]
        .as_array()
        .and_then(|dt| dt.iter().map(safe_integer).collect::<Option<Vec<_>>>())
        .ok_or_else(|| error(format!("{label} dt must contain safe integers")))?;
    if dt.len() + 1 != members {
        return Err(error(format!(
            "{label} dt length must be one less than its members"
        )));
    }
    let mut times = vec![time0];
    let mut time = time0;
    for gap in dt {
        time = time
            .checked_add(gap)
            .filter(|time| time.abs() <= MAX_SAFE)
            .ok_or_else(|| error(format!("{label} member times must stay safe integers")))?;
        times.push(time);
    }
    Ok((index, times))
}

fn exact_keys(object: &Map<String, Value>, keys: &[&str], label: &str) -> Result<(), StreamError> {
    if object.len() != keys.len() || !keys.iter().all(|key| object.contains_key(*key)) {
        return Err(error(format!(
            "{label} Assistant stream record must contain exactly {}",
            keys.join(", ")
        )));
    }
    Ok(())
}

fn strings(value: &Value, label: &str) -> Result<Vec<String>, StreamError> {
    value
        .as_array()
        .and_then(|items| {
            items
                .iter()
                .map(|item| item.as_str().map(str::to_owned))
                .collect::<Option<Vec<_>>>()
        })
        .ok_or_else(|| error(format!("{label} must be a string array")))
}

/// A JSON number that is a safe integer, as JavaScript's
/// `Number.isSafeInteger` decides: `1.0` counts, `0.5` doesn't.
fn safe_integer(value: &Value) -> Option<i64> {
    let number = value.as_number()?;
    let integer = match (number.as_i64(), number.as_f64()) {
        (Some(integer), _) => integer,
        (None, Some(float)) if float.fract() == 0.0 && float.abs() <= MAX_SAFE as f64 => {
            float as i64
        }
        _ => return None,
    };
    (integer.abs() <= MAX_SAFE).then_some(integer)
}

fn safe_time(time: i64) -> Result<i64, StreamError> {
    if time.abs() <= MAX_SAFE {
        Ok(time)
    } else {
        Err(error(format!(
            "Assistant stream time must be a safe integer, got {time}"
        )))
    }
}

fn safe_time_value(value: &Value) -> Result<i64, StreamError> {
    safe_integer(value).ok_or_else(|| {
        error(format!(
            "Assistant stream time must be a safe integer, got {value}"
        ))
    })
}

/// A non-negative safe integer index. dsh also refuses `-0`, which
/// serde_json reads as the float `-0.0`.
fn safe_index(value: &Value, label: &str) -> Result<u64, StreamError> {
    let negative_zero = value
        .as_f64()
        .is_some_and(|float| float == 0.0 && float.is_sign_negative());
    match safe_integer(value) {
        Some(index) if index >= 0 && !negative_zero => Ok(index as u64),
        _ => Err(error(format!(
            "{label} index must be a non-negative safe integer"
        ))),
    }
}

/// `next - previous`, when it is a safe integer. (dsh: `safeGap`.)
fn safe_gap(previous: i64, next: i64) -> Option<i64> {
    next.checked_sub(previous)
        .filter(|gap| gap.abs() <= MAX_SAFE)
}

/// dsh's JSON for one of Karaka's chunks: `block-start`, `text-delta`,
/// `reasoning-delta`, `tool-call-delta`, `block-end`, `usage` or `finish`.
///
/// A `block-end`'s block is v4's content block, as a message holds it: a
/// reasoning block's signature isn't in it, as dsh's isn't. A `finish` has
/// no `replayState`; [`finish_json`] writes one that does.
pub fn chunk_json(event: &Event) -> Value {
    match event {
        Event::BlockStart { index, block_type } => {
            json!({ "type": "block-start", "index": index, "blockType": block_type.name() })
        }
        Event::TextDelta { index, text } => {
            json!({ "type": "text-delta", "index": index, "text": text })
        }
        Event::ReasoningDelta { index, text } => {
            json!({ "type": "reasoning-delta", "index": index, "text": text })
        }
        Event::ToolCallDelta {
            index,
            id,
            name,
            arguments_delta,
        } => {
            let mut chunk = Map::new();
            chunk.insert("type".into(), json!("tool-call-delta"));
            chunk.insert("index".into(), json!(index));
            chunk.insert("id".into(), json!(id));
            if let Some(name) = name {
                chunk.insert("name".into(), json!(name));
            }
            chunk.insert("argumentsDelta".into(), json!(arguments_delta));
            Value::Object(chunk)
        }
        Event::BlockEnd { index, block } => {
            let block = content_block(block).map_or(Value::Null, |block| {
                serde_json::to_value(block).unwrap_or(Value::Null)
            });
            json!({ "type": "block-end", "index": index, "block": block })
        }
        Event::Usage { usage } => {
            let usage = serde_json::to_value(TokenUsage::from(*usage)).unwrap_or(Value::Null);
            json!({ "type": "usage", "usage": usage })
        }
        Event::Finish { reason } => finish_json(*reason, None),
    }
}

/// dsh's `finish` chunk for a call that finished, with the replay state of
/// the answer, if it has any: the same envelope its message's
/// `source.replayState` gets, as a dsh adapter carries it there.
pub fn finish_json(reason: FinishReason, replay_state: Option<&Value>) -> Value {
    let mut chunk = Map::new();
    chunk.insert("type".into(), json!("finish"));
    chunk.insert("reason".into(), json!({ "kind": reason.kind() }));
    if let Some(state) = replay_state {
        chunk.insert("replayState".into(), state.clone());
    }
    Value::Object(chunk)
}

/// dsh's `finish` chunk for a call that failed: `{ type: "finish", reason:
/// { kind: "error", failure } }`, the failure as `turn/end` writes it. A
/// dsh adapter's failure ends its stream so; an attempt with no finish at
/// all, in dsh, is one whose stream threw.
pub fn failure_json(failure: &Failure) -> Value {
    let failure = serde_json::to_value(failure).unwrap_or(Value::Null);
    json!({ "type": "finish", "reason": { "kind": "error", "failure": failure } })
}

/// The Karaka chunk a dsh chunk is, where Karaka has one: `None` for a kind
/// it doesn't model, such as a `finish` of kind `error`, or a block other
/// than text, reasoning or a tool call. Fields Karaka doesn't model, such
/// as a finish's `replayState`, are left out: read the JSON itself for
/// those. A reasoning block comes back without its signature, which dsh's
/// block doesn't hold.
pub fn chunk_event(chunk: &Value) -> Option<Event> {
    let index = || {
        chunk["index"]
            .as_u64()
            .and_then(|index| usize::try_from(index).ok())
    };
    let text = || chunk["text"].as_str().map(str::to_owned);
    Some(match chunk["type"].as_str()? {
        "block-start" => Event::BlockStart {
            index: index()?,
            block_type: match chunk["blockType"].as_str()? {
                "text" => BlockType::Text,
                "reasoning" => BlockType::Reasoning,
                "tool-call" => BlockType::ToolCall,
                _ => return None,
            },
        },
        "text-delta" => Event::TextDelta {
            index: index()?,
            text: text()?,
        },
        "reasoning-delta" => Event::ReasoningDelta {
            index: index()?,
            text: text()?,
        },
        "tool-call-delta" => Event::ToolCallDelta {
            index: index()?,
            id: chunk["id"].as_str()?.to_owned(),
            name: match chunk.get("name") {
                None => None,
                Some(name) => Some(name.as_str()?.to_owned()),
            },
            arguments_delta: chunk["argumentsDelta"].as_str()?.to_owned(),
        },
        "block-end" => Event::BlockEnd {
            index: index()?,
            block: model_block(&serde_json::from_value(chunk["block"].clone()).ok()?)?,
        },
        "usage" => Event::Usage {
            usage: (&serde_json::from_value::<TokenUsage>(chunk["usage"].clone()).ok()?).into(),
        },
        "finish" => Event::Finish {
            reason: match chunk["reason"]["kind"].as_str()? {
                "stop" => FinishReason::Stop,
                "tool-calls" => FinishReason::ToolCalls,
                "max-tokens" => FinishReason::MaxTokens,
                "paused" => FinishReason::Paused,
                _ => return None,
            },
        },
        _ => return None,
    })
}

/// v4's content block for one a model wrote.
fn content_block(block: &Block) -> Option<ContentBlock> {
    Some(match block {
        Block::Text(text) => ContentBlock::text(text.clone()),
        Block::Reasoning { text, .. } => ContentBlock::Reasoning(ReasoningBlock {
            text: text.clone(),
            extra: Extra::new(),
        }),
        Block::ToolCall(call) => ContentBlock::ToolCall(ToolCallBlock {
            id: call.id.clone(),
            name: call.name.clone(),
            arguments: call.arguments.clone(),
            extra: Extra::new(),
        }),
        Block::ToolResult(_) => return None,
    })
}

fn model_block(block: &ContentBlock) -> Option<Block> {
    Some(match block {
        ContentBlock::Text(text) => Block::Text(text.text.clone()),
        ContentBlock::Reasoning(reasoning) => Block::Reasoning {
            text: reasoning.text.clone(),
            replay: None,
        },
        ContentBlock::ToolCall(call) => Block::ToolCall(ToolCall {
            id: call.id.clone(),
            name: call.name.clone(),
            arguments: call.arguments.clone(),
        }),
        ContentBlock::Other(_) => return None,
    })
}
