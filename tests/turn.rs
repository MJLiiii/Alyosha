//! Build step 1: whole turns through the session driver with a scripted model
//! and a fake `read_file`. The cases come from the verification sections of
//! `docs/develop/AgentLoop.md` and `docs/develop/AgentState.md`.

mod support;

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use alyosha::{
    agent_loop::{DEFAULT_INSTRUCTION, SessionEvent},
    agent_state::{CallId, ToolOutcome, TurnOutcome, TurnPhase},
    llm_client::ModelError,
    tools::{RegisteredTool, ToolContext, ToolDefinition, ToolOutput, ToolRegistry, handler},
};
use serde_json::{Value, json};
use support::*;

/// The scripted turn of the implementation order's first step: two reads, one
/// of a missing file, then a reply.
#[tokio::test]
async fn a_turn_reads_two_files_and_replies_once() {
    let model = ScriptedModel::new();
    model.reply(vec![
        text("I'll read both."),
        call("call_a", "read_file", json!({"path": "a.txt"})),
        call("call_b", "read_file", json!({"path": "b.txt"})),
    ]);
    model.reply(vec![text("a.txt contains alpha. b.txt was missing.")]);
    let reads = Arc::new(AtomicUsize::new(0));
    let (driver, mut events) = start(model.clone(), files_registry(reads.clone()));

    let id = driver.submit("Read a.txt and b.txt.");
    let seen = events_until_turn_ends(&mut events, id).await;

    let started = |call: &str| SessionEvent::ToolStarted {
        call_id: call.into(),
        tool: "read_file".to_owned(),
        title: format!("read_file {}.txt", if call == "call_a" { "a" } else { "b" }),
    };
    assert_eq!(
        shown(&seen),
        [
            SessionEvent::Queued(id),
            SessionEvent::TurnStarted(id),
            SessionEvent::AssistantText("I'll read both.".to_owned()),
            started("call_a"),
            SessionEvent::ToolFinished {
                call_id: "call_a".into(),
                ok: true
            },
            started("call_b"),
            SessionEvent::ToolFinished {
                call_id: "call_b".into(),
                ok: false
            },
            SessionEvent::Reply {
                turn_id: id,
                text: "a.txt contains alpha. b.txt was missing.".to_owned(),
            },
            SessionEvent::TurnEnded {
                turn_id: id,
                outcome: TurnOutcome::Completed,
            },
        ]
    );

    let exchange = vec![
        user(id, "Read a.txt and b.txt."),
        assistant(vec![
            text("I'll read both."),
            call("call_a", "read_file", json!({"path": "a.txt"})),
            call("call_b", "read_file", json!({"path": "b.txt"})),
        ]),
        tool_result("call_a", ToolOutcome::Success("alpha".to_owned())),
        tool_result("call_b", ToolOutcome::Error("file not found".to_owned())),
    ];
    let requests = model.requests();
    assert_eq!(requests.len(), 2, "exactly two model requests");
    for request in &requests {
        assert_eq!(request.provider, PROVIDER);
        assert_eq!(request.model, MODEL);
        assert_eq!(request.system_instruction, DEFAULT_INSTRUCTION);
        assert_eq!(request.tools, [read_file_definition()]);
        assert_eq!(request.summary, None);
    }
    assert_eq!(requests[0].history, exchange[..1]);
    assert_eq!(
        requests[1].history, exchange,
        "both results under their call IDs"
    );

    let state = driver.snapshot();
    let mut history = exchange;
    history.push(assistant(vec![text(
        "a.txt contains alpha. b.txt was missing.",
    )]));
    assert_eq!(state.history, history, "the reply is recorded once");
    assert_eq!(state.outcomes.len(), 1);
    assert_eq!(state.outcomes[&id], TurnOutcome::Completed);
    assert!(state.active_work.is_none(), "the work slot is empty");
    assert!(state.pending_messages.is_empty());
    assert_eq!(
        reads.load(Ordering::SeqCst),
        2,
        "exactly two tool executions"
    );
}

#[tokio::test]
async fn a_clarifying_question_completes_the_turn() {
    let model = ScriptedModel::new();
    model.reply(vec![text("Which file do you mean?")]);
    let reads = Arc::new(AtomicUsize::new(0));
    let (driver, mut events) = start(model.clone(), files_registry(reads.clone()));

    let id = driver.submit("Read the file.");
    let seen = events_until_turn_ends(&mut events, id).await;

    assert!(seen.contains(&SessionEvent::Reply {
        turn_id: id,
        text: "Which file do you mean?".to_owned(),
    }));
    assert_eq!(model.requests().len(), 1);
    let state = driver.snapshot();
    assert_eq!(
        state.history,
        [
            user(id, "Read the file."),
            assistant(vec![text("Which file do you mean?")]),
        ]
    );
    assert_eq!(state.outcomes[&id], TurnOutcome::Completed);
    assert!(state.active_work.is_none());
    assert_eq!(reads.load(Ordering::SeqCst), 0);

    // A fresh session starts with every field at its initial value.
    let (fresh, _events) = start(ScriptedModel::new(), ToolRegistry::new());
    let state = fresh.snapshot();
    assert_eq!(state.provider, PROVIDER);
    assert!(state.history.is_empty());
    assert!(state.context_checkpoint.is_none());
    assert!(state.pending_messages.is_empty());
    assert!(state.active_work.is_none());
    assert!(state.outcomes.is_empty());
}

#[tokio::test]
async fn messages_sent_during_a_turn_wait_in_order() {
    let model = ScriptedModel::new();
    let first_reply = model.reply_gated(vec![text("Reply to A.")]);
    model.reply(vec![text("Reply to B.")]);
    model.reply(vec![text("Reply to C.")]);
    let (driver, mut events) = start(model.clone(), files_registry(Default::default()));

    let a = driver.submit("A");
    first_reply.started().await;
    let b = driver.submit("B");
    let c = driver.submit("C");
    let during = driver.snapshot();
    assert_eq!(during.active_turn().unwrap().turn_id, a);
    let waiting: Vec<_> = during
        .pending_messages
        .iter()
        .map(|message| message.id)
        .collect();
    assert_eq!(waiting, [b, c]);

    first_reply.release();
    let seen = events_until_turn_ends(&mut events, c).await;
    assert_eq!(started(&seen), [a, b, c]);

    let requests = model.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(
        requests[0].history,
        [user(a, "A")],
        "A sees only its own input"
    );
    assert_eq!(
        requests[1].history,
        [
            user(a, "A"),
            assistant(vec![text("Reply to A.")]),
            user(b, "B"),
        ]
    );
    assert_eq!(
        requests[2].history[3..],
        [assistant(vec![text("Reply to B.")]), user(c, "C")]
    );
    let state = driver.snapshot();
    for id in [a, b, c] {
        assert_eq!(state.outcomes[&id], TurnOutcome::Completed);
    }
}

#[tokio::test]
async fn the_same_text_twice_makes_two_turns() {
    let model = ScriptedModel::new();
    model.reply(vec![text("One.")]);
    model.reply(vec![text("Two.")]);
    let (driver, mut events) = start(model.clone(), files_registry(Default::default()));

    let first = driver.submit("Hello");
    let second = driver.submit("Hello");
    assert_ne!(first, second);
    let seen = events_until_turn_ends(&mut events, second).await;

    assert_eq!(
        started(&seen),
        [first, second],
        "each starts once, in order"
    );
    let state = driver.snapshot();
    assert_eq!(state.outcomes.len(), 2);
    assert_eq!(state.outcomes[&first], TurnOutcome::Completed);
    assert_eq!(state.outcomes[&second], TurnOutcome::Completed);
    assert_eq!(
        state.history,
        [
            user(first, "Hello"),
            assistant(vec![text("One.")]),
            user(second, "Hello"),
            assistant(vec![text("Two.")]),
        ]
    );
}

#[tokio::test]
async fn rejected_calls_get_their_own_errors() {
    let model = ScriptedModel::new();
    model.reply(vec![
        call("call_1", "list_files", json!({})),
        call("call_2", "read_file", json!({"path": 7})),
        call("call_3", "read_file", json!({"path": "a.txt"})),
    ]);
    model.reply(vec![text("Done.")]);
    let reads = Arc::new(AtomicUsize::new(0));
    let (driver, mut events) = start(model.clone(), files_registry(reads.clone()));

    let id = driver.submit("Look around.");
    events_until_turn_ends(&mut events, id).await;

    let requests = model.requests();
    assert_eq!(requests[0].tools, [read_file_definition()]);
    let results = &requests[1].history[2..];
    assert_eq!(
        results[0],
        tool_result(
            "call_1",
            ToolOutcome::Error("Unknown tool: list_files".to_owned())
        )
    );
    assert_eq!(
        results[1],
        tool_result(
            "call_2",
            ToolOutcome::Error(
                "Invalid arguments for read_file: /path: 7 is not of type \"string\"".to_owned()
            )
        )
    );
    assert_eq!(
        results[2],
        tool_result("call_3", ToolOutcome::Success("alpha".to_owned()))
    );
    assert_eq!(results.len(), 3);
    assert_eq!(reads.load(Ordering::SeqCst), 1, "only the valid call ran");
}

#[tokio::test]
async fn an_incomplete_response_runs_no_calls() {
    let model = ScriptedModel::new();
    model.fail(ModelError::Incomplete(
        "the stream broke after a tool call".to_owned(),
    ));
    let reads = Arc::new(AtomicUsize::new(0));
    let (driver, mut events) = start(model.clone(), files_registry(reads.clone()));

    let id = driver.submit("Read a.txt.");
    let seen = events_until_turn_ends(&mut events, id).await;

    let failed =
        TurnOutcome::Failed("incomplete response: the stream broke after a tool call".to_owned());
    assert_eq!(
        seen.last(),
        Some(&SessionEvent::TurnEnded {
            turn_id: id,
            outcome: failed.clone(),
        })
    );
    assert!(
        !seen
            .iter()
            .any(|event| matches!(event, SessionEvent::Reply { .. }))
    );
    let state = driver.snapshot();
    assert_eq!(state.history, [user(id, "Read a.txt.")], "no partial reply");
    assert_eq!(state.outcomes[&id], failed);
    assert!(state.active_work.is_none());
    assert_eq!(reads.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_failure_after_a_tool_call_keeps_its_result() {
    let model = ScriptedModel::new();
    model.reply(vec![call("call_a", "read_file", json!({"path": "a.txt"}))]);
    model.fail(ModelError::Timeout("no reply for 5 minutes".to_owned()));
    let (driver, mut events) = start(model.clone(), files_registry(Default::default()));

    let id = driver.submit("Read a.txt.");
    events_until_turn_ends(&mut events, id).await;

    let state = driver.snapshot();
    assert_eq!(
        state.history,
        [
            user(id, "Read a.txt."),
            assistant(vec![call("call_a", "read_file", json!({"path": "a.txt"}))]),
            tool_result("call_a", ToolOutcome::Success("alpha".to_owned())),
        ]
    );
    assert_eq!(
        state.outcomes[&id],
        TurnOutcome::Failed("timeout: no reply for 5 minutes".to_owned())
    );
}

#[tokio::test]
async fn stop_during_a_read_records_it_and_cancels_the_rest() {
    let model = ScriptedModel::new();
    // C names no registered tool. After Stop it is cancelled unstarted, not
    // rejected by the executor.
    let calls = vec![
        call("call_a", "read_file", json!({"path": "a.txt"})),
        call("call_b", "read_file", json!({"path": "b.txt"})),
        call("call_c", "list_files", json!({})),
    ];
    model.reply(calls.clone());
    let next_turn = model.reply_gated(vec![text("Next.")]);
    let reads = Arc::new(AtomicUsize::new(0));
    let read_a = Gate::default();
    let mut registry = ToolRegistry::new();
    registry
        .register(fake_read_file(
            reads.clone(),
            Some(("a.txt", read_a.clone())),
        ))
        .unwrap();
    let (driver, mut events) = start(model.clone(), registry);

    let first = driver.submit("Read a.txt and b.txt.");
    read_a.started().await;
    let second = driver.submit("Then this.");
    let reading_a = TurnPhase::RunningTools {
        active_call_id: Some(CallId::from("call_a")),
    };
    let before = driver.snapshot();
    assert_eq!(before.active_turn().unwrap().phase, reading_a);
    assert!(!before.active_turn().unwrap().stop_requested());

    driver.stop();
    let stopping = driver.snapshot();
    let turn = stopping.active_turn().unwrap();
    assert_eq!(turn.turn_id, first);
    assert_eq!(turn.phase, reading_a, "A is still active");
    assert!(turn.stop_requested(), "and Stop is requested");
    assert_eq!(
        stopping.pending_messages.len(),
        1,
        "Stop leaves the queue alone"
    );

    read_a.release();
    let seen = events_until_turn_ends(&mut events, first).await;
    assert_eq!(
        seen.last(),
        Some(&SessionEvent::TurnEnded {
            turn_id: first,
            outcome: TurnOutcome::Stopped,
        })
    );

    next_turn.started().await;
    let next = driver.snapshot();
    let turn = next.active_turn().unwrap();
    assert_eq!(turn.turn_id, second);
    assert!(
        !turn.stop_requested(),
        "the next turn has a fresh Stop signal"
    );
    next_turn.release();
    events_until_turn_ends(&mut events, second).await;

    let requests = model.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[1].history,
        [
            user(first, "Read a.txt and b.txt."),
            assistant(calls),
            tool_result("call_a", ToolOutcome::Success("alpha".to_owned())),
            tool_result("call_b", ToolOutcome::Cancelled),
            tool_result("call_c", ToolOutcome::Cancelled),
            user(second, "Then this."),
        ],
        "the next request belongs to the queued turn and carries every result"
    );
    let state = driver.snapshot();
    assert_eq!(state.outcomes[&first], TurnOutcome::Stopped);
    assert_eq!(state.outcomes[&second], TurnOutcome::Completed);
    assert_eq!(reads.load(Ordering::SeqCst), 1, "B never ran");
}

#[tokio::test]
async fn stop_during_generation_discards_the_reply() {
    let model = ScriptedModel::new();
    let generating = model.reply_gated(vec![call("call_a", "read_file", json!({"path": "a.txt"}))]);
    let reads = Arc::new(AtomicUsize::new(0));
    let (driver, mut events) = start(model.clone(), files_registry(reads.clone()));

    let id = driver.submit("Read a.txt.");
    generating.started().await;
    assert_eq!(
        driver.snapshot().active_turn().unwrap().phase,
        TurnPhase::Generating
    );
    driver.stop();
    events_until_turn_ends(&mut events, id).await;

    let state = driver.snapshot();
    assert_eq!(state.history, [user(id, "Read a.txt.")]);
    assert_eq!(state.outcomes[&id], TurnOutcome::Stopped);
    assert!(state.active_work.is_none());
    assert_eq!(reads.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_tool_notice_reaches_the_front_end() {
    let model = ScriptedModel::new();
    model.reply(vec![call("call_m", "remember", json!({}))]);
    model.reply(vec![text("Noted.")]);
    let mut registry = ToolRegistry::new();
    registry
        .register(RegisteredTool {
            definition: ToolDefinition {
                name: "remember".to_owned(),
                description: "A fake memory write.".to_owned(),
                input_schema: json!({"type": "object"}),
                requires_approval: false,
            },
            handler: handler(|_arguments: Value, _context: ToolContext| async {
                Ok(ToolOutput::text("created").with_notice("Remembered: editor"))
            }),
        })
        .unwrap();
    let (driver, mut events) = start(model.clone(), registry);

    let id = driver.submit("I use Helix.");
    let seen = events_until_turn_ends(&mut events, id).await;

    let notice = seen
        .iter()
        .position(|event| *event == SessionEvent::Notice("Remembered: editor".to_owned()))
        .expect("the notice was sent");
    let reply = seen
        .iter()
        .position(|event| matches!(event, SessionEvent::Reply { .. }))
        .unwrap();
    assert!(notice < reply);
    assert_eq!(
        model.requests()[1].history[2],
        tool_result("call_m", ToolOutcome::Success("created".to_owned())),
        "the model sees the output, not the notice"
    );
}
