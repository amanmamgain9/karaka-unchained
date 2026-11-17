//! `karaka`, the Karaka runtime binary.
//!
//! For now it does one thing: `karaka ask <question>` starts a runtime with
//! one model provider, asks the model, and prints the answer as it arrives.

mod answer;
mod config;

use std::ffi::OsString;
use std::io::{self, IsTerminal};
use std::process::ExitCode;

use karaka_kernel::Runtime;
use karaka_model::{Message, Model, Request};

use crate::answer::print_answer;
use crate::config::Config;

const USAGE: &str = "\
Usage: karaka ask <question...>
       karaka ask [-]

Asks a model one question and prints the answer as it is written.
With no question, or with -, the question is read from stdin.

Settings come from the environment:
  KARAKA_PROVIDER     anthropic (the default) or openai
  KARAKA_MODEL        the model's id; for anthropic, claude-opus-5-5 by default
  KARAKA_SYSTEM       a system prompt (optional)
  KARAKA_MAX_TOKENS   the most the model may write (optional)
  ANTHROPIC_API_KEY   needed for anthropic
  ANTHROPIC_BASE_URL  another server for anthropic (optional)
  OPENAI_BASE_URL     needed for openai, such as https://api.openai.com/v1
  OPENAI_API_KEY      for openai (optional)
";

/// What the command line asks for.
#[derive(Debug, PartialEq)]
enum Command {
    /// Ask the question given on the command line.
    Ask(String),
    /// Ask a question read from stdin.
    AskFromStdin,
    /// Print the usage text: for `--help`, or a command line that isn't right.
    Usage,
}

/// Reads the arguments that follow the program's name.
fn parse_args(args: &[String]) -> Command {
    match args {
        [command, rest @ ..] if command == "ask" => match rest {
            [] => Command::AskFromStdin,
            [dash] if dash == "-" => Command::AskFromStdin,
            words => Command::Ask(words.join(" ")),
        },
        _ => Command::Usage,
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    // `args_os`, because `args` panics on an argument that isn't UTF-8.
    let args: Result<Vec<String>, OsString> = std::env::args_os()
        .skip(1)
        .map(OsString::into_string)
        .collect();
    let question = match args.map(|args| parse_args(&args)) {
        Ok(Command::Ask(question)) => Some(question),
        Ok(Command::AskFromStdin) => None,
        Ok(Command::Usage) | Err(_) => {
            eprint!("{USAGE}");
            return ExitCode::from(2);
        }
    };

    match ask(question).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("karaka: {message}");
            ExitCode::FAILURE
        }
    }
}

/// Asks the model one question and prints the answer. With no question, it
/// is read from stdin.
async fn ask(question: Option<String>) -> Result<(), String> {
    // The settings are read before stdin, so a mistake in them shows at once,
    // not after someone has typed out a question.
    let config = Config::from_env(|name| std::env::var(name).ok())?;
    let question = match question {
        Some(question) => question,
        None => read_stdin()?,
    };
    let question = question.trim();
    if question.is_empty() {
        return Err("the question is empty".into());
    }

    let runtime = Runtime::start(config.provider.plugins())
        .await
        .map_err(|error| error.to_string())?;
    let model = runtime.get::<dyn Model>();

    let mut request = Request::new(config.model, vec![Message::user(question)]);
    request.system = config.system;
    request.max_tokens = config.max_tokens;
    print_answer(model.stream(request)).await
}

/// All of stdin, as the question.
fn read_stdin() -> Result<String, String> {
    let stdin = io::stdin();
    if stdin.is_terminal() {
        eprintln!("Type the question, then press Ctrl-D.");
    }
    // This blocks the thread, but nothing else is running yet to be held up.
    io::read_to_string(stdin).map_err(|error| format!("couldn't read the question: {error}"))
}

#[cfg(test)]
mod tests {
    use super::{Command, parse_args};

    fn parse(args: &[&str]) -> Command {
        let args: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
        parse_args(&args)
    }

    #[test]
    fn the_words_after_ask_are_the_question() {
        assert_eq!(
            parse(&["ask", "what", "is", "2 + 2?"]),
            Command::Ask("what is 2 + 2?".into())
        );
    }

    #[test]
    fn no_question_or_a_dash_reads_stdin() {
        assert_eq!(parse(&["ask"]), Command::AskFromStdin);
        assert_eq!(parse(&["ask", "-"]), Command::AskFromStdin);
    }

    #[test]
    fn a_dash_among_other_words_is_part_of_the_question() {
        assert_eq!(parse(&["ask", "-", "why"]), Command::Ask("- why".into()));
    }

    #[test]
    fn anything_else_gets_the_usage_text() {
        assert_eq!(parse(&[]), Command::Usage);
        assert_eq!(parse(&["--help"]), Command::Usage);
        assert_eq!(parse(&["-h"]), Command::Usage);
        assert_eq!(parse(&["chat", "hello"]), Command::Usage);
    }
}
