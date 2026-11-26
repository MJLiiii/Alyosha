//! Durable sessions through the session driver, with `MemoryStore` and the
//! scripted model. See `docs/develop/SessionStore.md#verification`.

mod support;

use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use alyosha::{
    agent_loop::{Persistence, SessionDriver, SessionEvent, SessionSetup},
    agent_state::{MessageId, TurnOutcome},
    clock::FixedClock,
    context_manager::RouteLimits,
    session_store::{MemoryStore, SessionStore, StoredSession},
    tools::ToolRegistry,
};
use chrono::{TimeZone, Utc};
use serde_json::json;
use support::{ScriptedModel, call, events_until_turn_ends, files_registry, text};
use tokio::sync::mpsc::UnboundedReceiver;

fn start(
    model: Arc<ScriptedModel>,
    registry: ToolRegistry,
    store: &Arc<MemoryStore>,
    stored: StoredSession,
    rebuilds: Option<Arc<AtomicUsize>>,
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
    // The context fixture: 10,000 tokens of usable input.
    setup.limits = RouteLimits {
        context_window: 20_000,
        default_output: 4_000,
        input_ceiling: Some(12_000),
    };
    setup.persistence = Some(Persistence {
        store: store.clone() as Arc<dyn SessionStore>,
        stored,
        resumed: false,
    });
    if let Some(rebuilds) = rebuilds {
        setup.rebuild_instruction = Some(Arc::new(move || {
            format!(
                "instruction {}",
                rebuilds.fetch_add(1, Ordering::SeqCst) + 1
            )
        }));
    }
    SessionDriver::start(setup)
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

async fn compact(driver: &SessionDriver, events: &mut UnboundedReceiver<SessionEvent>) -> String {
    driver.compact();
    loop {
        if let Some(SessionEvent::CommandStatus(status)) = events.recv().await {
            return status;
        }
    }
}

/// About 3,000 tokens of text.
fn big(tag: &str) -> String {
    format!("{tag} {}", "x".repeat(9_000))
}

fn two_file_turn(model: &ScriptedModel) {
    model.reply(vec![
        call("call_a", "read_file", json!({"path": "a.txt"})),
        call("call_b", "read_file", json!({"path": "b.txt"})),
    ]);
    model.reply(vec![text("A says alpha; B is missing.")]);
}

#[tokio::test]
async fn every_entry_is_stored_in_order() {
    let store = MemoryStore::new();
    let stored = store.create("owner:telegram", Path::new("/w")).await.unwrap();
    let id = stored.id;
    let model = ScriptedModel::new();
    two_file_turn(&model);
    let (driver, mut events) = start(model, files_registry(Arc::default()), &store, stored, None);
    assert_eq!(
        run(&driver, &mut events, "read both").await,
        TurnOutcome::Completed
    );
    assert_eq!(store.seqs(id), [0, 1, 2, 3, 4]);
    assert_eq!(store.entries(id), driver.snapshot().history);
}

#[tokio::test]
async fn a_failed_save_fails_the_turn_and_keeps_memory_and_store_equal() {
    let store = MemoryStore::new();
    let stored = store.create("owner:telegram", Path::new("/w")).await.unwrap();
    let id = stored.id;
    let model = ScriptedModel::new();
    two_file_turn(&model);
    model.reply(vec![text("Still here.")]);
    let (driver, mut events) = start(model, files_registry(Arc::default()), &store, stored, None);
    // The message, the reply with two calls, and the first result.
    store.fail_appends_after(3);
    let TurnOutcome::Failed(reason) = run(&driver, &mut events, "read both").await else {
        panic!("the turn fails");
    };
    assert!(reason.starts_with("Could not save history"), "{reason}");
    let history = driver.snapshot().history;
    assert_eq!(history.len(), 3);
    assert_eq!(store.entries(id), history);

    store.fail_appends_after(usize::MAX);
    assert_eq!(
        run(&driver, &mut events, "still there?").await,
        TurnOutcome::Completed
    );
    assert_eq!(store.entries(id), driver.snapshot().history);
}

#[tokio::test]
async fn compaction_saves_the_checkpoint_and_rebuilds_the_instruction() {
    let store = MemoryStore::new();
    let stored = store.create("owner:telegram", Path::new("/w")).await.unwrap();
    let id = stored.id;
    let model = ScriptedModel::new();
    for reply in ["one", "two"] {
        model.reply(vec![text(reply)]);
    }
    model.reply(vec![text("SUMMARY")]);
    model.reply(vec![text("three")]);
    let rebuilds = Arc::new(AtomicUsize::new(0));
    let (driver, mut events) = start(
        model.clone(),
        ToolRegistry::new(),
        &store,
        stored,
        Some(rebuilds.clone()),
    );
    run(&driver, &mut events, &big("first")).await;
    run(&driver, &mut events, &big("second")).await;
    assert_eq!(compact(&driver, &mut events).await, "Context compacted");
    let checkpoint = driver.snapshot().context_checkpoint.unwrap();
    assert_eq!(
        store.checkpoint(id),
        Some((checkpoint.first_kept_index as u64, "SUMMARY".to_owned()))
    );
    assert_eq!(rebuilds.load(Ordering::SeqCst), 1);
    assert_eq!(driver.system_instruction(), "instruction 1");
    run(&driver, &mut events, "third").await;
    assert_eq!(
        model.requests().last().unwrap().system_instruction,
        "instruction 1",
        "requests after the compaction use the new instruction"
    );
}

#[tokio::test]
async fn a_resumed_session_continues_from_its_checkpoint() {
    let store = MemoryStore::new();
    let stored = store
        .create("owner:telegram", Path::new("/w"))
        .await
        .unwrap();
    let model = ScriptedModel::new();
    for reply in ["one", "two"] {
        model.reply(vec![text(reply)]);
    }
    model.reply(vec![text("SUMMARY")]);
    let (driver, mut events) = start(model, ToolRegistry::new(), &store, stored, None);
    run(&driver, &mut events, &big("first")).await;
    run(&driver, &mut events, &big("second")).await;
    assert_eq!(compact(&driver, &mut events).await, "Context compacted");
    let before = driver.snapshot();
    let kept = before.context_checkpoint.as_ref().unwrap().first_kept_index;
    driver.end().await;

    let loaded = store.load_current("owner:telegram").await.unwrap().unwrap();
    assert_eq!(loaded.base_seq(), kept as u64);
    assert_eq!(loaded.entries, before.history[kept..]);
    let model = ScriptedModel::new();
    model.reply(vec![text("three")]);
    let (driver, mut events) = start(model.clone(), ToolRegistry::new(), &store, loaded, None);
    let state = driver.snapshot();
    assert_eq!(state.history, before.history[kept..]);
    assert_eq!(
        state.context_checkpoint.as_ref().unwrap().first_kept_index,
        0
    );

    let id = driver.submit("third");
    assert!(id > MessageId(2), "no stored message ID is reused");
    events_until_turn_ends(&mut events, id).await;
    let request = model.requests().pop().unwrap();
    assert_eq!(request.summary.as_deref(), Some("SUMMARY"));
    assert_eq!(request.history.len(), before.history.len() - kept + 1);
    // The new entries follow the stored ones.
    let all = store.entries(state_id(&store, "owner:telegram").await);
    assert_eq!(all.len(), before.history.len() + 2);
}

/// The ID of the key's current session, released again at once.
async fn state_id(store: &Arc<MemoryStore>, key: &str) -> alyosha::session_store::SessionId {
    store.drop_locks();
    let loaded = store.load_current(key).await.unwrap().unwrap();
    store.close(loaded.id).await.unwrap();
    loaded.id
}
