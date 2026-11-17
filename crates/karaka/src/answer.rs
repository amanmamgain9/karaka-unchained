//! Showing an answer while it is written: the text on stdout, everything
//! else on stderr, so `karaka ask ... > answer.txt` keeps only the answer.

use std::future::poll_fn;
use std::io::{self, IsTerminal, Write};

use karaka_model::{Event, EventStream, ModelError, StopReason, Usage};

/// Terminal escape codes that dim text, and that end the dimming.
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

/// Prints the answer's text to stdout as it arrives. Reasoning goes to
/// stderr, dimmed, but only for a person watching a terminal: in a pipe or a
/// log it would be noise, and so would the escape codes.
pub async fn print_answer(mut stream: EventStream) -> Result<(), String> {
    let show_reasoning = io::stderr().is_terminal();
    let mut stdout = io::stdout();
    let mut wrote_text = false;
    let mut in_reasoning = false;
    let write_failed = |error: io::Error| format!("couldn't write the answer: {error}");

    // `poll_fn` turns the stream's `poll_next` into something to `.await`, as
    // `karaka_model::complete` does. That needs no library of stream
    // helpers, and not even the `Stream` trait: a `dyn Stream` can call its
    // own methods without it.
    while let Some(event) = poll_fn(|cx| stream.as_mut().poll_next(cx)).await {
        // Whatever follows the reasoning starts on a line of its own.
        if in_reasoning && !matches!(event, Ok(Event::ReasoningDelta(_))) {
            eprintln!();
            in_reasoning = false;
        }
        match event {
            Ok(Event::TextDelta(text)) => {
                // Flushed every time, or the text would wait for a newline.
                stdout
                    .write_all(text.as_bytes())
                    .and_then(|()| stdout.flush())
                    .map_err(write_failed)?;
                wrote_text = true;
            }
            Ok(Event::ReasoningDelta(text)) if show_reasoning => {
                eprint!("{DIM}{text}{RESET}");
                in_reasoning = true;
            }
            Ok(Event::ReasoningDelta(_) | Event::Block(_)) => {}
            Ok(Event::Done { stop, usage }) => {
                writeln!(stdout)
                    .and_then(|()| stdout.flush())
                    .map_err(write_failed)?;
                eprintln!("{}", summary(&stop, &usage));
                return Ok(());
            }
            Err(error) => {
                // Ends the half-written answer, so the error starts on a line
                // of its own. The error matters more than this newline.
                if wrote_text {
                    let _ = writeln!(stdout);
                }
                return Err(describe(&error));
            }
        }
    }
    Err("the stream ended before the model finished".into())
}

/// The line that follows the answer, such as
/// `stop: finished · in 12 (+100 cached) · out 34 tokens`.
fn summary(stop: &StopReason, usage: &Usage) -> String {
    let stop = match stop {
        StopReason::EndTurn => "finished",
        StopReason::ToolCalls => "asked to call tools",
        StopReason::MaxTokens => "reached the token limit",
        StopReason::Other(reason) => reason,
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

    use karaka_model::{ErrorKind, ModelError, StopReason, Usage};

    use super::{describe, summary};

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
    fn the_summary_leaves_out_an_unused_cache_and_keeps_unknown_reasons() {
        let usage = Usage {
            input_tokens: 5,
            output_tokens: 0,
            ..Usage::default()
        };
        assert_eq!(
            summary(&StopReason::Other("refusal".into()), &usage),
            "stop: refusal · in 5 · out 0 tokens"
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
