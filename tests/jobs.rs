//! Background jobs and `run_command` in a real session with the real sandbox.
//! They need `bwrap`, `pasta`, and `bash` on `PATH`; without them each check
//! is skipped. See `docs/develop/Jobs.md#verification`.

mod support;

use std::{fs, sync::Arc, time::Duration};

use alyosha::{
    agent_loop::SessionEvent,
    agent_state::{HistoryEntry, ToolOutcome, TurnOutcome},
    clock::FixedClock,
    session::{self, FrontEnd, Started},
};
use alyosha_config::{Config, Paths};
use chrono::{TimeZone, Utc};
use serde_json::json;
use support::{ScriptedModel, call, events_until_turn_ends, text};

struct Home {
    _root: tempfile::TempDir,
    paths: Paths,
    workspace: std::path::PathBuf,
}

fn home() -> Option<Home> {
    let has = |name: &str| {
        std::env::var_os("PATH")
            .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(name).is_file()))
    };
    if !(has("bwrap") && has("pasta") && has("bash")) {
        eprintln!("skipped: bwrap, pasta, or bash is not on PATH");
        return None;
    }
    let root = tempfile::tempdir().unwrap();
    let base = root.path().canonicalize().unwrap();
    let paths = Paths::new(
        &base,
        Some(base.join("config").as_os_str()),
        Some(base.join("data").as_os_str()),
    );
    let workspace = base.join("Projects/ws");
    fs::create_dir_all(&workspace).unwrap();
    Some(Home {
        _root: root,
        paths,
        workspace,
    })
}

async fn open(home: &Home, model: Arc<ScriptedModel>) -> Started {
    session::start_with_clock(
        &Config::default(),
        &home.paths,
        &home.workspace,
        model,
        FrontEnd {
            approver: None,
            channel_line: None,
            log_file: None,
            on_loading_dev_shell: None,
            persistence: None,
        },
        Arc::new(FixedClock(
            Utc.with_ymd_and_hms(2026, 11, 25, 9, 2, 0).unwrap(),
        )),
    )
    .await
    .unwrap()
}

fn results(started: &Started) -> Vec<(String, ToolOutcome)> {
    started
        .driver
        .snapshot()
        .history
        .iter()
        .filter_map(|entry| match entry {
            HistoryEntry::ToolResult(result) => {
                Some((result.call_id.0.clone(), result.outcome.clone()))
            }
            _ => None,
        })
        .collect()
}

/// Waits for a finished job's report and returns its job ID and text.
async fn next_job_finished(started: &mut Started) -> (String, String) {
    loop {
        let event = tokio::time::timeout(Duration::from_secs(30), started.events.recv())
            .await
            .expect("a finished job")
            .unwrap();
        if let SessionEvent::JobFinished { job_id, text } = event {
            return (job_id, text);
        }
    }
}

#[tokio::test]
async fn run_command_reports_status_and_keeps_long_output() {
    let Some(home) = home() else { return };
    let model = ScriptedModel::new();
    model.reply(vec![
        call(
            "ok",
            "run_command",
            json!({"command": "echo hello; exit 3"}),
        ),
        call("long", "run_command", json!({"command": "seq 1 5000"})),
        call(
            "too-long",
            "run_command",
            json!({"command": "true", "timeout_seconds": 3600}),
        ),
        call(
            "path",
            "run_command",
            json!({"command": "echo made > out.txt; realpath out.txt"}),
        ),
    ]);
    model.reply(vec![text("Done.")]);
    let mut started = open(&home, model).await;
    let id = started.driver.submit("Run things.");
    events_until_turn_ends(&mut started.events, id).await;
    let results = results(&started);
    assert_eq!(
        results[0].1,
        ToolOutcome::Success("hello\n\n\n[status: 3]".into())
    );
    let ToolOutcome::Success(long) = &results[1].1 else {
        panic!()
    };
    assert!(
        long.contains(
            "[status: 0]\n[shortened: showed 8192 of 23893 bytes]\n[log_path: /session-logs/cmd-"
        ),
        "{}",
        &long[long.len() - 120..]
    );
    assert!(
        matches!(&results[2].1, ToolOutcome::Error(message) if message.contains("at most 1800"))
    );
    assert_eq!(
        results[3].1,
        ToolOutcome::Success("/workspace/out.txt\n\n\n[status: 0]".into())
    );
}

#[tokio::test]
async fn a_background_job_starts_a_turn_when_it_ends() {
    let Some(home) = home() else { return };
    let model = ScriptedModel::new();
    model.reply(vec![call(
        "bg",
        "run_command",
        json!({"command": "sleep 1; echo done", "background": true}),
    )]);
    model.reply(vec![text("Started it.")]);
    model.reply(vec![text("The job printed done.")]);
    let mut started = open(&home, model.clone()).await;
    let id = started.driver.submit("Run it in the background.");
    let seen = events_until_turn_ends(&mut started.events, id).await;
    assert!(seen.contains(&SessionEvent::Notice(
        "Job j1 started: sleep 1; echo done".into()
    )));
    let ToolOutcome::Success(result) = &results(&started)[0].1 else {
        panic!()
    };
    assert!(result.starts_with("status: Started\njob_id: j1\nlog_path: /session-logs/job-j1-"));
    assert_eq!(
        started.driver.snapshot().outcomes[&id],
        TurnOutcome::Completed
    );

    // Nothing joins the queue until the bridge sends the report back.
    let (job_id, report) = next_job_finished(&mut started).await;
    assert_eq!(job_id, "j1");
    assert!(started.driver.snapshot().pending_messages.is_empty());
    let job_turn = started.driver.submit(report);
    events_until_turn_ends(&mut started.events, job_turn).await;
    let state = started.driver.snapshot();
    let HistoryEntry::User(message) = &state.history[4] else {
        panic!("{:?}", state.history[4])
    };
    assert!(
        message.text.contains(
            "[Job j1 finished: status 0 after 1s · sleep 1; echo done]\nLast output:\ndone\n"
        ),
        "{}",
        message.text
    );
    assert!(model.requests().last().unwrap().history.len() == 5);
}

#[tokio::test]
async fn jobs_have_limits_and_stop_without_a_message() {
    let Some(home) = home() else { return };
    let model = ScriptedModel::new();
    model.reply(vec![
        call(
            "j1",
            "run_command",
            json!({"command": "sleep 60", "background": true}),
        ),
        call(
            "j2",
            "run_command",
            json!({"command": "sleep 60", "background": true}),
        ),
        call(
            "j3",
            "run_command",
            json!({"command": "sleep 60", "background": true}),
        ),
        call("stop", "job", json!({"command": "stop", "id": "j1"})),
        call("list", "job", json!({"command": "list"})),
        call("unknown", "job", json!({"command": "status", "id": "j9"})),
    ]);
    model.reply(vec![text("Ok.")]);
    let mut started = open(&home, model).await;
    let id = started.driver.submit("Start jobs.");
    events_until_turn_ends(&mut started.events, id).await;
    let results = results(&started);
    assert!(
        matches!(&results[2].1, ToolOutcome::Error(message) if message.contains("2 jobs are already running"))
    );
    assert!(
        matches!(&results[3].1, ToolOutcome::Success(text) if text.contains("state: Finished(Stopped)"))
    );
    assert!(
        matches!(&results[4].1, ToolOutcome::Success(text) if text.contains("j1 Finished(Stopped)") && text.contains("j2 Running"))
    );
    assert!(matches!(&results[5].1, ToolOutcome::Error(_)));

    // /job runs at once; the session ending kills the running jobs quietly.
    let status = started.driver.start_job("sleep 1; echo hi");
    assert_eq!(status, "Job j3 started: sleep 1; echo hi");
    let (_, text) = next_job_finished(&mut started).await;
    assert!(text.contains("· started by the owner with /job]"), "{text}");
    let ended = std::time::Instant::now();
    started.driver.end().await;
    assert!(ended.elapsed() < Duration::from_secs(6));
    assert_eq!(
        started
            .driver
            .services()
            .jobs
            .as_ref()
            .unwrap()
            .running_count(),
        0
    );
}

#[tokio::test]
async fn stop_ends_the_turn_but_not_the_job() {
    let Some(home) = home() else { return };
    let model = ScriptedModel::new();
    model.reply(vec![
        call(
            "bg",
            "run_command",
            json!({"command": "sleep 30", "background": true}),
        ),
        call("fg", "run_command", json!({"command": "sleep 30"})),
    ]);
    let mut started = open(&home, model).await;
    let id = started.driver.submit("Go.");
    loop {
        let event = tokio::time::timeout(Duration::from_secs(10), started.events.recv())
            .await
            .unwrap()
            .unwrap();
        if matches!(&event, SessionEvent::ToolStarted { call_id, .. } if call_id.0 == "fg") {
            break;
        }
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    started.driver.stop();
    events_until_turn_ends(&mut started.events, id).await;
    assert_eq!(
        started.driver.snapshot().outcomes[&id],
        TurnOutcome::Stopped
    );
    let results = results(&started);
    assert!(
        matches!(&results[1].1, ToolOutcome::Success(text) if text.contains("[status: Stopped]"))
    );
    assert_eq!(
        started
            .driver
            .services()
            .jobs
            .as_ref()
            .unwrap()
            .running_count(),
        1,
        "the job keeps running"
    );
    started.driver.end().await;
}
