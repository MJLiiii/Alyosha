//! Context Manager checks through the session driver, with the guide's
//! budget fixture: a 20,000-token window, a 4,000-token reply allowance, and
//! a 12,000-token input ceiling, so usable input is 10,000 tokens. See
//! `docs/develop/ContextManager.md#verify-the-contracts`.

mod support;

use std::sync::Arc;

use alyosha::{
    agent_loop::{COMPACT_BUSY, SessionDriver, SessionEvent, SessionSetup},
    agent_state::{HistoryEntry, TurnOutcome},
    clock::FixedClock,
    context_manager::{NO_OLDER_CONTEXT, RouteLimits},
    llm_client::ModelError,
    tools::ToolRegistry,
};
use chrono::{TimeZone, Utc};
use serde_json::json;
use support::{ScriptedModel, call, events_until_turn_ends, files_registry, text};
use tokio::sync::mpsc::UnboundedReceiver;

fn start(
    model: Arc<ScriptedModel>,
    registry: ToolRegistry,
) -> (SessionDriver, UnboundedReceiver<SessionEvent>) {
    let mut setup = SessionSetup::minimal(
        "p",
        "m",
        chrono_tz::Europe::Amsterdam,
        model,
        registry,
        Arc::new(FixedClock(
            Utc.with_ymd_and_hms(2026, 11, 25, 9, 2, 0).unwrap(),
        )),
    );
    setup.limits = RouteLimits {
        context_window: 20_000,
        default_output: 4_000,
        input_ceiling: Some(12_000),
    };
    SessionDriver::start(setup)
}

/// About 3,000 tokens of text.
fn big(tag: &str) -> String {
    format!("{tag} {}", "x".repeat(9_000))
}

async fn run(
    driver: &SessionDriver,
    events: &mut UnboundedReceiver<SessionEvent>,
    message: &str,
) -> TurnOutcome {
    let id = driver.submit(message);
    events_until_turn_ends(events, id).await;
    driver.snapshot().outcomes[&id].clone()
}

#[tokio::test]
async fn below_then_over_budget() {
    let model = ScriptedModel::new();
    for reply in ["one", "two", "three"] {
        model.reply(vec![text(reply)]);
    }
    model.reply(vec![text("SUMMARY of turns one to three")]);
    model.reply(vec![text("four")]);
    let (driver, mut events) = start(model.clone(), ToolRegistry::new());
    for tag in ["first", "second", "third"] {
        assert_eq!(
            run(&driver, &mut events, &big(tag)).await,
            TurnOutcome::Completed
        );
    }
    let requests = model.requests();
    assert_eq!(requests.len(), 3, "no summary below the budget");
    assert_eq!(requests[2].history.len(), 5, "every entry, in order");

    let id = driver.submit(big("fourth"));
    let seen = events_until_turn_ends(&mut events, id).await;
    assert!(seen.contains(&SessionEvent::Notice("Compacting context…".into())));
    let requests = model.requests();
    assert_eq!(
        requests.len(),
        5,
        "one summary request, then the turn's request"
    );
    let summary = &requests[3];
    assert!(summary.tools.is_empty());
    assert_eq!(summary.max_output_tokens, Some(4096));
    assert!(summary.system_instruction.contains("five sections"));
    let transcript = match &summary.history[0] {
        HistoryEntry::User(user) => user.text.clone(),
        other => panic!("{other:?}"),
    };
    assert!(
        transcript.contains("first")
            && transcript.contains("third")
            && !transcript.contains("fourth")
    );

    let request = &requests[4];
    assert_eq!(
        request.summary.as_deref(),
        Some("SUMMARY of turns one to three")
    );
    assert_eq!(
        request.history.len(),
        1,
        "only the active message stays verbatim"
    );
    let state = driver.snapshot();
    assert_eq!(state.history.len(), 8, "the full history remains");
    assert_eq!(
        state.context_checkpoint.as_ref().unwrap().first_kept_index,
        6
    );
    assert_eq!(state.outcomes[&id], TurnOutcome::Completed);
}

#[tokio::test]
async fn a_long_turn_keeps_its_opening_message_and_latest_batch() {
    let model = ScriptedModel::new();
    // Each batch reads a large result; the third crosses the threshold.
    for n in 0..3 {
        model.reply(vec![call(
            &format!("c{n}"),
            "read_file",
            json!({"path": "a.txt"}),
        )]);
    }
    model.reply(vec![text("SUMMARY of earlier batches")]);
    model.reply(vec![text("done")]);
    let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (driver, mut events) = start(model.clone(), files_registry(reads.clone()));
    // The fake read returns `alpha`; the opening message itself is large.
    let id = driver.submit(big("opening") + &"y".repeat(15_000));
    events_until_turn_ends(&mut events, id).await;
    let requests = model.requests();
    let last = requests.last().unwrap();
    let openings = last
        .history
        .iter()
        .filter(|entry| matches!(entry, HistoryEntry::User(user) if user.text.contains("opening")))
        .count();
    assert_eq!(openings, 1, "the opening message appears once");
    assert_eq!(
        reads.load(std::sync::atomic::Ordering::SeqCst),
        3,
        "earlier tools ran once"
    );
    assert_eq!(driver.snapshot().outcomes[&id], TurnOutcome::Completed);
}

#[tokio::test]
async fn an_oversized_message_fails_with_a_context_error() {
    let model = ScriptedModel::new();
    let (driver, mut events) = start(model.clone(), ToolRegistry::new());
    let outcome = run(&driver, &mut events, &"z".repeat(40_000)).await;
    let TurnOutcome::Failed(reason) = outcome else {
        panic!("{outcome:?}")
    };
    assert!(reason.starts_with("context window exceeded"), "{reason}");
    assert!(model.requests().is_empty());
}

#[tokio::test]
async fn a_failed_summary_keeps_the_previous_checkpoint() {
    let model = ScriptedModel::new();
    for reply in ["one", "two", "three"] {
        model.reply(vec![text(reply)]);
    }
    model.fail(ModelError::Incomplete("cut".into()));
    let (driver, mut events) = start(model, ToolRegistry::new());
    for tag in ["first", "second", "third"] {
        run(&driver, &mut events, &big(tag)).await;
    }
    let outcome = run(&driver, &mut events, &big("fourth")).await;
    assert!(matches!(outcome, TurnOutcome::Failed(reason) if reason.starts_with("summary failed")));
    assert!(driver.snapshot().context_checkpoint.is_none());
}

#[tokio::test]
async fn provider_overflow_compacts_once_and_retries() {
    let model = ScriptedModel::new();
    model.reply(vec![text("one")]);
    model.reply(vec![call("c1", "read_file", json!({"path": "a.txt"}))]);
    model.fail(ModelError::ContextOverflow("prompt is too long".into()));
    model.reply(vec![text("SUMMARY")]);
    model.reply(vec![text("recovered")]);
    let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (driver, mut events) = start(model.clone(), files_registry(reads.clone()));
    run(&driver, &mut events, "hello").await;
    let outcome = run(&driver, &mut events, "read a.txt").await;
    assert_eq!(outcome, TurnOutcome::Completed);
    let requests = model.requests();
    let rejected = &requests[2];
    let retried = requests.last().unwrap();
    assert_eq!(retried.summary.as_deref(), Some("SUMMARY"));
    assert!(
        alyosha::context_manager::estimate(retried) < alyosha::context_manager::estimate(rejected),
        "the retry is smaller"
    );
    assert_eq!(
        reads.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the batch ran once"
    );

    // A second overflow in one turn ends recovery.
    model.reply(vec![call("c2", "read_file", json!({"path": "a.txt"}))]);
    model.fail(ModelError::ContextOverflow("again".into()));
    model.reply(vec![text("SUMMARY 2")]);
    model.fail(ModelError::ContextOverflow("still".into()));
    let outcome = run(&driver, &mut events, "read it again").await;
    assert!(
        matches!(outcome, TurnOutcome::Failed(reason) if reason.contains("context window exceeded"))
    );
}

#[tokio::test]
async fn manual_compaction() {
    let model = ScriptedModel::new();
    let (driver, mut events) = start(model.clone(), ToolRegistry::new());
    driver.compact();
    let status = loop {
        if let Some(SessionEvent::CommandStatus(status)) = events.recv().await {
            break status;
        }
    };
    assert_eq!(status, NO_OLDER_CONTEXT);
    assert!(model.requests().is_empty(), "no model call");

    for reply in ["one", "two"] {
        model.reply(vec![text(reply)]);
    }
    run(&driver, &mut events, &big("first")).await;
    run(&driver, &mut events, &big("second")).await;
    model.reply(vec![text("SUMMARY")]);
    driver.compact();
    let status = loop {
        if let Some(SessionEvent::CommandStatus(status)) = events.recv().await {
            break status;
        }
    };
    assert_eq!(status, "Context compacted");
    let state = driver.snapshot();
    assert_eq!(state.history.len(), 4, "the command adds no history");
    assert_eq!(state.outcomes.len(), 2, "and no turn outcome");
    assert!(state.context_checkpoint.is_some());
}

#[tokio::test]
async fn compact_is_busy_during_a_turn() {
    let model = ScriptedModel::new();
    let gate = model.reply_gated(vec![text("slow")]);
    let (driver, mut events) = start(model, ToolRegistry::new());
    let id = driver.submit("hello");
    gate.started().await;
    driver.compact();
    let status = loop {
        if let Some(SessionEvent::CommandStatus(status)) = events.recv().await {
            break status;
        }
    };
    assert_eq!(status, COMPACT_BUSY);
    gate.release();
    events_until_turn_ends(&mut events, id).await;
    assert_eq!(driver.snapshot().history.len(), 2);
}
