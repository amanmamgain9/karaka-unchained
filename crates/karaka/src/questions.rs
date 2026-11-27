//! Questions the model asks, put to the user on the terminal.
//!
//! [`TerminalAnswerer`] is the user-questions service's answerer for the
//! binary: it writes each question to stderr, so stdout stays the model's
//! answer, and reads the answer as a line of stdin. It is the end of the
//! chain, so it answers every request. (dsh's terminal UIs are other
//! packages; this one is Karaka's own.)
//!
//! # One reader of stdin
//!
//! `chat` reads its messages from stdin too, so stdin has one reader,
//! [`Lines`], which both share. It reads a line only when someone asks for
//! one, so a line goes to whoever is waiting for it. `chat` asks for its
//! next message only once the agent is idle, and a question is asked only
//! during a turn, so the two never wait at once: while a question is open,
//! the next lines answer it, and the line after them is the next message.
//! Nothing is read ahead, so a piped stdin is split the same way: with
//! `printf '1\n' | karaka ask "…"`, the `1` answers the first question.
//!
//! When stdin ends with a question open, the answer is `ASK_ABORTED`, and
//! the tool's result says so; nothing waits.
//!
//! # Approvals
//!
//! [`TerminalApprover`] is the approval service's answerer, on the same
//! reader: it names the tool and the reason on stderr, and reads `y` or `n`.
//! Anything but `y` or `yes` rejects, an empty line included, so the
//! default is no; the end of stdin cancels.

use std::io::{self, BufRead, Write};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, PoisonError};

use karaka_user_approval::{
    ApprovalAnswerer, ApprovalFuture, ApprovalNext, ApprovalOutcome, ApprovalRequest,
};
use karaka_user_questions::{
    AnswerFuture, AnswerNext, AskUserQuestionAnswer, AskUserQuestionAnswerItem,
    AskUserQuestionItem, QuestionAnswerer, QuestionError, QuestionRequest, UserQuestionError,
};
use tokio::sync::oneshot;

/// One line, without its line ending; `None` once stdin has ended.
pub type Line = io::Result<Option<String>>;

/// The one reader of stdin, which reads a line only when asked for one.
///
/// The reading is done on a thread of its own, so a read that blocks holds
/// up no task, and an exit doesn't wait for it. A line read for a caller
/// that stopped waiting, such as a cancelled question, is kept for the
/// next caller.
#[derive(Debug)]
pub struct Lines {
    requests: mpsc::Sender<oneshot::Sender<Line>>,
}

impl Lines {
    /// A reader of `source`, such as stdin.
    pub fn new(mut source: impl BufRead + Send + 'static) -> Self {
        let (requests, waiting) = mpsc::channel::<oneshot::Sender<Line>>();
        std::thread::Builder::new()
            .name("stdin".into())
            .spawn(move || {
                let mut kept: Option<Line> = None;
                for reply in waiting {
                    let line = kept.take().unwrap_or_else(|| read_line(&mut source));
                    if let Err(line) = reply.send(line) {
                        kept = Some(line);
                    }
                }
            })
            .expect("the stdin thread starts");
        Self { requests }
    }

    /// The next line.
    pub async fn next(&self) -> Line {
        let (reply, line) = oneshot::channel();
        if self.requests.send(reply).is_err() {
            return Ok(None);
        }
        line.await.unwrap_or(Ok(None))
    }
}

fn read_line(source: &mut impl BufRead) -> Line {
    let mut line = String::new();
    if source.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    let end = line.trim_end_matches(['\n', '\r']).len();
    line.truncate(end);
    Ok(Some(line))
}

/// Puts each question to the user and reads the answers, one line each.
pub struct TerminalAnswerer {
    lines: Arc<Lines>,
    out: Mutex<Box<dyn Write + Send>>,
}

impl TerminalAnswerer {
    /// An answerer that reads from `lines` and writes to `out`, stderr in
    /// the binary.
    pub fn new(lines: Arc<Lines>, out: Box<dyn Write + Send>) -> Self {
        Self {
            lines,
            out: Mutex::new(out),
        }
    }

    fn write(&self, text: &str) -> Result<(), QuestionError> {
        let mut out = self.out.lock().unwrap_or_else(PoisonError::into_inner);
        out.write_all(text.as_bytes())
            .and_then(|()| out.flush())
            .map_err(QuestionError::other)
    }

    async fn ask(&self, request: &QuestionRequest) -> Result<AskUserQuestionAnswer, QuestionError> {
        let total = request.questions.len();
        let mut answers = Vec::with_capacity(total);
        for (index, question) in request.questions.iter().enumerate() {
            self.write(&render(question, index, total))?;
            let line = match &request.signal {
                Some(signal) => tokio::select! {
                    line = self.lines.next() => line,
                    () = signal.wait() => return Err(UserQuestionError::aborted(None).into()),
                },
                None => self.lines.next().await,
            };
            match line.map_err(QuestionError::other)? {
                Some(line) => answers.push(parse(question, &line)),
                // stdin ended: nobody is left to answer.
                None => {
                    self.write("\n")?;
                    return Err(UserQuestionError::aborted(None).into());
                }
            }
        }
        Ok(AskUserQuestionAnswer { answers })
    }
}

impl QuestionAnswerer for TerminalAnswerer {
    fn answer<'a>(&'a self, request: &'a QuestionRequest, _: AnswerNext<'a>) -> AnswerFuture<'a> {
        Box::pin(self.ask(request))
    }
}

/// Asks the user to approve each request, on the terminal.
pub struct TerminalApprover {
    lines: Arc<Lines>,
    out: Mutex<Box<dyn Write + Send>>,
}

impl TerminalApprover {
    /// An approver that reads from `lines` and writes to `out`, stderr in
    /// the binary.
    pub fn new(lines: Arc<Lines>, out: Box<dyn Write + Send>) -> Self {
        Self {
            lines,
            out: Mutex::new(out),
        }
    }

    async fn decide(&self, request: &ApprovalRequest) -> io::Result<ApprovalOutcome> {
        {
            let mut out = self.out.lock().unwrap_or_else(PoisonError::into_inner);
            out.write_all(render_approval(request).as_bytes())?;
            out.flush()?;
        }
        let line = match &request.signal {
            Some(signal) => tokio::select! {
                line = self.lines.next() => line?,
                () = signal.wait() => return Ok(ApprovalOutcome::Cancelled),
            },
            None => self.lines.next().await?,
        };
        Ok(match line {
            Some(line) => parse_approval(&line),
            // stdin ended: nobody is left to decide.
            None => ApprovalOutcome::Cancelled,
        })
    }
}

impl ApprovalAnswerer for TerminalApprover {
    fn answer<'a>(
        &'a self,
        request: &'a ApprovalRequest,
        _: ApprovalNext<'a>,
    ) -> ApprovalFuture<'a> {
        Box::pin(async move { Ok(self.decide(request).await?) })
    }
}

/// How an approval shows: the tool, the reason, shown in English when
/// there is one to display, and the prompt.
pub fn render_approval(request: &ApprovalRequest) -> String {
    let mut text = format!("\nAllow tool \"{}\"?\n", request.tool_name);
    let reason = request
        .display_reason
        .as_ref()
        .map(|reason| reason.en.as_str())
        .or(request.reason.as_deref());
    if let Some(reason) = reason {
        text.push_str(&format!("  Reason: {reason}\n"));
    }
    text.push_str("Allow it once? [y/N] ");
    text
}

/// `y` or `yes`, in any case, allows once; anything else rejects.
pub fn parse_approval(line: &str) -> ApprovalOutcome {
    match line.trim().to_lowercase().as_str() {
        "y" | "yes" => ApprovalOutcome::AllowedOnce,
        _ => ApprovalOutcome::Rejected,
    }
}

/// How one question shows: its place when there are several, its header,
/// the question, its detail, its numbered options, and how to answer.
pub fn render(question: &AskUserQuestionItem, index: usize, total: usize) -> String {
    let mut text = String::from("\n");
    if total > 1 {
        text.push_str(&format!("({}/{total}) ", index + 1));
    }
    if let Some(header) = &question.header {
        text.push_str(&format!("[{header}] "));
    }
    text.push_str(&question.question);
    text.push('\n');
    if let Some(detail) = &question.detail {
        for line in detail.lines() {
            text.push_str(&format!("  {line}\n"));
        }
    }
    let options = question.options.as_deref().unwrap_or_default();
    for (number, option) in options.iter().enumerate() {
        text.push_str(&format!("  {}. {}", number + 1, option.label));
        if let Some(description) = &option.description {
            text.push_str(&format!(" - {description}"));
        }
        text.push('\n');
    }
    text.push_str(
        match (options.is_empty(), question.multi_select == Some(true)) {
            (true, _) => "Type your answer. Enter skips.\n",
            (false, false) => "Pick a number, or type your own answer. Enter skips.\n",
            (false, true) => {
                "Pick numbers separated by commas, or type your own answer. Enter skips.\n"
            }
        },
    );
    text.push_str("> ");
    text
}

/// Reads one answer:
/// - an empty line skips the question: nothing selected, no custom text;
/// - an option's number picks it, and for a multi-select question,
///   numbers separated by commas pick each, in the order given, once;
/// - any other line is the user's own answer, as `custom`.
pub fn parse(question: &AskUserQuestionItem, line: &str) -> AskUserQuestionAnswerItem {
    let line = line.trim();
    let answer = |selected: Vec<String>, custom: Option<String>| AskUserQuestionAnswerItem {
        id: question.id.clone(),
        selected,
        custom,
    };
    if line.is_empty() {
        return answer(Vec::new(), None);
    }
    let options = question.options.as_deref().unwrap_or_default();
    let numbers: Option<Vec<usize>> = line
        .split(',')
        .map(|part| {
            part.trim()
                .parse::<usize>()
                .ok()
                .filter(|number| (1..=options.len()).contains(number))
        })
        .collect();
    match numbers {
        Some(numbers) if numbers.len() == 1 || question.multi_select == Some(true) => {
            let mut selected: Vec<String> = Vec::new();
            for number in numbers {
                let label = &options[number - 1].label;
                if !selected.contains(label) {
                    selected.push(label.clone());
                }
            }
            answer(selected, None)
        }
        _ => answer(Vec::new(), Some(line.to_owned())),
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::sync::Arc;

    use karaka_user_questions::{
        ASK_ABORTED, AskUserQuestionAnswerItem, AskUserQuestionItem, AskUserQuestionOption,
        QuestionRequest,
    };

    use super::{Lines, TerminalAnswerer, parse, render};

    fn choice(multi_select: bool) -> AskUserQuestionItem {
        AskUserQuestionItem {
            header: Some("Setup".into()),
            options: Some(vec![
                AskUserQuestionOption {
                    label: "pnpm (Recommended)".into(),
                    description: Some("Workspace default.".into()),
                },
                AskUserQuestionOption::new("npm"),
                AskUserQuestionOption::new("bun"),
            ]),
            multi_select: Some(multi_select),
            ..AskUserQuestionItem::new("pkg", "Which package manager?")
        }
    }

    fn picked(labels: &[&str]) -> AskUserQuestionAnswerItem {
        AskUserQuestionAnswerItem {
            id: "pkg".into(),
            selected: labels.iter().map(|label| (*label).to_owned()).collect(),
            custom: None,
        }
    }

    fn typed(text: &str) -> AskUserQuestionAnswerItem {
        AskUserQuestionAnswerItem {
            id: "pkg".into(),
            selected: Vec::new(),
            custom: Some(text.into()),
        }
    }

    #[test]
    fn a_number_picks_its_option() {
        assert_eq!(parse(&choice(false), "2"), picked(&["npm"]));
        assert_eq!(
            parse(&choice(false), " 1 "),
            picked(&["pnpm (Recommended)"])
        );
    }

    #[test]
    fn a_multi_select_takes_numbers_separated_by_commas() {
        assert_eq!(
            parse(&choice(true), "3, 1"),
            picked(&["bun", "pnpm (Recommended)"])
        );
        assert_eq!(parse(&choice(true), "2,2"), picked(&["npm"]));
        // A single-select question takes one number; several are text.
        assert_eq!(parse(&choice(false), "1,2"), typed("1,2"));
    }

    #[test]
    fn anything_else_is_the_users_own_answer() {
        assert_eq!(parse(&choice(false), "yarn"), typed("yarn"));
        // A number that names no option is text too.
        assert_eq!(parse(&choice(false), "4"), typed("4"));
        assert_eq!(parse(&choice(true), "1, x"), typed("1, x"));
        let open = AskUserQuestionItem::new("pkg", "Which package manager?");
        assert_eq!(parse(&open, "1"), typed("1"));
    }

    #[test]
    fn an_empty_line_skips_the_question() {
        assert_eq!(parse(&choice(false), ""), picked(&[]));
        assert_eq!(parse(&choice(false), "   "), picked(&[]));
    }

    #[test]
    fn a_question_shows_its_options_numbered() {
        assert_eq!(
            render(&choice(false), 0, 1),
            "\n[Setup] Which package manager?\n  1. pnpm (Recommended) - Workspace default.\n  \
             2. npm\n  3. bun\nPick a number, or type your own answer. Enter skips.\n> "
        );
        let plan = AskUserQuestionItem {
            detail: Some("# Plan\nStep one.".into()),
            ..AskUserQuestionItem::new("plan", "Approve?")
        };
        assert_eq!(
            render(&plan, 1, 2),
            "\n(2/2) Approve?\n  # Plan\n  Step one.\nType your answer. Enter skips.\n> "
        );
        assert!(
            render(&choice(true), 0, 1)
                .contains("Pick numbers separated by commas, or type your own answer.")
        );
    }

    #[tokio::test]
    async fn lines_are_read_one_at_a_time() {
        let lines = Lines::new(Cursor::new("one\r\ntwo\n"));
        assert_eq!(lines.next().await.unwrap().as_deref(), Some("one"));
        assert_eq!(lines.next().await.unwrap().as_deref(), Some("two"));
        assert_eq!(lines.next().await.unwrap(), None);
        assert_eq!(lines.next().await.unwrap(), None);
    }

    /// A writer the test can read back.
    #[derive(Clone, Default)]
    struct Shown(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Shown {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    async fn answer(input: &str, questions: Vec<AskUserQuestionItem>) -> (String, String) {
        let shown = Shown::default();
        let lines = Arc::new(Lines::new(Cursor::new(input.to_owned())));
        let answerer = TerminalAnswerer::new(Arc::clone(&lines), Box::new(shown.clone()));
        let result = match answerer.ask(&QuestionRequest::new(questions)).await {
            Ok(answer) => serde_json::to_string(&answer).unwrap(),
            Err(error) => format!("{:?}: {error}", error.code()),
        };
        let shown = String::from_utf8(shown.0.lock().unwrap().clone()).unwrap();
        // What is left is the next reader's.
        let rest = lines.next().await.unwrap().unwrap_or_default();
        (result, format!("{shown}|{rest}"))
    }

    #[tokio::test]
    async fn each_question_takes_one_line_and_leaves_the_rest() {
        let (result, shown) = answer(
            "1\n\nbye\n",
            vec![choice(false), AskUserQuestionItem::new("note", "Any note?")],
        )
        .await;
        assert_eq!(
            result,
            r#"{"answers":[{"id":"pkg","selected":["pnpm (Recommended)"]},{"id":"note","selected":[]}]}"#
        );
        assert!(
            shown.starts_with("\n(1/2) [Setup] Which package manager?\n"),
            "{shown}"
        );
        assert!(shown.contains("\n(2/2) Any note?\n"), "{shown}");
        assert!(shown.ends_with("> |bye"), "{shown}");
    }

    #[test]
    fn y_or_yes_allows_and_anything_else_rejects() {
        use karaka_user_approval::ApprovalOutcome::{AllowedOnce, Rejected};
        for (line, outcome) in [
            ("y", AllowedOnce),
            (" YES ", AllowedOnce),
            ("", Rejected),
            ("n", Rejected),
            ("no", Rejected),
            ("sure", Rejected),
        ] {
            assert_eq!(super::parse_approval(line), outcome, "{line:?}");
        }
    }

    /// An agent with only an id.
    struct Nobody(karaka_agent::FrameListeners, karaka_session::SessionId);

    impl karaka_agent::Agent for Nobody {
        fn id(&self) -> &karaka_session::SessionId {
            &self.1
        }

        fn followup(&self, _: karaka_session::UserMessage) -> Result<(), karaka_agent::InboxError> {
            Ok(())
        }

        fn steer(&self, _: karaka_session::UserMessage) -> Result<(), karaka_agent::InboxError> {
            Ok(())
        }

        fn inject(&self, _: karaka_session::UserMessage) -> Result<(), karaka_agent::InboxError> {
            Ok(())
        }

        fn cancel(&self, _: karaka_session::CancelCause) {}

        fn when_idle(&self) -> karaka_agent::WhenIdle<'_> {
            Box::pin(async {})
        }

        fn subscribe(
            &self,
            listener: karaka_agent::FrameListener,
        ) -> karaka_agent::FrameSubscription {
            self.0.subscribe(listener)
        }
    }

    #[tokio::test]
    async fn an_approval_names_the_tool_and_reason_and_ends_with_stdin() {
        use karaka_user_approval::{ApprovalOutcome, ApprovalRequest, DisplayReason};

        let agent = Arc::new(Nobody(
            karaka_agent::FrameListeners::new(),
            karaka_session::SessionId::new("s"),
        ));
        let request = ApprovalRequest {
            reason: Some("logged reason".into()),
            ..ApprovalRequest::new(agent, "working_directory")
        };
        assert_eq!(
            super::render_approval(&request),
            "\nAllow tool \"working_directory\"?\n  Reason: logged reason\nAllow it once? [y/N] "
        );
        let shown = ApprovalRequest {
            display_reason: Some(DisplayReason::en("shown reason")),
            ..request.clone()
        };
        assert!(super::render_approval(&shown).contains("  Reason: shown reason\n"));

        let lines = Arc::new(Lines::new(Cursor::new("yes\n".to_owned())));
        let approver = super::TerminalApprover::new(Arc::clone(&lines), Box::new(Shown::default()));
        assert_eq!(
            approver.decide(&request).await.unwrap(),
            ApprovalOutcome::AllowedOnce
        );
        assert_eq!(
            approver.decide(&request).await.unwrap(),
            ApprovalOutcome::Cancelled
        );
    }

    #[tokio::test]
    async fn the_end_of_stdin_aborts_the_question() {
        let (result, _) = answer("", vec![choice(false)]).await;
        assert_eq!(
            result,
            format!(
                "Some({ASK_ABORTED:?}): ask_user_question was aborted before the user answered"
            )
        );
    }
}
