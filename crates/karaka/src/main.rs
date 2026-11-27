//! `karaka`, the Karaka runtime binary.
//!
//! For now it does two things, both through the agent loop, with one model
//! provider and the retry plugin. `karaka ask <question>` asks one question
//! in a turn of its own and prints the answer as it is written. `karaka
//! chat` adds one tool and holds a conversation read line by line from
//! stdin.

mod answer;
mod chat;
mod config;
mod current_time;
mod questions;

use std::ffi::OsString;
use std::io::{self, BufReader, IsTerminal};
use std::process::ExitCode;
use std::sync::Arc;

use std::sync::{Mutex, PoisonError};

use karaka_agent::Agents;
use karaka_kernel::Runtime;
use karaka_model::{Model, ModelRegistry};
use karaka_session::{SessionEvent, SessionId, SessionStore, UserMessage, UserSource};

use crate::answer::Printer;
use crate::chat::Approvals;
use crate::config::Config;
use crate::questions::Lines;

const USAGE: &str = "\
Usage: karaka ask [options] <question...>
       karaka ask [options] [-]
       karaka chat [options]

ask asks a model one question and prints the answer as it is written.
With no question, or with -, the question is read from stdin. A request
that fails for a reason worth retrying is tried again first.

chat reads messages from stdin, one per line, and answers each through
the agent loop and its tools. Each tool call is printed on a line of its
own, then the answer. A request that fails for a reason worth retrying,
such as a rate limit, is tried again first. With KARAKA_SESSIONS set, the
chat is stored there, and the next chat in the same session carries on
the conversation.

Both load skills: a directory holding a SKILL.md file, under the
project's .dsh/skills or .agents/skills, or ~/.dsh/skills or
~/.agents/skills. The model is shown each skill's name and description
and loads the ones a task needs; /name in a message loads that skill for
the model at once. A chat sees skills added or changed while it runs.

Either may ask you a question on stderr. Answer it on stdin with an
option's number (numbers separated by commas where several may be
picked), or type your own answer; an empty line skips it.

Options, before the question:
  --ask-before <tool>       ask for your approval, on stderr, before each
                            call to <tool>; y allows it once, anything else
                            rejects it. May be given more than once
  --approval-policy <p>     ask (the default) asks you; never rejects
                            every approval without asking

Settings come from the environment:
  KARAKA_PROVIDER     anthropic (the default), deepseek or openai. deepseek
                      speaks Anthropic's Messages format, as dsh does;
                      openai is for Chat Completions servers such as
                      OpenAI and Ollama
  KARAKA_ROUTE        the name the provider's model is registered under,
                      such as deepseek or ollama (optional; the provider's
                      name otherwise)
  KARAKA_MODEL        the model's id; by default claude-opus-5-5 for
                      anthropic and deepseek-flash for deepseek
  KARAKA_SYSTEM       the deployment's persona: the system prompt after
                      Karaka's identity line (optional)
  KARAKA_MAX_TOKENS   the most the model may write (optional; deepseek
                      sends 256000 otherwise)
  KARAKA_EFFORT       low, medium, high, xhigh or max (optional; the
                      provider's default otherwise). deepseek takes only
                      low, high or max, and high is its default
  KARAKA_MAX_RETRIES  how many times a request that failed for a reason
                      worth retrying, such as a rate limit, is retried
                      (optional; 5 otherwise). 0 turns retries off
  KARAKA_SESSIONS     a directory chat keeps its sessions in, one file
                      each (optional; chats are kept in memory otherwise)
  KARAKA_SESSION      the name of chat's session (optional; chat otherwise)
  ANTHROPIC_API_KEY   needed for anthropic
  ANTHROPIC_BASE_URL  another server for anthropic (optional)
  ANTHROPIC_THINKING_DISPLAY
                      summarized (the default) or omitted
  ANTHROPIC_CACHE     short (the default, 5 minutes), long (1 hour) or off
  DEEPSEEK_API_KEY    needed for deepseek
  DEEPSEEK_BASE_URL   another server for deepseek (optional; by default
                      https://api.deepseek.com/anthropic)
  DEEPSEEK_THINKING   enabled (the default) or disabled; disabled takes
                      no KARAKA_EFFORT
  OPENAI_BASE_URL     needed for openai, such as https://api.openai.com/v1
  OPENAI_API_KEY      for openai (optional)
  DSH_HOME            where ~/.dsh is: its skills are DSH_HOME/skills
                      (optional)
  DSH_AGENTS_HOME     where ~/.agents is: its skills are
                      DSH_AGENTS_HOME/skills (optional)
  DSH_BUNDLED_SKILL_DIR
                      one more directory of skills, after the others
                      (optional)
";

/// What the command line asks for.
#[derive(Debug, PartialEq)]
enum Command {
    /// Ask the question given on the command line.
    Ask(String, Approvals),
    /// Ask a question read from stdin.
    AskFromStdin(Approvals),
    /// Hold a conversation read from stdin.
    Chat(Approvals),
    /// Print the usage text: for `--help`, or a command line that isn't right.
    Usage,
    /// Print this, then the usage text.
    Invalid(String),
}

/// Reads the arguments that follow the program's name.
fn parse_args(args: &[String]) -> Command {
    let Some((command, rest)) = args.split_first() else {
        return Command::Usage;
    };
    if command != "ask" && command != "chat" {
        return Command::Usage;
    }
    let (approvals, words) = match options(rest) {
        Ok(parsed) => parsed,
        Err(message) => return Command::Invalid(message),
    };
    match (command.as_str(), words) {
        ("ask", []) => Command::AskFromStdin(approvals),
        ("ask", [dash]) if dash == "-" => Command::AskFromStdin(approvals),
        ("ask", words) => Command::Ask(words.join(" "), approvals),
        ("chat", []) => Command::Chat(approvals),
        _ => Command::Usage,
    }
}

/// The options at the front of `args`, and the words after them. `--`
/// ends the options.
fn options(mut args: &[String]) -> Result<(Approvals, &[String]), String> {
    let mut approvals = Approvals::default();
    while let Some((first, rest)) = args.split_first() {
        let (name, inline) = match first.split_once('=') {
            Some((name, value)) if name.starts_with("--") => (name, Some(value.to_owned())),
            _ => (first.as_str(), None),
        };
        if name == "--" {
            return Ok((approvals, rest));
        }
        if name != "--ask-before" && name != "--approval-policy" {
            break;
        }
        let (value, rest) = match inline {
            Some(value) => (value, rest),
            None => match rest.split_first() {
                Some((value, rest)) => (value.clone(), rest),
                None => return Err(format!("{name} needs a value")),
            },
        };
        if value.is_empty() {
            return Err(format!("{name} needs a value"));
        }
        if name == "--ask-before" {
            approvals.ask_before.push(value);
        } else {
            approvals.policy = value.parse().map_err(|error| format!("{error}"))?;
        }
        args = rest;
    }
    Ok((approvals, args))
}

#[tokio::main]
async fn main() -> ExitCode {
    // `args_os`, because `args` panics on an argument that isn't UTF-8.
    let args: Result<Vec<String>, OsString> = std::env::args_os()
        .skip(1)
        .map(OsString::into_string)
        .collect();
    let result = match args.map(|args| parse_args(&args)) {
        Ok(Command::Ask(question, approvals)) => ask(Some(question), approvals).await,
        Ok(Command::AskFromStdin(approvals)) => ask(None, approvals).await,
        Ok(Command::Chat(approvals)) => match Config::from_env(|name| std::env::var(name).ok()) {
            Ok(config) => chat::chat(config, approvals).await,
            Err(message) => Err(message),
        },
        Ok(Command::Invalid(message)) => {
            eprintln!("karaka: {message}");
            eprint!("{USAGE}");
            return ExitCode::from(2);
        }
        Ok(Command::Usage) | Err(_) => {
            eprint!("{USAGE}");
            return ExitCode::from(2);
        }
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("karaka: {message}");
            ExitCode::FAILURE
        }
    }
}

/// Asks the model one question and prints the answer. With no question, it
/// is read from stdin.
async fn ask(question: Option<String>, approvals: Approvals) -> Result<(), String> {
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

    // The question is a one-turn chat on a session of its own, so it goes
    // through the agent loop and is retried as the provider's policy says.
    // The answer is printed from the loop's live frames as it is written.
    let route = config.provider.route().to_owned();
    let runtime = Runtime::start(chat::loop_plugins(config, &approvals, false))
        .await
        .map_err(|error| error.to_string())?;
    // Checked before the turn, so an unknown route lists the routes there
    // are, rather than failing the turn.
    model_at(&runtime.get::<ModelRegistry>(), &route)?;
    // The model's questions are put on the terminal, and answered from
    // stdin, which `ask` otherwise reads only for a question it wasn't
    // given: `printf '1\n' | karaka ask "…"` picks the first option.
    chat::answer_on_the_terminal(
        &runtime,
        &Arc::new(Lines::new(BufReader::new(io::stdin()))),
        &approvals,
    );

    let id = SessionId::new("ask");
    let agent = runtime
        .get::<dyn Agents>()
        .open(id.clone())
        .map_err(|error| error.to_string())?;
    let printer = Arc::new(Mutex::new(Printer::new(io::stderr().is_terminal())));
    let printing = Arc::clone(&printer);
    let _subscription = agent.subscribe(Arc::new(move |frame| {
        printing
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .frame(frame);
    }));
    agent
        .followup(UserMessage::text("user-0", UserSource::user(), question))
        .map_err(|error| format!("couldn't send the question: {error}"))?;
    agent.when_idle().await;

    let ending = runtime
        .get::<SessionStore>()
        .events(&id)
        .map_err(|error| error.to_string())?
        .into_iter()
        .rev()
        .find_map(|entry| match entry.event {
            SessionEvent::TurnEnd(end) => Some(end.reason),
            _ => None,
        });
    printer
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .finish(ending.as_ref())
}

/// The model registered under `route`, or an error that lists the routes
/// there are.
fn model_at(registry: &ModelRegistry, route: &str) -> Result<Arc<dyn Model>, String> {
    registry.get(route).ok_or_else(|| {
        let routes = registry.routes();
        if routes.is_empty() {
            format!("no model is registered under the route \"{route}\"; no routes are registered")
        } else {
            format!(
                "no model is registered under the route \"{route}\"; the routes are: {}",
                routes.join(", ")
            )
        }
    })
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
    use std::sync::Arc;

    use karaka_model::ModelRegistry;
    use karaka_openai::OpenAiModel;

    use karaka_user_approval::ApprovalPolicy;

    use super::{Approvals, Command, model_at, parse_args};

    fn parse(args: &[&str]) -> Command {
        let args: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
        parse_args(&args)
    }

    #[test]
    fn the_words_after_ask_are_the_question() {
        assert_eq!(
            parse(&["ask", "what", "is", "2 + 2?"]),
            Command::Ask("what is 2 + 2?".into(), Approvals::default())
        );
    }

    #[test]
    fn no_question_or_a_dash_reads_stdin() {
        assert_eq!(parse(&["ask"]), Command::AskFromStdin(Approvals::default()));
        assert_eq!(
            parse(&["ask", "-"]),
            Command::AskFromStdin(Approvals::default())
        );
    }

    #[test]
    fn a_dash_among_other_words_is_part_of_the_question() {
        assert_eq!(
            parse(&["ask", "-", "why"]),
            Command::Ask("- why".into(), Approvals::default())
        );
    }

    #[test]
    fn chat_takes_no_arguments() {
        assert_eq!(parse(&["chat"]), Command::Chat(Approvals::default()));
        assert_eq!(parse(&["chat", "hello"]), Command::Usage);
    }

    #[test]
    fn approval_options_come_before_the_question() {
        let approvals = Approvals {
            policy: ApprovalPolicy::Never,
            ask_before: vec!["working_directory".into(), "todo_write".into()],
        };
        assert_eq!(
            parse(&[
                "ask",
                "--ask-before",
                "working_directory",
                "--approval-policy=never",
                "--ask-before=todo_write",
                "where",
                "am",
                "I?"
            ]),
            Command::Ask("where am I?".into(), approvals.clone())
        );
        assert_eq!(
            parse(&[
                "chat",
                "--approval-policy",
                "never",
                "--ask-before",
                "working_directory",
                "--ask-before",
                "todo_write"
            ]),
            Command::Chat(approvals)
        );
        // `--` ends the options, and an option after the question is a word.
        assert_eq!(
            parse(&["ask", "--", "--ask-before", "x"]),
            Command::Ask("--ask-before x".into(), Approvals::default())
        );
        assert_eq!(
            parse(&["ask", "why", "--ask-before", "x"]),
            Command::Ask("why --ask-before x".into(), Approvals::default())
        );
    }

    #[test]
    fn a_bad_approval_option_is_refused() {
        assert_eq!(
            parse(&["chat", "--approval-policy", "sometimes"]),
            Command::Invalid("approval policy must be one of \"ask\" or \"never\"".into())
        );
        assert_eq!(
            parse(&["ask", "--ask-before"]),
            Command::Invalid("--ask-before needs a value".into())
        );
        assert_eq!(
            parse(&["ask", "--ask-before=", "why"]),
            Command::Invalid("--ask-before needs a value".into())
        );
    }

    #[test]
    fn anything_else_gets_the_usage_text() {
        assert_eq!(parse(&[]), Command::Usage);
        assert_eq!(parse(&["--help"]), Command::Usage);
        assert_eq!(parse(&["-h"]), Command::Usage);
        assert_eq!(parse(&["chat", "hello"]), Command::Usage);
    }

    #[test]
    fn an_unknown_route_is_an_error_that_lists_the_routes() {
        let registry = ModelRegistry::new();
        let Err(error) = model_at(&registry, "deepseek") else {
            panic!("expected no model");
        };
        assert_eq!(
            error,
            "no model is registered under the route \"deepseek\"; no routes are registered"
        );

        for route in ["anthropic", "ollama"] {
            let model = OpenAiModel::new("http://127.0.0.1:1");
            registry.register(route, Arc::new(model)).unwrap();
        }
        let Err(error) = model_at(&registry, "deepseek") else {
            panic!("expected no model");
        };
        assert_eq!(
            error,
            "no model is registered under the route \"deepseek\"; the routes are: anthropic, ollama"
        );
        assert!(model_at(&registry, "ollama").is_ok());
    }
}
