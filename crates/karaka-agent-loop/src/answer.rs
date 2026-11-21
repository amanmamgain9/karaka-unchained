//! Assembling one model call's answer from its events, and what to keep of
//! an answer a cancel cut short. (dsh: `BlockAssembler` in
//! `llm/src/assembler.ts`, which the loop feeds through
//! `AssistantStreamAttempt` in `agent-loop/src/assistant-stream.ts`.)

use karaka_model::{Block, Event, StopReason};

/// The blocks of an answer so far.
///
/// A [`Model`](karaka_model::Model) sends each block whole once it is
/// finished, so a finished answer is just those blocks. The deltas only
/// matter when a cancel stops the answer partway: they hold the text of the
/// block that was still being written.
#[derive(Debug, Default)]
pub(crate) struct Answer {
    /// The finished blocks, in the order they arrived.
    finished: Vec<Block>,
    /// The text and reasoning written since their last finished block, in
    /// the order each started.
    open: Vec<Block>,
}

impl Answer {
    /// Takes in one event that isn't [`Event::Done`].
    pub(crate) fn push(&mut self, event: Event) {
        match event {
            Event::TextDelta(more) => match self.open_block(false) {
                Some(Block::Text(text)) => text.push_str(&more),
                _ => self.open.push(Block::Text(more)),
            },
            Event::ReasoningDelta(more) => match self.open_block(true) {
                Some(Block::Reasoning { text, .. }) => text.push_str(&more),
                _ => self.open.push(Block::Reasoning {
                    text: more,
                    replay: None,
                }),
            },
            Event::Block(block) => {
                // The finished block replaces the deltas that wrote it.
                let reasoning = match &block {
                    Block::Text(_) => Some(false),
                    Block::Reasoning { .. } => Some(true),
                    _ => None,
                };
                if let Some(reasoning) = reasoning
                    && let Some(index) = self.open_index(reasoning)
                {
                    self.open.remove(index);
                }
                self.finished.push(block);
            }
            Event::Done { .. } => {}
        }
    }

    /// The answer's blocks once the call finished with `stop`.
    ///
    /// A call cut off by the output limit keeps no tool calls. Providers
    /// already drop them, and `karaka_model::complete` drops them again;
    /// the loop keeps the same rule for any model, as dsh's assembler does
    /// (`assembler.ts:131-141`).
    pub(crate) fn finish(self, stop: StopReason) -> Vec<Block> {
        let mut blocks = self.finished;
        if stop == StopReason::MaxTokens {
            blocks.retain(|block| !matches!(block, Block::ToolCall(_)));
        }
        blocks
    }

    /// What to keep of an answer a cancel interrupted. (dsh:
    /// `interruptedBlocks`, `assembler.ts:159-178`.)
    ///
    /// Only text and reasoning with something other than spaces in it, the
    /// finished blocks first and then the ones still being written. Tool
    /// calls are dropped, finished or not: none of them ran, and keeping one
    /// would need a made-up result.
    ///
    /// Reasoning is kept without its signature. dsh's replay state arrives
    /// only with the stream's last chunk, so an interrupted message never
    /// has one (`agent.ts:466-482`).
    pub(crate) fn interrupted(self) -> Vec<Block> {
        self.finished
            .into_iter()
            .chain(self.open)
            .filter_map(|block| match block {
                Block::Text(text) => Some(Block::Text(text)),
                Block::Reasoning { text, .. } => Some(Block::Reasoning { text, replay: None }),
                Block::ToolCall(_) | Block::ToolResult(_) => None,
            })
            .filter(|block| match block {
                Block::Text(text) | Block::Reasoning { text, .. } => !text.trim().is_empty(),
                _ => false,
            })
            .collect()
    }

    fn open_index(&self, reasoning: bool) -> Option<usize> {
        self.open
            .iter()
            .position(|block| matches!(block, Block::Reasoning { .. }) == reasoning)
    }

    fn open_block(&mut self, reasoning: bool) -> Option<&mut Block> {
        let index = self.open_index(reasoning)?;
        Some(&mut self.open[index])
    }
}

#[cfg(test)]
mod tests {
    use karaka_model::{Block, Event, Origin, Replay, StopReason, ToolCall};

    use super::Answer;

    fn answer(events: Vec<Event>) -> Answer {
        let mut answer = Answer::default();
        for event in events {
            answer.push(event);
        }
        answer
    }

    fn call() -> Block {
        Block::ToolCall(ToolCall {
            id: "c1".into(),
            name: "read".into(),
            arguments: "{}".into(),
        })
    }

    #[test]
    fn a_finished_answer_is_its_blocks() {
        let answer = answer(vec![
            Event::TextDelta("Let me ".into()),
            Event::TextDelta("check.".into()),
            Event::Block(Block::Text("Let me check.".into())),
            Event::Block(call()),
        ]);
        assert_eq!(
            answer.finish(StopReason::ToolCalls),
            [Block::Text("Let me check.".into()), call()]
        );
    }

    #[test]
    fn the_output_limit_drops_tool_calls() {
        let answer = answer(vec![
            Event::Block(Block::Text("Let me".into())),
            Event::Block(call()),
        ]);
        assert_eq!(
            answer.finish(StopReason::MaxTokens),
            [Block::Text("Let me".into())]
        );
    }

    /// dsh: "cancel mid-stream finalizes the streamed prefix onto the
    /// surface" (`cancel.spec.ts:571`).
    #[test]
    fn an_interrupted_answer_keeps_the_text_written_so_far() {
        let answer = answer(vec![
            Event::TextDelta("part".into()),
            Event::TextDelta("ial".into()),
        ]);
        assert_eq!(answer.interrupted(), [Block::Text("partial".into())]);
    }

    /// dsh: "cancel drops a half-streamed tool call and keeps the completed
    /// text before it" (`cancel.spec.ts:664`).
    #[test]
    fn an_interrupted_answer_drops_tool_calls() {
        let answer = answer(vec![
            Event::TextDelta("reading the file".into()),
            Event::Block(Block::Text("reading the file".into())),
            Event::Block(call()),
        ]);
        assert_eq!(
            answer.interrupted(),
            [Block::Text("reading the file".into())]
        );
    }

    /// dsh: "cancel during reasoning-only streaming finalizes the reasoning
    /// prefix" (`cancel.spec.ts:640`), here without its signature.
    #[test]
    fn interrupted_reasoning_is_kept_without_its_signature() {
        let signed = Replay::new(Origin::new("anthropic-messages", "anthropic", "m"), "sig");
        let answer = answer(vec![
            Event::ReasoningDelta("thinking".into()),
            Event::Block(Block::Reasoning {
                text: "thinking".into(),
                replay: Some(signed),
            }),
            Event::ReasoningDelta("more".into()),
            Event::TextDelta("Hel".into()),
        ]);
        assert_eq!(
            answer.interrupted(),
            [
                Block::Reasoning {
                    text: "thinking".into(),
                    replay: None
                },
                Block::Reasoning {
                    text: "more".into(),
                    replay: None
                },
                Block::Text("Hel".into()),
            ]
        );
    }

    /// dsh: "cancel before any visible content finalizes nothing"
    /// (`cancel.spec.ts:775`).
    #[test]
    fn an_answer_of_only_spaces_or_tool_calls_keeps_nothing() {
        let answer = answer(vec![Event::TextDelta("  \n".into()), Event::Block(call())]);
        assert!(answer.interrupted().is_empty());
    }
}
