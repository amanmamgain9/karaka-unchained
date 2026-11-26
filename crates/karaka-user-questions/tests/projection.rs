//! The `userQuestions` fold and projection. Ported from dsh's
//! `user-questions/tests/projection.spec.ts`, case for case. Events are
//! written as dsh's v4 JSON, as the spec writes them.

use karaka_session::{Entry, SessionHeader, SessionId, TOOL_OUTCOME_UNKNOWN};
use karaka_session_projection::{ClientView, Projection};
use karaka_user_questions::{
    AskUserQuestionAnswerItem, AskUserQuestionItem, AskUserQuestionOption, PendingUserQuestion,
    SettledUserQuestion, TIMED_WAIT_PARAMETER, UserQuestionFold, UserQuestionProjectionView,
    UserQuestionState, UserQuestionsProjection, UserQuestionsState, apply_user_question_event,
    fold_user_questions, is_timed_ask_user_question_schema, questions_of,
};
use serde_json::{Value, json};

const CALL_ID: &str = "call_ask_1";

fn tool_arguments() -> String {
    json!({
        "questions": [{
            "id": "scope",
            "question": "Which scope?",
            "header": "Scope",
            "options": [
                { "label": "Tool only", "description": "Only the tool package." },
                { "label": "Everything" },
            ],
            "multi_select": true,
        }],
    })
    .to_string()
}

fn questions() -> Vec<AskUserQuestionItem> {
    vec![AskUserQuestionItem {
        header: Some("Scope".into()),
        options: Some(vec![
            AskUserQuestionOption {
                label: "Tool only".into(),
                description: Some("Only the tool package.".into()),
            },
            AskUserQuestionOption {
                label: "Everything".into(),
                description: None,
            },
        ]),
        multi_select: Some(true),
        ..AskUserQuestionItem::new("scope", "Which scope?")
    }]
}

/// One event in dsh's envelope. The surface types are appended.
fn event(seq: u64, event_type: &str, data: Value) -> Entry {
    let mut envelope = json!({ "type": event_type, "seq": seq, "time": seq, "data": data });
    if matches!(event_type, "tool/result" | "user/message") {
        envelope["surfaceOp"] = json!("append");
    }
    serde_json::from_value(envelope).expect("the event fits dsh's envelope")
}

/// The `ask_user_question` schema as the blocking legacy tool declares it:
/// no wait parameter.
fn legacy_schema() -> Value {
    json!({
        "name": "ask_user_question",
        "description": "Ask the user a concise question.",
        "parameters": { "type": "object", "properties": { "questions": { "type": "array" } } },
    })
}

/// The `ask_user_question` schema as the timed tool declares it: the wait
/// parameter marks it.
fn timed_schema() -> Value {
    json!({
        "name": "ask_user_question",
        "description": "Ask brief questions.",
        "parameters": {
            "type": "object",
            "properties": { "questions": { "type": "array" }, TIMED_WAIT_PARAMETER: { "type": "integer" } },
        },
    })
}

fn bash_schema() -> Value {
    json!({ "name": "bash", "description": "Run a command.", "parameters": { "type": "object", "properties": {} } })
}

/// The request header the model saw before it asked.
fn header(seq: u64, tools: Option<Vec<Value>>) -> Entry {
    let mut header =
        json!({ "config": { "provider": "deepseek-official", "model": "deepseek-v4-flash" } });
    if let Some(tools) = tools {
        header["tools"] = Value::Array(tools);
    }
    event(
        seq,
        "request/header",
        json!({ "header": header, "reason": "initial" }),
    )
}

fn timed_header() -> Entry {
    header(0, Some(vec![bash_schema(), timed_schema()]))
}

fn legacy_header() -> Entry {
    header(0, Some(vec![bash_schema(), legacy_schema()]))
}

fn call(seq: u64, call_id: &str, name: &str, arguments: &str) -> Entry {
    event(
        seq,
        "tool/call",
        json!({ "turn": 1, "step": 1, "callId": call_id, "name": name, "arguments": arguments }),
    )
}

fn asked() -> Entry {
    call(1, CALL_ID, "ask_user_question", &tool_arguments())
}

fn result_of(
    seq: u64,
    call_id: &str,
    content: Value,
    is_error: bool,
    error: Option<Value>,
) -> Entry {
    let mut data = json!({
        "turn": 1,
        "step": 1,
        "message": {
            "id": format!("tool-{call_id}"),
            "role": "tool",
            "source": { "kind": "tool", "callId": call_id },
            "toolCallId": call_id,
            "content": content,
            "isError": is_error,
        },
    });
    if let Some(error) = error {
        data["error"] = error;
    }
    event(seq, "tool/result", data)
}

fn resulted(text: &str, error: Option<(&str, &str)>) -> Entry {
    result_of(
        2,
        CALL_ID,
        json!([{ "type": "text", "text": text }]),
        error.is_some(),
        error.map(|(name, code)| json!({ "name": name, "code": code })),
    )
}

fn pending_result() -> Entry {
    resulted(r#"{"pending":true,"callId":"call_ask_1"}"#, None)
}

fn interrupted() -> Entry {
    resulted(
        "The tool call was interrupted after it was recorded.",
        Some(("SessionFormatError", TOOL_OUTCOME_UNKNOWN)),
    )
}

fn reply_message(call_id: &str, content: Value) -> Value {
    json!({
        "id": format!("reply-{call_id}"),
        "role": "user",
        "source": { "kind": "user-question-reply", "callId": call_id, "outcome": "answered" },
        "content": content,
    })
}

fn reply(text: &str) -> Value {
    reply_message(CALL_ID, json!([{ "type": "text", "text": text }]))
}

fn user_message(seq: u64, message: Value) -> Entry {
    event(seq, "user/message", message)
}

fn spliced(seq: u64, data: Value) -> Entry {
    event(seq, "agent/inbox/spliced", data)
}

fn answer_batch() -> Vec<AskUserQuestionAnswerItem> {
    vec![AskUserQuestionAnswerItem {
        id: "scope".into(),
        selected: vec!["Tool only".into()],
        custom: Some("also docs".into()),
    }]
}

fn answered_text() -> String {
    json!({
        "kind": "answer_to_pending_question",
        "tool": "ask_user_question",
        "callId": CALL_ID,
        "questions": questions(),
        "answers": answer_batch(),
    })
    .to_string()
}

fn empty() -> UserQuestionProjectionView {
    UserQuestionProjectionView::default()
}

fn active(call_id: &str, state: UserQuestionState) -> UserQuestionProjectionView {
    UserQuestionProjectionView {
        active: vec![PendingUserQuestion {
            call_id: call_id.into(),
            questions: questions(),
            state,
        }],
        settled: Vec::new(),
    }
}

fn settled(answers: Vec<AskUserQuestionAnswerItem>) -> UserQuestionProjectionView {
    UserQuestionProjectionView {
        active: Vec::new(),
        settled: vec![SettledUserQuestion {
            call_id: CALL_ID.into(),
            answers,
        }],
    }
}

fn fold(entries: &[Entry]) -> UserQuestionProjectionView {
    fold_user_questions(entries)
}

/// The fold after `entries`, from the start.
fn folded(entries: &[Entry]) -> UserQuestionFold {
    entries
        .iter()
        .fold(UserQuestionFold::default(), |fold, entry| {
            apply_user_question_event(&fold, &entry.event).unwrap_or(fold)
        })
}

// "recognises the timed tool schema by its wait parameter alone"
#[test]
fn the_timed_schema_is_known_by_its_wait_parameter_alone() {
    let parameters = |schema: Value| schema["parameters"].clone();
    assert!(is_timed_ask_user_question_schema(
        "ask_user_question",
        &parameters(timed_schema())
    ));
    assert!(!is_timed_ask_user_question_schema(
        "ask_user_question",
        &parameters(legacy_schema())
    ));
    assert!(!is_timed_ask_user_question_schema(
        "bash",
        &parameters(timed_schema())
    ));
    assert!(!is_timed_ask_user_question_schema(
        "ask_user_question",
        &json!({ "type": "object" })
    ));
}

// "opens a question asked under the timed schema and maps the tool
// vocabulary"
#[test]
fn a_question_asked_under_the_timed_schema_opens() {
    assert_eq!(
        fold(&[timed_header(), asked()]),
        active(CALL_ID, UserQuestionState::Open)
    );
}

// "never tracks a call made under the blocking legacy tool, so a legacy
// Session folds to the empty view". dsh's last case, a seeded header whose
// tools are the placeholder string `{{tools}}` and whose config is empty,
// has no typed form here: Karaka's header refuses it when it is read.
#[test]
fn a_call_under_the_legacy_tool_is_never_tracked() {
    let legacy = [legacy_header(), asked()];
    assert_eq!(fold(&legacy), empty());
    let with = |last: Entry| {
        let mut log = legacy.to_vec();
        log.push(last);
        fold(&log)
    };
    assert_eq!(
        with(resulted(
            r#"{"answers":[{"id":"scope","selected":["Tool only"]}]}"#,
            None
        )),
        empty()
    );
    assert_eq!(with(interrupted()), empty());
    // No header at all, and a header without tools, read as the legacy tool.
    assert_eq!(fold(&[asked()]), empty());
    assert_eq!(fold(&[header(0, None), asked()]), empty());
}

// "follows the latest header when the composed tool changes mid-Session"
#[test]
fn the_latest_header_decides() {
    let later = || call(3, "call_ask_2", "ask_user_question", &tool_arguments());
    assert_eq!(
        fold(&[
            legacy_header(),
            asked(),
            header(2, Some(vec![timed_schema()])),
            later()
        ]),
        active("call_ask_2", UserQuestionState::Open)
    );
    assert_eq!(
        fold(&[
            timed_header(),
            asked(),
            header(2, Some(vec![legacy_schema()])),
            later()
        ]),
        active(CALL_ID, UserQuestionState::Open)
    );
}

// "settles an in-time answer batch with that batch and drops every failure
// a live Host records"
#[test]
fn an_answer_settles_and_every_failure_drops() {
    let after = |last: Entry| fold(&[timed_header(), asked(), last]);
    assert_eq!(
        after(resulted(
            r#"{"answers":[{"id":"scope","selected":["Tool only"]}]}"#,
            None
        )),
        settled(vec![AskUserQuestionAnswerItem {
            id: "scope".into(),
            selected: vec!["Tool only".into()],
            custom: None,
        }])
    );
    for error in [
        ("cancelled", ("UserQuestionError", "ASK_CANCELLED")),
        ("aborted", ("UserQuestionError", "ASK_ABORTED")),
        ("boom", ("Error", "UNEXPECTED")),
    ] {
        assert_eq!(after(resulted(error.0, Some(error.1))), empty());
    }
    assert_eq!(after(resulted("not json at all", None)), empty());
    assert_eq!(after(resulted(r#"{"answers":"nope"}"#, None)), empty());
    // A batch spelled inside an error result is a failure, whatever its
    // text says.
    assert_eq!(
        after(result_of(
            2,
            CALL_ID,
            json!([{ "type": "text", "text": r#"{"answers":[]}"# }]),
            true,
            None
        )),
        empty()
    );
}

// "keeps a pending call, or one the process never finished, answerable as
// continued"
#[test]
fn a_pending_or_unfinished_call_continues() {
    let continued = active(CALL_ID, UserQuestionState::Continued);
    assert_eq!(
        fold(&[timed_header(), asked(), pending_result()]),
        continued
    );
    assert_eq!(fold(&[timed_header(), asked(), interrupted()]), continued);
}

// "closes a continued question only when its reply is admitted as a user
// message"
#[test]
fn a_continued_question_closes_only_when_its_reply_is_admitted() {
    let continued = [timed_header(), asked(), pending_result()];
    let mut queued_then_dropped = continued.to_vec();
    queued_then_dropped.push(spliced(
        3,
        json!({ "target": "next-step", "start": 0, "inserted": [reply("{}")] }),
    ));
    queued_then_dropped.push(spliced(
        4,
        json!({ "target": "next-step", "start": 0, "removedCount": 1, "inserted": [], "outcome": "canceled" }),
    ));
    assert_eq!(
        fold(&queued_then_dropped),
        active(CALL_ID, UserQuestionState::Continued)
    );
    let mut admitted = continued.to_vec();
    admitted.push(user_message(3, reply("{}")));
    assert_eq!(fold(&admitted), settled(Vec::new()));
}

// "records the answers one reply carried, exactly once across its splice
// and its own event"
#[test]
fn a_reply_records_its_answers_once() {
    let replied = reply(&answered_text());
    let continued = [timed_header(), asked(), pending_result()];
    let mut admitted = continued.to_vec();
    admitted.push(user_message(3, replied.clone()));
    assert_eq!(fold(&admitted), settled(answer_batch()));
    let mut queued = continued.to_vec();
    queued.push(spliced(
        3,
        json!({ "target": "next-step", "start": 0, "inserted": [replied.clone()] }),
    ));
    queued.push(user_message(4, replied));
    assert_eq!(fold(&queued), settled(answer_batch()));
}

// "continues a pending PTC sub-call when only run_code appeared in the
// request header"
#[test]
fn a_pending_ptc_sub_call_continues() {
    let sub_call_id = "run_1:ptc:1";
    let arguments: Value = serde_json::from_str(&tool_arguments()).unwrap();
    let dispatch_data = json!({
        "rootCallId": "run_1",
        "parentCallId": "run_1",
        "subCallId": sub_call_id,
        "name": "ask_user_question",
        "arguments": arguments,
        "isError": false,
        "content": [{ "type": "text", "text": json!({ "pending": true, "callId": sub_call_id }).to_string() }],
    });
    let dispatch = |data: Value| event(2, "tool/ptc-dispatch", data);
    let with = |key: &str, value: Value| {
        let mut data = dispatch_data.clone();
        data[key] = value;
        dispatch(data)
    };
    let run_code_header = || {
        header(
            0,
            Some(vec![
                json!({ "name": "run_code", "description": "Run code.", "parameters": { "type": "object" } }),
            ]),
        )
    };
    let continued = active(sub_call_id, UserQuestionState::Continued);
    assert_eq!(
        fold(&[run_code_header(), dispatch(dispatch_data.clone())]),
        continued
    );
    assert_eq!(
        fold(&[
            run_code_header(),
            dispatch(dispatch_data.clone()),
            dispatch(dispatch_data.clone())
        ]),
        continued
    );
    assert_eq!(
        fold(&[
            run_code_header(),
            with("arguments", json!({ "questions": "invalid" }))
        ]),
        empty()
    );
    assert_eq!(
        fold(&[run_code_header(), with("isError", json!(true))]),
        empty()
    );
    assert_eq!(
        fold(&[
            run_code_header(),
            with(
                "content",
                json!([{ "type": "text", "text": r#"{"answers":[]}"# }])
            )
        ]),
        empty()
    );
}

// "settles with no answers when the reply text carries none this reader
// can use"
#[test]
fn a_reply_without_a_usable_batch_settles_with_none() {
    let continued = [timed_header(), asked(), pending_result()];
    for message in [
        reply("no json here"),
        reply(r#"{"answers":"nope"}"#),
        reply_message(CALL_ID, json!([])),
    ] {
        let mut log = continued.to_vec();
        log.push(user_message(3, message));
        assert_eq!(fold(&log), settled(Vec::new()));
    }
}

// "returns the same fold for unrelated events, unreadable calls, and an
// unchanged header". dsh returns the same object; here, no change.
#[test]
fn unrelated_events_change_nothing() {
    let fold = folded(&[timed_header(), asked()]);
    let user = json!({
        "id": "hi", "role": "user", "source": { "kind": "user" },
        "content": [{ "type": "text", "text": "hi" }],
    });
    for entry in [
        event(5, "turn/start", json!({ "turn": 2 })),
        header(5, Some(vec![timed_schema()])),
        call(5, "other", "bash", "{}"),
        call(5, "broken", "ask_user_question", "{not json"),
        spliced(
            5,
            json!({ "target": "next-step", "start": 0, "inserted": [user.clone()] }),
        ),
        user_message(5, user),
        user_message(
            5,
            reply_message("other", json!([{ "type": "text", "text": "{}" }])),
        ),
        result_of(
            2,
            "other",
            json!([{ "type": "text", "text": "{}" }]),
            false,
            None,
        ),
    ] {
        assert_eq!(
            apply_user_question_event(&fold, &entry.event),
            None,
            "{:?}",
            entry.event
        );
    }
}

// "tracks several timed calls at once and re-asks under a reused call id
// in place"
#[test]
fn several_calls_are_tracked_and_a_reused_id_reasks_in_place() {
    let other = call(
        3,
        "call_ask_2",
        "ask_user_question",
        r#"{"questions":[{"id":"name","question":"Name?"}]}"#,
    );
    let both = folded(&[timed_header(), asked(), other]);
    let ids = |fold: &UserQuestionFold| {
        fold.questions
            .active
            .iter()
            .map(|question| (question.call_id.clone(), question.state))
            .collect::<Vec<_>>()
    };
    use UserQuestionState::{Continued, Open};
    assert_eq!(
        ids(&both),
        [(CALL_ID.to_owned(), Open), ("call_ask_2".to_owned(), Open)]
    );
    let continued = apply_user_question_event(&both, &pending_result().event).unwrap();
    assert_eq!(
        ids(&continued),
        [
            (CALL_ID.to_owned(), Continued),
            ("call_ask_2".to_owned(), Open)
        ]
    );
    let reasked = apply_user_question_event(&continued, &asked().event).unwrap();
    assert_eq!(
        ids(&reasked),
        [("call_ask_2".to_owned(), Open), (CALL_ID.to_owned(), Open)]
    );
}

// "closes a tracked call whose result carries no text at all"
#[test]
fn a_result_without_text_closes_the_call() {
    assert_eq!(
        fold(&[
            timed_header(),
            asked(),
            result_of(2, CALL_ID, json!([]), false, None)
        ]),
        empty()
    );
}

// "restores a checkpointed state in service vocabulary and refuses
// duplicate call ids"
#[test]
fn a_checkpoint_restores_and_duplicate_ids_are_refused() {
    let full = json!({
        "callId": "call_ask_1",
        "questions": [
            {
                "id": "plan", "question": "Approve?", "detail": "# Plan", "header": "Review",
                "options": [{ "label": "Approve", "description": "Ship it." }, { "label": "Refuse" }],
                "multiSelect": false,
            },
            { "id": "bare", "question": "Anything else?" },
        ],
        "state": "continued",
    });
    let view = json!({ "active": [full], "settled": [{ "callId": "call_ask_0", "answers": answer_batch() }] });
    let state = json!({ "inheritedEventCount": 3, "timed": true, "questions": view });
    let restored: UserQuestionsState = serde_json::from_value(state.clone()).unwrap();
    assert_eq!(restored.inherited_event_count, 3);
    assert!(restored.timed);
    assert_eq!(restored.questions.active[0].questions[1].options, None);
    // It writes back exactly as it was read, so the second question has no
    // `options` key.
    assert_eq!(serde_json::to_value(&restored).unwrap(), state);

    let view_of = |value: Value| serde_json::from_value::<UserQuestionProjectionView>(value);
    let mut open = full.clone();
    open["state"] = json!("open");
    let duplicate = view_of(json!({ "active": [full.clone(), open], "settled": [] })).unwrap_err();
    assert!(
        duplicate.to_string().contains("callIds must be unique"),
        "{duplicate}"
    );
    let mut nameless = full.clone();
    nameless["callId"] = json!("");
    assert!(view_of(json!({ "active": [nameless], "settled": [] })).is_err());
    // Strict, as dsh's restore schema is.
    let mut extra = full.clone();
    extra["other"] = json!(1);
    assert!(view_of(json!({ "active": [extra], "settled": [] })).is_err());
    let mut unasked = full;
    unasked["questions"] = json!([]);
    assert!(view_of(json!({ "active": [unasked], "settled": [] })).is_err());
}

// "reads only well-formed question batches out of the arguments"
#[test]
fn only_well_formed_batches_are_read() {
    assert_eq!(questions_of(r#"{"questions":[]}"#), None);
    assert_eq!(questions_of(r#"{"questions":[{"id":"x"}]}"#), None);
    assert_eq!(questions_of("[]"), None);
    assert_eq!(
        questions_of(r#"{"questions":[{"id":"x","question":"Q?"}]}"#),
        Some(vec![AskUserQuestionItem::new("x", "Q?")])
    );
    // zod's `optional()` takes an absent key, not a null.
    assert_eq!(
        questions_of(r#"{"questions":[{"id":"x","question":"Q?","header":null}]}"#),
        None
    );
}

// "does not inherit a parent Session question into a fork and exposes the
// whole view on the wire"
#[test]
fn a_fork_inherits_no_question_and_the_view_is_the_whole_fold() {
    let unit = UserQuestionsProjection;
    let header = SessionHeader::new(SessionId::new("s"), 0);
    let inherited = unit.init(&header, 2);
    assert_eq!(unit.apply(&inherited, &timed_header()), None);
    assert_eq!(unit.apply(&inherited, &asked()), None);

    let fresh = unit.init(&header, 0);
    let armed = unit.apply(&fresh, &timed_header()).unwrap();
    assert_eq!((armed.inherited_event_count, armed.timed), (0, true));
    assert_eq!(unit.view(&armed), unit.view(&fresh));
    let opened = unit.apply(&armed, &asked()).unwrap();
    assert_eq!(unit.view(&opened), active(CALL_ID, UserQuestionState::Open));
    assert_eq!(
        unit.apply(&opened, &event(5, "turn/start", json!({ "turn": 2 }))),
        None
    );
    assert_eq!(unit.state_version(), 2);
    assert_eq!(UserQuestionsProjection::KEY, "userQuestions");
    assert_eq!(
        serde_json::to_value(unit.view(&opened)).unwrap(),
        json!({
            "active": [{ "callId": CALL_ID, "questions": questions(), "state": "open" }],
            "settled": [],
        })
    );
}
