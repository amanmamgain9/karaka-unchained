//! Assembling a streamed answer from its chunks. (dsh: `BlockAssembler`,
//! `llm/src/assembler.ts`.)
//!
//! It is the one place an answer is put together, as in dsh: [`complete`]
//! and the agent loop both use it, so they keep the same blocks and drop the
//! same ones.
//!
//! [`complete`]: crate::complete

use crate::{Block, BlockType, Event, FinishReason, Message, Response, Role, ToolCall, Usage};

/// Builds blocks from the chunks of one model call, as they arrive.
///
/// It is tolerant, as dsh's is: a delta for a block that never started
/// starts it, and a delta or a second end for a block that has ended is
/// ignored, so a provider that misbehaves can't change a finished block.
#[derive(Clone, Debug, Default)]
pub struct BlockAssembler {
    /// Every block seen, in the order it was first seen.
    partials: Vec<Partial>,
    usage: Option<Usage>,
    finish: Option<FinishReason>,
}

#[derive(Clone, Debug)]
struct Partial {
    index: usize,
    block_type: BlockType,
    text: String,
    tool_call_id: Option<String>,
    tool_call_name: Option<String>,
    tool_call_arguments: String,
    /// Set by the block's end, which is authoritative and freezes it.
    block: Option<Block>,
}

impl BlockAssembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Takes in the next chunk, in stream order. (dsh: `push`.)
    pub fn push(&mut self, event: &Event) {
        match event {
            Event::BlockStart { index, block_type } => {
                self.ensure(*index, *block_type);
            }
            Event::TextDelta { index, text } => {
                let partial = self.ensure(*index, BlockType::Text);
                if partial.block.is_none() {
                    partial.text.push_str(text);
                }
            }
            Event::ReasoningDelta { index, text } => {
                let partial = self.ensure(*index, BlockType::Reasoning);
                if partial.block.is_none() {
                    partial.text.push_str(text);
                }
            }
            Event::ToolCallDelta {
                index,
                id,
                name,
                arguments_delta,
            } => {
                let partial = self.ensure(*index, BlockType::ToolCall);
                if partial.block.is_none() {
                    partial.tool_call_id = Some(id.clone());
                    if let Some(name) = name.as_ref().filter(|name| !name.is_empty()) {
                        partial.tool_call_name = Some(name.clone());
                    }
                    partial.tool_call_arguments.push_str(arguments_delta);
                }
            }
            Event::BlockEnd { index, block } => {
                let partial = self.ensure(*index, block_type(block));
                // The first end wins.
                if partial.block.is_none() {
                    partial.block = Some(block.clone());
                }
            }
            Event::Usage { usage } => self.usage = Some(*usage),
            Event::Finish { reason } => self.finish = Some(*reason),
        }
    }

    fn ensure(&mut self, index: usize, block_type: BlockType) -> &mut Partial {
        let position = match self.partials.iter().position(|p| p.index == index) {
            Some(position) => position,
            None => {
                self.partials.push(Partial {
                    index,
                    block_type,
                    text: String::new(),
                    tool_call_id: None,
                    tool_call_name: None,
                    tool_call_arguments: String::new(),
                    block: None,
                });
                self.partials.len() - 1
            }
        };
        &mut self.partials[position]
    }

    /// Every block seen, in stream order. A block still open is built from
    /// its deltas. A finish of [`FinishReason::MaxTokens`] drops the tool
    /// calls, which the limit may have cut off and which aren't safe to run.
    /// (dsh: `blocks`.)
    pub fn blocks(&self) -> Vec<Block> {
        let cut_off = self.finish == Some(FinishReason::MaxTokens);
        self.partials
            .iter()
            .map(assemble)
            .filter(|block| !(cut_off && matches!(block, Block::ToolCall(_))))
            .collect()
    }

    /// What is worth keeping of an answer a cancel interrupted: text and
    /// reasoning with something other than spaces in them, finished or
    /// not, in stream order. Tool calls are left out, since none of them
    /// ran, and keeping one would need a made-up result. (dsh:
    /// `interruptedBlocks`.)
    ///
    /// Reasoning keeps no signature unless the call had finished. dsh's
    /// signatures come only with the finish, so an interrupted message
    /// never has one; Karaka's come with each block, and are dropped the
    /// same.
    pub fn interrupted_blocks(&self) -> Vec<Block> {
        let finished = self.finish.is_some();
        self.partials
            .iter()
            .filter(|partial| {
                let block_type = partial
                    .block
                    .as_ref()
                    .map_or(partial.block_type, block_type);
                matches!(block_type, BlockType::Text | BlockType::Reasoning)
            })
            .map(assemble)
            .filter_map(|block| match block {
                Block::Text(text) => Some(Block::Text(text)),
                Block::Reasoning { text, replay } => Some(Block::Reasoning {
                    text,
                    replay: replay.filter(|_| finished),
                }),
                Block::ToolCall(_) | Block::ToolResult(_) => None,
            })
            .filter(|block| match block {
                Block::Text(text) | Block::Reasoning { text, .. } => !text.trim().is_empty(),
                _ => false,
            })
            .collect()
    }

    /// The usage, once a `usage` chunk came. (dsh: `usage`.)
    pub fn usage(&self) -> Option<Usage> {
        self.usage
    }

    /// Why the call finished, once its `finish` came. (dsh: `finish`, which
    /// defaults to a stop; a Karaka stream always ends with a finish or an
    /// error, so none means the call isn't over.)
    pub fn finish(&self) -> Option<FinishReason> {
        self.finish
    }

    /// The finished answer, once the call has finished.
    pub fn response(&self) -> Option<Response> {
        let finish = self.finish?;
        Some(Response {
            message: Message {
                role: Role::Assistant,
                content: self.blocks(),
            },
            finish,
            usage: self.usage,
        })
    }
}

fn block_type(block: &Block) -> BlockType {
    match block {
        Block::Text(_) => BlockType::Text,
        Block::Reasoning { .. } => BlockType::Reasoning,
        // A model never writes a tool result; a fake that sends one gets it
        // treated as a call it can't make.
        Block::ToolCall(_) | Block::ToolResult(_) => BlockType::ToolCall,
    }
}

/// dsh: `assemble`. An open tool call with no id is named after its index,
/// `call-<index>`, as dsh names it.
fn assemble(partial: &Partial) -> Block {
    if let Some(block) = &partial.block {
        return block.clone();
    }
    match partial.block_type {
        BlockType::Text => Block::Text(partial.text.clone()),
        BlockType::Reasoning => Block::Reasoning {
            text: partial.text.clone(),
            replay: None,
        },
        BlockType::ToolCall => Block::ToolCall(ToolCall {
            id: partial
                .tool_call_id
                .clone()
                .unwrap_or_else(|| format!("call-{}", partial.index)),
            name: partial.tool_call_name.clone().unwrap_or_default(),
            arguments: partial.tool_call_arguments.clone(),
        }),
    }
}

/// Ported from dsh's `llm/tests/assembler.spec.ts` where it fits.
#[cfg(test)]
mod tests {
    use super::BlockAssembler;
    use crate::{Block, BlockType, Event, FinishReason, Origin, Replay, ToolCall, Usage};

    fn assembled(events: &[Event]) -> BlockAssembler {
        let mut assembler = BlockAssembler::new();
        for event in events {
            assembler.push(event);
        }
        assembler
    }

    fn text_delta(index: usize, text: &str) -> Event {
        Event::TextDelta {
            index,
            text: text.into(),
        }
    }

    fn call(id: &str, arguments: &str) -> Block {
        Block::ToolCall(ToolCall {
            id: id.into(),
            name: "read".into(),
            arguments: arguments.into(),
        })
    }

    #[test]
    fn interleaved_blocks_assemble_by_their_index() {
        let answer = assembled(&[
            Event::BlockStart {
                index: 0,
                block_type: BlockType::Reasoning,
            },
            Event::BlockStart {
                index: 1,
                block_type: BlockType::Text,
            },
            text_delta(1, "Hel"),
            Event::ReasoningDelta {
                index: 0,
                text: "think".into(),
            },
            text_delta(1, "lo"),
            Event::Usage {
                usage: Usage::default(),
            },
            Event::Finish {
                reason: FinishReason::Stop,
            },
        ]);
        assert_eq!(
            answer.blocks(),
            [
                Block::Reasoning {
                    text: "think".into(),
                    replay: None
                },
                Block::Text("Hello".into())
            ]
        );
        assert_eq!(answer.usage(), Some(Usage::default()));
        assert_eq!(answer.finish(), Some(FinishReason::Stop));
    }

    /// A block's end is authoritative, and the first one wins; deltas after
    /// it are ignored.
    #[test]
    fn a_finished_block_is_frozen() {
        let answer = assembled(&[
            text_delta(0, "draft"),
            Event::BlockEnd {
                index: 0,
                block: Block::Text("final".into()),
            },
            text_delta(0, " straggler"),
            Event::BlockEnd {
                index: 0,
                block: Block::Text("again".into()),
            },
        ]);
        assert_eq!(answer.blocks(), [Block::Text("final".into())]);
        assert_eq!(answer.finish(), None);
        assert_eq!(answer.response(), None);
    }

    /// dsh: the max-tokens rule (`assembler.ts:131-141`).
    #[test]
    fn the_output_limit_drops_tool_calls() {
        let answer = assembled(&[
            text_delta(0, "Let me"),
            Event::ToolCallDelta {
                index: 1,
                id: "c1".into(),
                name: Some("read".into()),
                arguments_delta: r#"{"pa"#.into(),
            },
            Event::BlockEnd {
                index: 1,
                block: call("c1", r#"{"pa"#),
            },
            Event::Finish {
                reason: FinishReason::MaxTokens,
            },
        ]);
        assert_eq!(answer.blocks(), [Block::Text("Let me".into())]);
    }

    /// An open tool call builds from its deltas, named after its index if
    /// it has no id.
    #[test]
    fn an_open_tool_call_builds_from_its_deltas() {
        let answer = assembled(&[
            Event::BlockStart {
                index: 0,
                block_type: BlockType::ToolCall,
            },
            Event::ToolCallDelta {
                index: 0,
                id: "c1".into(),
                name: Some("read".into()),
                arguments_delta: r#"{"path":"#.into(),
            },
            Event::ToolCallDelta {
                index: 0,
                id: "c1".into(),
                name: None,
                arguments_delta: r#""a"}"#.into(),
            },
            Event::BlockStart {
                index: 1,
                block_type: BlockType::ToolCall,
            },
        ]);
        assert_eq!(
            answer.blocks(),
            [
                call("c1", r#"{"path":"a"}"#),
                Block::ToolCall(ToolCall {
                    id: "call-1".into(),
                    name: String::new(),
                    arguments: String::new(),
                })
            ]
        );
    }

    /// dsh: "cancel mid-stream finalizes the streamed prefix", "cancel
    /// drops a half-streamed tool call and keeps the completed text before
    /// it", and "cancel before any visible content finalizes nothing"
    /// (`agent-loop/tests/cancel.spec.ts:571,664,775`).
    #[test]
    fn an_interrupted_answer_keeps_only_text_and_reasoning() {
        let signed = Replay::new(Origin::new("anthropic-messages", "anthropic", "m"), "sig");
        let answer = assembled(&[
            Event::BlockEnd {
                index: 0,
                block: Block::Reasoning {
                    text: "thinking".into(),
                    replay: Some(signed),
                },
            },
            Event::BlockEnd {
                index: 1,
                block: Block::Text("reading the file".into()),
            },
            Event::ToolCallDelta {
                index: 2,
                id: "c1".into(),
                name: Some("read".into()),
                arguments_delta: r#"{"pa"#.into(),
            },
            text_delta(3, "  "),
            text_delta(4, "par"),
        ]);
        assert_eq!(
            answer.interrupted_blocks(),
            [
                Block::Reasoning {
                    text: "thinking".into(),
                    replay: None
                },
                Block::Text("reading the file".into()),
                Block::Text("par".into()),
            ]
        );
        assert!(
            assembled(&[text_delta(0, " \n")])
                .interrupted_blocks()
                .is_empty()
        );
    }
}
