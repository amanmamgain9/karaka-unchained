//! Showing an answer while it is written: the text on stdout, everything
//! else on stderr, so `karaka ask ... > answer.txt` keeps only the answer.
//!
//! The answer comes as the agent loop's live frames (`AssistantStreamFrame`),
//! each model call an attempt of its own. A retried call is a new attempt.
//! dsh's views replace a failed attempt's text with the retry's, by its
//! attempt id; a terminal can't take printed text back, so this ends the
//! failed attempt's line, and if it had printed any text, says on stderr
//! that it is trying again. That is this command's own behaviour, not the
//! loop's.

use std::io::{self, Write};

use karaka_agent::{AssistantStreamFrame, StreamChunk};
use karaka_model::{Event, ModelError, StopReason, Usage};
use karaka_session::{Failure, TurnEndReason};

/// Terminal escape codes that dim text, and that end the dimming.
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

/// Prints one turn's frames as they come.
#[derive(Debug)]
pub struct Printer {
    /// Reasoning goes to stderr, dimmed, but only for a person watching a
    /// terminal: in a pipe or a log it would be noise, and so would the
    /// escape codes.
    show_reasoning: bool,
    /// Whether stdout's current line holds answer text.
    wrote_text: bool,
    /// Whether this attempt printed any text at all.
    attempt_wrote_text: bool,
    in_reasoning: bool,
    attempts: u64,
    /// The failure the last attempt ended with, as the model gave it.
    failure: Option<ModelError>,
    /// The first write that failed. Printing stops there.
    write_error: Option<io::Error>,
}

impl Printer {
    pub fn new(show_reasoning: bool) -> Self {
        Self {
            show_reasoning,
            wrote_text: false,
            attempt_wrote_text: false,
            in_reasoning: false,
            attempts: 0,
            failure: None,
            write_error: None,
        }
    }

    /// Prints what `frame` adds.
    pub fn frame(&mut self, frame: &AssistantStreamFrame) {
        if self.write_error.is_some() {
            return;
        }
        if let Err(error) = self.print(frame) {
            self.write_error = Some(error);
        }
    }

    fn print(&mut self, frame: &AssistantStreamFrame) -> io::Result<()> {
        let mut stdout = io::stdout();
        // Whatever follows the reasoning starts on a line of its own.
        let more_reasoning = matches!(
            frame,
            AssistantStreamFrame::Chunk {
                chunk: StreamChunk::Event(Event::ReasoningDelta(_)),
                ..
            }
        );
        if self.in_reasoning && !more_reasoning {
            eprintln!();
            self.in_reasoning = false;
        }
        match frame {
            AssistantStreamFrame::Start { .. } => {
                self.attempts += 1;
                if self.attempts > 1 && self.attempt_wrote_text {
                    eprintln!("karaka: trying again");
                }
                self.attempt_wrote_text = false;
                self.failure = None;
            }
            AssistantStreamFrame::Chunk { chunk, .. } => match chunk {
                StreamChunk::Event(Event::TextDelta(text)) => {
                    // Flushed every time, or the text would wait for a
                    // newline.
                    stdout.write_all(text.as_bytes())?;
                    stdout.flush()?;
                    self.wrote_text = true;
                    self.attempt_wrote_text = true;
                }
                StreamChunk::Event(Event::ReasoningDelta(text)) if self.show_reasoning => {
                    eprint!("{DIM}{text}{RESET}");
                    self.in_reasoning = true;
                }
                StreamChunk::Event(Event::ReasoningDelta(_) | Event::Block(_)) => {}
                StreamChunk::Event(Event::Done { stop, usage }) => {
                    writeln!(stdout)?;
                    stdout.flush()?;
                    self.wrote_text = false;
                    eprintln!("{}", summary(stop, usage));
                }
                StreamChunk::Failed(error) => {
                    // Ends the half-written answer, so whatever comes next,
                    // a retry or the error, starts on a line of its own.
                    if self.wrote_text {
                        writeln!(stdout)?;
                        stdout.flush()?;
                        self.wrote_text = false;
                    }
                    self.failure = Some(error.clone());
                }
            },
            AssistantStreamFrame::End { .. } => {}
        }
        Ok(())
    }

    /// How the command ends, once the turn has: `reason` is its `turn/end`.
    /// A failed turn reports the model's own error, as the last attempt's
    /// failure chunk gave it, with whether trying again might help.
    pub fn finish(&self, reason: Option<&TurnEndReason>) -> Result<(), String> {
        if let Some(error) = &self.write_error {
            return Err(format!("couldn't write the answer: {error}"));
        }
        match reason {
            Some(TurnEndReason::Completed | TurnEndReason::MaxTokens) => Ok(()),
            Some(TurnEndReason::Error(failure)) => Err(match &self.failure {
                Some(error) => describe(error),
                None => other_failure(failure),
            }),
            Some(TurnEndReason::Aborted(_)) => Err("the turn was cancelled".into()),
            Some(other) => Err(format!("the turn ended: {other:?}")),
            None => Err("the turn didn't end".into()),
        }
    }
}

/// A failure that wasn't the model's, such as a request-error handler's.
fn other_failure(failure: &Failure) -> String {
    format!("{} ({})", failure.message, failure.code)
}

/// The line that follows the answer, such as
/// `stop: finished · in 12 (+100 cached) · out 34 tokens`.
fn summary(stop: &StopReason, usage: &Usage) -> String {
    let stop = match stop {
        StopReason::EndTurn => "finished",
        StopReason::ToolCalls => "asked to call tools",
        StopReason::MaxTokens => "reached the token limit",
        StopReason::Paused => "paused before finishing",
    };
    let mut cache = Vec::new();
    if usage.cache_read_tokens > 0 {
        cache.push(format!("+{} cached", usage.cache_read_tokens));
    }
    if usage.cache_write_tokens > 0 {
        cache.push(format!("+{} written to cache", usage.cache_write_tokens));
    }
    let cache = if cache.is_empty() {
        String::new()
    } else {
        format!(" ({})", cache.join(", "))
    };
    format!(
        "stop: {stop} · in {}{cache} · out {} tokens",
        usage.input_tokens, usage.output_tokens
    )
}

/// A failed call, and whether trying again might help.
fn describe(error: &ModelError) -> String {
    let hint = if error.kind.is_retryable() {
        "trying again may help"
    } else {
        "trying again won't help"
    };
    match error.retry_after {
        Some(wait) => format!("{error} ({hint}; the provider asked to wait {wait:?})"),
        None => format!("{error} ({hint})"),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use karaka_agent::{AssistantStreamFrame, StreamChunk};
    use karaka_model::{ErrorKind, ModelError, Refusal, StopReason, Usage};
    use karaka_session::{Failure, TurnEndReason};

    use super::{Printer, describe, summary};

    fn failed(error: ModelError) -> AssistantStreamFrame {
        AssistantStreamFrame::Chunk {
            attempt_id: "ask:1".into(),
            revision: 2,
            index: 0,
            time: karaka_session::Time(1),
            chunk: StreamChunk::Failed(error),
        }
    }

    fn failure(message: &str, code: &str) -> TurnEndReason {
        TurnEndReason::Error(Failure {
            message: message.into(),
            code: code.into(),
            status: None,
            provider_retry_after_ms: None,
            request_id: None,
            offload_images: None,
        })
    }

    /// The error line is the model's own error, from the failure chunk, so
    /// a refusal still names its category, which a logged failure can't.
    #[test]
    fn a_failed_turn_reports_the_models_own_error() {
        let mut refused = ModelError::new(ErrorKind::Refused, "No.");
        refused.refusal = Some(Refusal {
            category: Some("cyber".into()),
            explanation: None,
        });
        let mut printer = Printer::new(false);
        printer.frame(&failed(refused));
        assert_eq!(
            printer.finish(Some(&failure("No.", "REFUSED"))),
            Err(
                "the model declined to answer: No. (category: cyber) (trying again won't help)"
                    .into()
            )
        );
    }

    #[test]
    fn a_turn_that_failed_outside_the_model_says_so() {
        let printer = Printer::new(false);
        assert_eq!(
            printer.finish(Some(&failure("recovery failed", "UNKNOWN"))),
            Err("recovery failed (UNKNOWN)".into())
        );
        assert_eq!(printer.finish(Some(&TurnEndReason::Completed)), Ok(()));
    }

    #[test]
    fn the_summary_reads_as_a_sentence() {
        let usage = Usage {
            input_tokens: 12,
            output_tokens: 34,
            cache_read_tokens: 100,
            cache_write_tokens: 0,
        };
        assert_eq!(
            summary(&StopReason::EndTurn, &usage),
            "stop: finished · in 12 (+100 cached) · out 34 tokens"
        );
    }

    #[test]
    fn the_summary_leaves_out_an_unused_cache() {
        let usage = Usage {
            input_tokens: 5,
            output_tokens: 0,
            ..Usage::default()
        };
        assert_eq!(
            summary(&StopReason::MaxTokens, &usage),
            "stop: reached the token limit · in 5 · out 0 tokens"
        );
    }

    #[test]
    fn a_refusal_is_an_error_that_trying_again_wont_fix() {
        let error = ModelError::new(ErrorKind::Refused, "Anthropic gave no explanation");
        assert_eq!(
            describe(&error),
            "the model declined to answer: Anthropic gave no explanation (trying again won't help)"
        );
    }

    #[test]
    fn a_rate_limit_says_to_try_again_and_when() {
        let mut error = ModelError::new(ErrorKind::RateLimited, "Slow down.");
        error.retry_after = Some(Duration::from_secs(7));
        assert_eq!(
            describe(&error),
            "rate limited: Slow down. (trying again may help; the provider asked to wait 7s)"
        );
    }

    #[test]
    fn a_refused_key_says_trying_again_wont_help() {
        let error = ModelError::new(ErrorKind::Auth, "invalid x-api-key");
        assert_eq!(
            describe(&error),
            "the provider refused the credentials: invalid x-api-key (trying again won't help)"
        );
    }
}
