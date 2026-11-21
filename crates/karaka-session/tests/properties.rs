//! Properties of derivation over many generated logs.
//! (dsh v4: `packages/core/session/tests/properties.spec.ts`.)
//!
//! v4 generates logs with fast-check. Here a small seeded generator makes
//! the same kinds of logs, so every run checks the same cases and a failure
//! names the seed that found it. There is no shrinking: a failing seed is
//! reproduced by running that one seed.

use karaka_model::{Block, ToolResult};
use karaka_session::{
    AssistantAttemptData, AssistantMessage, AssistantMessageData, CallConfig, ContentBlock, Extra,
    IN_HISTORY, Passthrough, RequestContext, RequestHeader, RequestHeaderReason, SessionEvent,
    SessionLog, SurfaceIntent, TokenUsage, ToolResultData, TurnEndReason, UserMessage, UserSource,
};
use serde_json::json;

const CASES: u64 = 500;

/// A tiny xorshift generator, enough to pick events. Not for anything that
/// needs real randomness.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        // Mixed so that small seeds don't start out similar; never zero.
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    /// A number from 0 up to, but not including, `bound`.
    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }

    fn coin(&mut self) -> bool {
        self.below(2) == 0
    }

    fn text(&mut self) -> String {
        let length = self.below(6);
        (0..length)
            .map(|_| char::from(b'a' + self.below(26) as u8))
            .collect()
    }

    /// Up to three text blocks, as v4's `textContentArb` makes.
    fn texts(&mut self) -> Vec<String> {
        (0..self.below(4)).map(|_| self.text()).collect()
    }
}

/// An event that produces a message: these do change the history.
fn message_event(rng: &mut Rng, index: usize) -> SessionEvent {
    match rng.below(4) {
        0 => {
            let mut message = UserMessage::text(format!("user-{index}"), UserSource::user(), "");
            message.content = rng.texts().into_iter().map(ContentBlock::text).collect();
            SessionEvent::user(message)
        }
        kind @ (1 | 2) => {
            let content: Vec<Block> = rng.texts().into_iter().map(Block::Text).collect();
            SessionEvent::assistant(AssistantMessageData {
                turn: 1,
                step: 1,
                message: AssistantMessage::from_model(
                    format!("assistant-{index}"),
                    "mock",
                    "mock",
                    &content,
                ),
                stream: Vec::new(),
                // As in v4, one kind of assistant event carries usage.
                usage: (kind == 2).then(|| TokenUsage {
                    input_tokens: 1,
                    output_tokens: 1,
                    ..TokenUsage::default()
                }),
                interrupted: false,
                extra: Extra::new(),
            })
        }
        _ => {
            let result = ToolResult {
                call_id: format!("call-{}", rng.text()),
                content: rng.text(),
                is_error: rng.coin(),
            };
            SessionEvent::ToolResult {
                data: ToolResultData::new(format!("result-{index}"), 1, 1, &result),
                surface: SurfaceIntent::append(),
            }
        }
    }
}

/// An event that produces no message: it must never change the history.
fn noise_event(rng: &mut Rng) -> SessionEvent {
    let passthrough = |event_type: &str, data| {
        SessionEvent::Passthrough(Passthrough {
            event_type: event_type.into(),
            data,
            surface_op: None,
            source_event_seqs: None,
        })
    };
    match rng.below(9) {
        0 => SessionEvent::turn_start(1),
        1 => SessionEvent::turn_end(1, TurnEndReason::Completed),
        2 => SessionEvent::step_start(1, 1),
        3 => SessionEvent::step_end(1, 1),
        4 => SessionEvent::AssistantAttempt(AssistantAttemptData {
            turn: 1,
            step: 1,
            stream: vec![json!(
                { "type": "text-chunks", "time0": 1, "index": 0, "dt": [], "texts": [rng.text()] }
            )],
            extra: Extra::new(),
        }),
        5 => SessionEvent::tool_call(
            1,
            1,
            &karaka_model::ToolCall {
                id: format!("call-{}", rng.text()),
                name: "clock".into(),
                arguments: "{}".into(),
            },
        ),
        6 => SessionEvent::request_header(
            RequestHeader::new(CallConfig::new("mock", format!("m-{}", rng.text())), &[]).unwrap(),
            RequestHeaderReason::Change,
        ),
        7 => SessionEvent::RequestContext(RequestContext {
            provider: "mock".into(),
            model: format!("m-{}", rng.text()),
            context_window: None,
            system_prompt_update: Some(IN_HISTORY.into()),
            extra: Extra::new(),
        }),
        _ => passthrough(
            "approval/asked",
            json!({ "id": rng.text(), "toolName": "clock" }),
        ),
    }
}

fn messages(rng: &mut Rng) -> Vec<SessionEvent> {
    (0..rng.below(13) as usize)
        .map(|index| message_event(rng, index))
        .collect()
}

fn noise(rng: &mut Rng) -> Vec<SessionEvent> {
    (0..rng.below(13)).map(|_| noise_event(rng)).collect()
}

fn any_log(rng: &mut Rng) -> Vec<SessionEvent> {
    (0..rng.below(26) as usize)
        .map(|index| {
            if rng.coin() {
                message_event(rng, index)
            } else {
                noise_event(rng)
            }
        })
        .collect()
}

fn build(events: Vec<SessionEvent>) -> SessionLog {
    let mut log = SessionLog::new();
    log.append_all(events).expect("generated events are valid");
    log
}

#[test]
fn derivation_is_deterministic() {
    for seed in 0..CASES {
        let log = build(any_log(&mut Rng::new(seed)));
        assert_eq!(log.history(), log.history(), "seed {seed}");
    }
}

#[test]
fn seqs_count_up_from_zero_without_gaps() {
    for seed in 0..CASES {
        let events = any_log(&mut Rng::new(seed));
        let count = events.len();
        let log = build(events);
        for (index, entry) in log.events().iter().enumerate() {
            assert_eq!(entry.seq.0, index as u64, "seed {seed}");
        }
        assert_eq!(log.len(), count, "seed {seed}");
    }
}

// v4's "replay-from-seed reproduces the derivation identically". Karaka's
// restore adds no `session/end-seed` marker, so the restored log is the same
// length, not one longer.
#[test]
fn a_log_restored_from_its_json_derives_the_same_history() {
    for seed in 0..CASES {
        let original = build(any_log(&mut Rng::new(seed)));
        let json = serde_json::to_string(original.events()).unwrap();
        let restored =
            SessionLog::restore(serde_json::from_str::<Vec<karaka_session::Entry>>(&json).unwrap())
                .unwrap();
        assert_eq!(restored.events(), original.events(), "seed {seed}");
        assert_eq!(restored.history(), original.history(), "seed {seed}");
    }
}

// v4's "non-message events never affect derived history (any interleaving)".
#[test]
fn non_message_events_never_affect_the_history_in_any_interleaving() {
    for seed in 0..CASES {
        let mut rng = Rng::new(seed);
        let messages = messages(&mut rng);
        let noise = noise(&mut rng);
        let clean = build(messages.clone()).history();

        // A random merge of the two lists that keeps each one's own order.
        let mut merged = Vec::new();
        let (mut message, mut noisy) = (
            messages.into_iter().peekable(),
            noise.into_iter().peekable(),
        );
        while message.peek().is_some() || noisy.peek().is_some() {
            let take_noise = noisy.peek().is_some() && (message.peek().is_none() || rng.coin());
            merged.push(
                if take_noise {
                    noisy.next()
                } else {
                    message.next()
                }
                .unwrap(),
            );
        }
        assert_eq!(build(merged).history(), clean, "seed {seed}");
    }
}

// v4's "every derived message has a known role", in Karaka's terms: the
// system prompt is never a message, and every result sits in a user
// message.
#[test]
fn every_tool_result_is_in_a_user_message_and_reading_never_changes_the_log() {
    for seed in 0..CASES {
        let log = build(any_log(&mut Rng::new(seed)));
        let before = log.events().to_vec();
        let mut derived_results = 0;
        for message in log.messages() {
            let results = message
                .content
                .iter()
                .filter(|block| matches!(block, Block::ToolResult(_)))
                .count();
            if results > 0 {
                assert_eq!(message.role, karaka_model::Role::User, "seed {seed}");
                assert_eq!(
                    results,
                    message.content.len(),
                    "seed {seed}: a results message holds only results"
                );
            }
            derived_results += results;
        }
        // No result is dropped, and none is made up.
        let logged_results = log
            .events()
            .iter()
            .filter(|entry| matches!(entry.event, SessionEvent::ToolResult { .. }))
            .count();
        assert_eq!(derived_results, logged_results, "seed {seed}");
        assert_eq!(log.events(), before, "seed {seed}");
    }
}
