//! Whole sessions started like `alyosha acp` starts them, in a temporary home, with
//! a scripted model: the file tools, Code Mode, memory, skills, MCP, and
//! approvals. See the verification sections of the pages in `docs/`.

mod support;

use std::{
    collections::VecDeque,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use alyosha::{
    BoxFuture,
    agent_loop::SessionEvent,
    agent_state::{HistoryEntry, ToolOutcome, TurnOutcome, TurnPhase},
    approvals::{Approver, OwnerAnswer, OwnerRequest},
    clock::FixedClock,
    session::{self, FrontEnd, Started},
};
use alyosha_config::{Config, McpServerConfig, Paths};
use chrono::{TimeZone, Utc};
use serde_json::{Value, json};
use support::{ScriptedModel, call, events_until_turn_ends, text};
use tokio_util::sync::CancellationToken;

struct Home {
    _root: tempfile::TempDir,
    paths: Paths,
    workspace: PathBuf,
}

fn home() -> Home {
    let root = tempfile::tempdir().unwrap();
    let base = root.path().canonicalize().unwrap();
    let paths = Paths::new(
        &base,
        Some(base.join("config").as_os_str()),
        Some(base.join("data").as_os_str()),
    );
    let workspace = base.join("Projects/ws");
    fs::create_dir_all(&workspace).unwrap();
    fs::create_dir_all(&paths.data_dir).unwrap();
    Home {
        _root: root,
        paths,
        workspace,
    }
}

/// Answers approvals and questions from a script and records each request.
#[derive(Default)]
struct ScriptedApprover {
    answers: Mutex<VecDeque<OwnerAnswer>>,
    requests: Mutex<Vec<OwnerRequest>>,
}

impl ScriptedApprover {
    fn with(answers: &[OwnerAnswer]) -> Arc<Self> {
        Arc::new(Self {
            answers: Mutex::new(answers.iter().copied().collect()),
            requests: Mutex::default(),
        })
    }
}

impl Approver for ScriptedApprover {
    fn ask(&self, request: OwnerRequest, cancel: CancellationToken) -> BoxFuture<'_, OwnerAnswer> {
        self.requests.lock().unwrap().push(request);
        let answer = self.answers.lock().unwrap().pop_front();
        Box::pin(async move {
            match answer {
                Some(answer) => answer,
                // No scripted answer: wait like an owner who never replies.
                None => {
                    cancel.cancelled().await;
                    OwnerAnswer::Cancelled
                }
            }
        })
    }
}

async fn open(
    home: &Home,
    model: Arc<ScriptedModel>,
    config: &Config,
    approver: Option<Arc<dyn Approver>>,
) -> Started {
    session::start_with_clock(
        config,
        &home.paths,
        &home.workspace,
        model,
        FrontEnd {
            approver,
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

/// Runs one message and returns the tool results recorded for it.
async fn turn(
    started: &mut Started,
    message: &str,
) -> (Vec<SessionEvent>, Vec<(String, ToolOutcome)>, TurnOutcome) {
    let before = started.driver.snapshot().history.len();
    let id = started.driver.submit(message);
    let events = events_until_turn_ends(&mut started.events, id).await;
    let state = started.driver.snapshot();
    let results = state.history[before..]
        .iter()
        .filter_map(|entry| match entry {
            HistoryEntry::ToolResult(result) => {
                Some((result.call_id.0.clone(), result.outcome.clone()))
            }
            _ => None,
        })
        .collect();
    (events, results, state.outcomes[&id].clone())
}

fn success(outcome: &ToolOutcome) -> &str {
    match outcome {
        ToolOutcome::Success(text) => text,
        other => panic!("expected success, got {other:?}"),
    }
}

fn error(outcome: &ToolOutcome) -> &str {
    match outcome {
        ToolOutcome::Error(text) => text,
        other => panic!("expected an error, got {other:?}"),
    }
}

#[tokio::test]
async fn the_file_tools_work_in_the_workspace() {
    let home = home();
    let model = ScriptedModel::new();
    model.reply(vec![
        call(
            "w",
            "write_file",
            json!({"path": "notes.md", "content": "Decision: ship it\nTodo: tests\n"}),
        ),
        call(
            "e",
            "edit_file",
            json!({"path": "notes.md", "old_text": "Todo: tests", "new_text": "Done: tests"}),
        ),
        call("r", "read_file", json!({"path": "/workspace/notes.md"})),
        call(
            "dup",
            "edit_file",
            json!({"path": "notes.md", "old_text": "s", "new_text": "S"}),
        ),
        call("missing", "read_file", json!({"path": "absent.md"})),
        call(
            "skills",
            "write_file",
            json!({"path": "/skills/x/SKILL.md", "content": "x"}),
        ),
        call("outside", "read_file", json!({"path": "/etc/passwd"})),
    ]);
    model.reply(vec![text("Done.")]);
    let mut started = open(&home, model, &Config::default(), None).await;
    let (_, results, outcome) = turn(&mut started, "Write and edit notes.").await;
    assert_eq!(outcome, TurnOutcome::Completed);
    assert_eq!(success(&results[0].1), "bytes_written: 30");
    assert_eq!(success(&results[1].1), "Edited notes.md.");
    assert_eq!(success(&results[2].1), "Decision: ship it\nDone: tests\n");
    assert!(error(&results[3].1).contains("occurs 4 times in notes.md"));
    assert!(error(&results[4].1).contains("no such file"));
    assert!(error(&results[5].1).contains("read-only"));
    assert!(error(&results[6].1).contains("outside the workspace"));
    assert_eq!(
        fs::read_to_string(home.workspace.join("notes.md")).unwrap(),
        "Decision: ship it\nDone: tests\n"
    );
}

#[tokio::test]
async fn a_long_read_records_where_to_continue() {
    let home = home();
    fs::write(home.workspace.join("big.txt"), "a".repeat(20_000)).unwrap();
    let model = ScriptedModel::new();
    model.reply(vec![call("r1", "read_file", json!({"path": "big.txt"}))]);
    model.reply(vec![call(
        "r2",
        "read_file",
        json!({"path": "big.txt", "offset": 16384}),
    )]);
    model.reply(vec![text("Read it all.")]);
    let mut started = open(&home, model, &Config::default(), None).await;
    let (_, results, _) = turn(&mut started, "Read big.txt.").await;
    let first = success(&results[0].1);
    assert!(
        first.ends_with("\n\n[next_offset: 8192]"),
        "{}",
        &first[first.len() - 40..]
    );
    assert_eq!(success(&results[1].1).len(), 20_000 - 16384);
}

#[tokio::test]
async fn code_mode_reads_and_filters_in_one_call() {
    let home = home();
    fs::create_dir_all(home.workspace.join("notes")).unwrap();
    fs::write(
        home.workspace.join("notes/monday.md"),
        "Decision: use Rust\nchat\n",
    )
    .unwrap();
    fs::write(
        home.workspace.join("notes/friday.md"),
        "noise\nDecision: ship Friday\n",
    )
    .unwrap();
    let script = r#"async (tools) => {
      const decisions = [];
      for (const path of ["notes/monday.md", "notes/friday.md"]) {
        const { text } = await tools.read_file({ path, max_bytes: 4096 });
        for (const line of text.split("\n")) {
          if (line.startsWith("Decision:")) decisions.push(path + "  " + line.trim());
        }
      }
      let denied = [];
      for (const path of ["/session-logs/cmd-1.log", "/skills/x/SKILL.md", "/etc/passwd"]) {
        try { await tools.read_file({ path }); } catch (e) { denied.push(path); }
      }
      let overlapping = "allowed";
      try { await Promise.all([tools.read_file({ path: "notes/monday.md" }), tools.read_file({ path: "notes/friday.md" })]); }
      catch (e) { overlapping = "rejected"; }
      return decisions.join("\n") + "\ndenied " + denied.length + "\noverlap " + overlapping
        + "\nmemory " + typeof tools.memory + "\nmcp " + typeof tools.mcp__fake__echo;
    }"#;
    let model = ScriptedModel::new();
    model.reply(vec![call("code", "execute_code", json!({"code": script}))]);
    model.reply(vec![
        call("bad", "execute_code", json!({"code": "async () => 42"})),
        call("syntax", "execute_code", json!({"code": "async () => {"})),
        call(
            "throws",
            "execute_code",
            json!({"code": "async () => { throw new Error('boom') }"}),
        ),
    ]);
    model.reply(vec![text("Two decisions.")]);
    let mut started = open(&home, model.clone(), &Config::default(), None).await;
    let (_, results, _) = turn(&mut started, "List every decision.").await;
    assert_eq!(results.len(), 4, "one outer result per script");
    assert_eq!(
        success(&results[0].1),
        "notes/monday.md  Decision: use Rust\nnotes/friday.md  Decision: ship Friday\ndenied 3\noverlap rejected\nmemory undefined\nmcp undefined"
    );
    assert!(error(&results[1].1).contains("must resolve to a string"));
    assert!(error(&results[2].1).contains("not a valid function expression"));
    assert!(error(&results[3].1).contains("boom"));
}

#[tokio::test]
async fn stop_ends_a_script_that_computes_or_waits_forever() {
    for script in [
        "async () => { while (true) {} }",
        "async () => new Promise(() => {})",
    ] {
        let home = home();
        let model = ScriptedModel::new();
        model.reply(vec![call("loop", "execute_code", json!({"code": script}))]);
        let mut started = open(&home, model, &Config::default(), None).await;
        let id = started.driver.submit("Run it.");
        loop {
            let event = tokio::time::timeout(Duration::from_secs(5), started.events.recv())
                .await
                .unwrap()
                .unwrap();
            if matches!(event, SessionEvent::ToolStarted { .. }) {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        started.driver.stop();
        events_until_turn_ends(&mut started.events, id).await;
        let state = started.driver.snapshot();
        assert_eq!(state.outcomes[&id], TurnOutcome::Stopped, "{script}");
        let HistoryEntry::ToolResult(result) = state.history.last().unwrap() else {
            panic!()
        };
        assert_eq!(result.outcome, ToolOutcome::Cancelled, "{script}");
    }
}

#[tokio::test]
async fn memory_writes_show_a_notice_and_reach_the_next_session() {
    let home = home();
    let model = ScriptedModel::new();
    model.reply(vec![call(
        "m",
        "memory",
        json!({"command": "write", "name": "editor", "description": "The owner uses Helix", "content": "Helix, not Vim."}),
    )]);
    model.reply(vec![text("Noted.")]);
    // The store dates a write with today in the configured timezone.
    let today = || {
        Utc::now()
            .with_timezone(&Config::default().timezone)
            .date_naive()
    };
    let before = today();
    let mut started = open(&home, model.clone(), &Config::default(), None).await;
    let (events, results, _) = turn(&mut started, "I use Helix.").await;
    let after = today();
    assert_eq!(success(&results[0].1), "created");
    assert!(events.contains(&SessionEvent::Notice("Remembered: editor".into())));
    let requests = model.requests();
    assert!(requests[0].system_instruction.contains("No memories yet."));
    assert_eq!(
        requests[0].system_instruction, requests[1].system_instruction,
        "fixed for the session"
    );

    let next = ScriptedModel::new();
    next.reply(vec![text("Hi.")]);
    let mut second = open(&home, next.clone(), &Config::default(), None).await;
    turn(&mut second, "Hello.").await;
    let instruction = &next.requests()[0].system_instruction;
    assert!(
        [before, after]
            .iter()
            .any(|date| instruction.contains(&format!("- editor ({date}): The owner uses Helix"))),
        "{instruction}"
    );
}

#[tokio::test]
async fn skills_are_indexed_and_loaded() {
    let home = home();
    let skill = home.paths.config_dir.join("skills/blog-release");
    fs::create_dir_all(skill.join("scripts")).unwrap();
    fs::write(
        skill.join("SKILL.md"),
        "---\nname: blog-release\ndescription: Publish a blog post\n---\n1. Check links.\n",
    )
    .unwrap();
    fs::write(skill.join("scripts/check.sh"), "echo ok").unwrap();
    fs::create_dir_all(home.paths.config_dir.join("skills/Broken")).unwrap();
    fs::write(
        home.paths.config_dir.join("skills/Broken/SKILL.md"),
        "no header",
    )
    .unwrap();
    let model = ScriptedModel::new();
    model.reply(vec![
        call("s", "skill", json!({"name": "blog-release"})),
        call(
            "f",
            "read_file",
            json!({"path": "/skills/blog-release/scripts/check.sh"}),
        ),
    ]);
    model.reply(vec![text("Loaded.")]);
    let mut started = open(&home, model.clone(), &Config::default(), None).await;
    assert!(
        started
            .warnings
            .iter()
            .any(|warning| warning == "1 skills could not be loaded: Broken")
    );
    let (_, results, _) = turn(&mut started, "Release the post.").await;
    assert_eq!(
        success(&results[0].1),
        "Skill: blog-release (/skills/blog-release/SKILL.md)\nFiles:\n/skills/blog-release/scripts/check.sh\n\n1. Check links.\n"
    );
    assert_eq!(success(&results[1].1), "echo ok");
    assert!(
        model.requests()[0]
            .system_instruction
            .contains("## Skills\n")
    );
    assert!(
        model.requests()[0]
            .system_instruction
            .contains("- blog-release: Publish a blog post")
    );
}

#[tokio::test]
async fn ask_owner_offers_choices_and_reports_the_answer() {
    let home = home();
    let approver = ScriptedApprover::with(&[
        OwnerAnswer::Chosen(0),
        OwnerAnswer::Chosen(1),
        OwnerAnswer::Chosen(3),
        OwnerAnswer::NoAnswer,
    ]);
    let model = ScriptedModel::new();
    model.reply(vec![
        call("q1", "ask_owner", json!({"question": "Book it?"})),
        call(
            "q2",
            "ask_owner",
            json!({"question": "Which flight?", "choices": ["KL1", "KL2", "KL3"]}),
        ),
        call(
            "q3",
            "ask_owner",
            json!({"question": "Which seat?", "choices": ["A", "B", "C"]}),
        ),
        call("q4", "ask_owner", json!({"question": "Still there?"})),
        call(
            "q5",
            "ask_owner",
            json!({"question": "Too many", "choices": ["1", "2", "3", "4", "5", "6", "7"]}),
        ),
    ]);
    model.reply(vec![text("OK.")]);
    let mut started = open(&home, model, &Config::default(), Some(approver.clone())).await;
    let (events, results, _) = turn(&mut started, "Plan the trip.").await;
    assert_eq!(success(&results[0].1), "The owner answered: Yes");
    assert_eq!(success(&results[1].1), "The owner chose: KL2");
    assert_eq!(
        success(&results[2].1),
        "The owner chose none of the choices."
    );
    assert_eq!(
        success(&results[3].1),
        "The owner did not answer within 60 minutes."
    );
    assert!(error(&results[4].1).starts_with("Invalid arguments for ask_owner"));
    let requests = approver.requests.lock().unwrap();
    assert_eq!(requests[0].options, ["Yes", "No"]);
    assert_eq!(requests[1].options, ["KL1", "KL2", "KL3", "None of these"]);
    assert_eq!(requests[0].call_id, "q1");
    assert!(events.contains(&SessionEvent::Notice("Asking the owner".into())));
}

fn python() -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join("python3"))
            .find(|candidate| candidate.is_file())
    })
}

fn fake_server_config(home: &Home, ask: &[&str]) -> Option<Config> {
    let python = python()?;
    let mut config = Config::default();
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_mcp_server.py");
    let mut env = std::collections::BTreeMap::new();
    env.insert(
        "FAKE_MCP_LOG".to_owned(),
        toml_secret(&home.paths.data_dir.join("fake.log").display().to_string()),
    );
    config.mcp.servers.insert(
        "fake".to_owned(),
        McpServerConfig {
            command: python.display().to_string(),
            args: vec!["-I".into(), script.display().to_string()],
            tools: None,
            ask: ask.iter().map(|name| (*name).to_owned()).collect(),
            env,
        },
    );
    config.mcp.servers.insert(
        "missing".to_owned(),
        McpServerConfig {
            command: "/nonexistent/server".into(),
            args: Vec::new(),
            tools: None,
            ask: Vec::new(),
            env: Default::default(),
        },
    );
    Some(config)
}

fn toml_secret(value: &str) -> alyosha_config::Secret {
    alyosha_config::Secret::new(value)
}

#[tokio::test]
async fn mcp_tools_register_call_and_convert() {
    let home = home();
    let Some(config) = fake_server_config(&home, &[]) else {
        eprintln!("skipped: python3 is not on PATH");
        return;
    };
    let model = ScriptedModel::new();
    model.reply(vec![
        call("c1", "mcp__fake__echo", json!({"text": "hello"})),
        call("c2", "mcp__fake__image", json!({})),
        call("c3", "mcp__fake__fail", json!({})),
        call("c4", "mcp__fake__echo", json!({"wrong": 1})),
        call("c5", "mcp__fake__get_item", json!({})),
        call("c6", "mcp__fake__env", json!({})),
        call("c7", "mcp__fake__big", json!({})),
    ]);
    model.reply(vec![text("Done.")]);
    let mut started = open(&home, model.clone(), &config, None).await;
    assert!(
        started
            .warnings
            .iter()
            .any(|warning| warning.starts_with("MCP: 2 problems at startup")
                && warning.contains("missing: failed to start")
                && warning.contains("longer than 64 bytes")),
        "{:?}",
        started.warnings
    );
    let names: Vec<String> = model_tools(&model, &mut started).await;
    assert!(
        names.contains(&"mcp__fake__echo".to_owned())
            && names.contains(&"mcp__fake__get_item".to_owned())
    );

    let (events, results, _) = turn(&mut started, "Use the fake server.").await;
    assert_eq!(success(&results[0].1), "hello");
    assert_eq!(success(&results[1].1), "[image omitted: image/png]");
    assert_eq!(error(&results[2].1), "it broke");
    assert!(error(&results[3].1).starts_with("Invalid arguments for mcp__fake__echo"));
    assert_eq!(
        success(&results[4].1),
        "get.item",
        "called by its original name"
    );
    let report: Value = serde_json::from_str(success(&results[5].1)).unwrap();
    let names: Vec<&str> = report["env"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(
        names.iter().all(|name| [
            "PATH",
            "HOME",
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
            "FAKE_MCP_LOG"
        ]
        .contains(name)),
        "{names:?}"
    );
    assert!(names.contains(&"FAKE_MCP_LOG"));
    assert_eq!(report["cwd"], home.workspace.display().to_string());
    assert!(events.iter().any(|event| matches!(event, SessionEvent::ToolStarted { title, .. } if title == "MCP: fake.echo")));

    // The 50 KB reply keeps its start and a log the model can read on.
    let big = success(&results[6].1);
    let (_, metadata) = alyosha::context_manager::split_metadata(big);
    let log_path = metadata
        .lines()
        .find_map(|line| {
            line.strip_prefix("[log_path: ")
                .and_then(|rest| rest.strip_suffix(']'))
        })
        .expect("a log path");
    let read = ScriptedModel::new();
    let _ = read;
    let state = started.driver.snapshot();
    assert!(matches!(
        state.history.last(),
        Some(HistoryEntry::Assistant(_))
    ));
    let model2 = model.clone();
    model2.reply(vec![
        call("c8", "read_file", json!({"path": log_path, "offset": 8192, "max_bytes": 10})),
        call("c9", "execute_code", json!({"code": format!("async (t) => (await t.read_file({{ path: {log_path:?} }})).text")})),
    ]);
    model2.reply(vec![text("Read.")]);
    let (_, results, _) = turn(&mut started, "Read the rest.").await;
    let digits: String = (8192..8202)
        .map(|i| char::from(b'0' + (i % 10) as u8))
        .collect();
    assert_eq!(success(&results[0].1).split("\n\n").next().unwrap(), digits);
    assert!(
        error(&results[1].1).contains("cannot be opened here")
            || error(&results[1].1).contains("outside")
    );
    started.driver.end().await;
    let log_file = home.paths.data_dir.join("sessions");
    let leftover: Vec<_> = walk(&log_file)
        .into_iter()
        .filter(|path| path.to_string_lossy().contains("mcp-"))
        .collect();
    assert!(
        leftover.is_empty(),
        "logs removed at session end: {leftover:?}"
    );
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                found.extend(walk(&path));
            } else {
                found.push(path);
            }
        }
    }
    found
}

/// The tool names the session advertises, from one throwaway turn.
async fn model_tools(model: &Arc<ScriptedModel>, _started: &mut Started) -> Vec<String> {
    // The first request has not been made yet; use the definitions the
    // driver will send by asking for one direct reply.
    let _ = model;
    _started
        .driver
        .services()
        .mcp
        .as_ref()
        .map(|mcp| {
            mcp.tools()
                .into_iter()
                .map(|tool| tool.definition.name)
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test]
async fn stop_cancels_an_mcp_call_at_the_server() {
    let home = home();
    let Some(config) = fake_server_config(&home, &[]) else {
        return;
    };
    let model = ScriptedModel::new();
    model.reply(vec![
        call("s1", "mcp__fake__sleep", json!({})),
        call("s2", "mcp__fake__echo", json!({"text": "x"})),
    ]);
    let mut started = open(&home, model, &config, None).await;
    let id = started.driver.submit("Sleep.");
    loop {
        let event = tokio::time::timeout(Duration::from_secs(5), started.events.recv())
            .await
            .unwrap()
            .unwrap();
        if matches!(event, SessionEvent::ToolStarted { .. }) {
            break;
        }
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    started.driver.stop();
    events_until_turn_ends(&mut started.events, id).await;
    let state = started.driver.snapshot();
    assert_eq!(state.outcomes[&id], TurnOutcome::Stopped);
    let results: Vec<_> = state
        .history
        .iter()
        .filter_map(|entry| match entry {
            HistoryEntry::ToolResult(result) => Some(result.outcome.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(results, [ToolOutcome::Cancelled, ToolOutcome::Cancelled]);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let log = fs::read_to_string(home.paths.data_dir.join("fake.log")).unwrap();
    assert!(log.contains("call sleep\ncancelled "), "{log}");
    assert!(!log.contains("call echo"));
}

#[tokio::test]
async fn an_ask_tool_runs_only_after_approval() {
    let home = home();
    let Some(config) = fake_server_config(&home, &["send"]) else {
        return;
    };
    let approver = ScriptedApprover::with(&[
        OwnerAnswer::Chosen(0),
        OwnerAnswer::Chosen(1),
        OwnerAnswer::NoAnswer,
        OwnerAnswer::Undeliverable,
    ]);
    let model = ScriptedModel::new();
    let send = |id: &str| {
        call(
            id,
            "mcp__fake__send",
            json!({"to": "anna@example.com", "body": "Hi Anna,\nFriday at 7 works."}),
        )
    };
    model.reply(vec![
        send("a1"),
        send("a2"),
        send("a3"),
        send("a4"),
        call("e", "mcp__fake__echo", json!({"text": "x"})),
    ]);
    model.reply(vec![text("Done.")]);
    let mut started = open(&home, model, &config, Some(approver.clone())).await;
    let (events, results, _) = turn(&mut started, "Email Anna.").await;
    assert_eq!(success(&results[0].1), "sent to anna@example.com");
    assert_eq!(
        error(&results[1].1),
        "The owner declined this call. It did not run."
    );
    assert_eq!(
        error(&results[2].1),
        "The owner did not answer within 60 minutes. The call did not run."
    );
    assert_eq!(
        error(&results[3].1),
        "The approval request could not be delivered. The call did not run."
    );
    assert_eq!(success(&results[4].1), "x");
    let requests = approver.requests.lock().unwrap();
    assert_eq!(requests.len(), 4, "echo is not marked ask");
    assert_eq!(requests[0].title, "Approve? fake.send");
    assert_eq!(
        requests[0].body,
        "to: anna@example.com\nbody:\nHi Anna,\nFriday at 7 works."
    );
    assert_eq!(requests[0].options, ["Approve", "Decline"]);
    assert!(events.contains(&SessionEvent::Notice(
        "Waiting for approval: fake.send".into()
    )));
    let log = fs::read_to_string(home.paths.data_dir.join("fake.log")).unwrap();
    assert_eq!(
        log.matches("call send").count(),
        1,
        "only the approved call reached the server"
    );
}

#[tokio::test]
async fn stop_while_waiting_for_the_owner_declines_and_stops() {
    let home = home();
    let Some(config) = fake_server_config(&home, &["send"]) else {
        return;
    };
    let approver = ScriptedApprover::with(&[]);
    let model = ScriptedModel::new();
    model.reply(vec![call(
        "a1",
        "mcp__fake__send",
        json!({"to": "x", "body": "y"}),
    )]);
    let mut started = open(&home, model, &config, Some(approver)).await;
    let id = started.driver.submit("Send it.");
    loop {
        let event = tokio::time::timeout(Duration::from_secs(5), started.events.recv())
            .await
            .unwrap()
            .unwrap();
        if event == SessionEvent::Status(alyosha::agent_loop::Status::WaitingForOwner) {
            break;
        }
    }
    let waiting = started.driver.snapshot();
    assert_eq!(
        waiting.active_turn().unwrap().phase,
        TurnPhase::WaitingForOwner {
            call_id: "a1".into()
        }
    );
    started.driver.stop();
    events_until_turn_ends(&mut started.events, id).await;
    assert_eq!(
        started.driver.snapshot().outcomes[&id],
        TurnOutcome::Stopped
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    let log = fs::read_to_string(home.paths.data_dir.join("fake.log")).unwrap_or_default();
    assert!(!log.contains("call send"));
}

#[tokio::test]
async fn a_dead_server_fails_its_calls_and_the_session_continues() {
    let home = home();
    let Some(config) = fake_server_config(&home, &[]) else {
        return;
    };
    let model = ScriptedModel::new();
    model.reply(vec![call("x", "mcp__fake__exit", json!({}))]);
    model.reply(vec![call("y", "mcp__fake__echo", json!({"text": "hi"}))]);
    model.reply(vec![text("It stopped.")]);
    let mut started = open(&home, model, &config, None).await;
    let (_, results, outcome) = turn(&mut started, "Exit, then echo.").await;
    assert!(error(&results[0].1).contains("has stopped"));
    assert!(error(&results[1].1).contains("has stopped"));
    assert_eq!(outcome, TurnOutcome::Completed);
}

#[tokio::test]
async fn a_resumed_session_closes_the_turn_a_restart_cut_off() {
    use alyosha::{
        agent_loop::Persistence,
        session_store::{
            INTERRUPTED_CALL, INTERRUPTED_LINE, MemoryStore, RESUMED_LINE, SessionStore,
        },
    };
    let home = home();
    let store = MemoryStore::new();
    let created = store.create("owner:telegram", &home.workspace).await.unwrap();
    // The process died after the first of two calls had its result.
    let saved = [
        support::user(alyosha::agent_state::MessageId(4), "read both"),
        support::assistant(vec![
            call("call_a", "read_file", json!({"path": "a.txt"})),
            call("call_b", "read_file", json!({"path": "b.txt"})),
        ]),
        support::tool_result("call_a", ToolOutcome::Success("alpha".into())),
    ];
    for (seq, entry) in saved.iter().enumerate() {
        store.append(created.id, seq as u64, entry).await.unwrap();
    }
    store.close(created.id).await.unwrap();
    let stored = store.load_current("owner:telegram").await.unwrap().unwrap();
    let model = ScriptedModel::new();
    model.reply(vec![text("Done.")]);
    let mut started = session::start_with_clock(
        &Config::default(),
        &home.paths,
        &home.workspace,
        model,
        FrontEnd {
            approver: None,
            channel_line: None,
            log_file: None,
            on_loading_dev_shell: None,
            persistence: Some(Persistence {
                store: store.clone() as Arc<dyn SessionStore>,
                stored,
                resumed: true,
            }),
        },
        Arc::new(FixedClock(
            Utc.with_ymd_and_hms(2026, 11, 25, 9, 2, 0).unwrap(),
        )),
    )
    .await
    .unwrap();
    assert_eq!(
        started.warnings[0],
        format!("{RESUMED_LINE} {INTERRUPTED_LINE}")
    );
    let history = started.driver.snapshot().history;
    assert_eq!(history.len(), 4);
    assert_eq!(
        history[3],
        support::tool_result("call_b", ToolOutcome::Error(INTERRUPTED_CALL.into()))
    );
    let (_, _, outcome) = turn(&mut started, "and now?").await;
    assert_eq!(outcome, TurnOutcome::Completed);
    let id = started.driver.snapshot().history[4].clone();
    let HistoryEntry::User(user) = id else {
        panic!("the new message follows");
    };
    assert_eq!(user.message_id.0, 5, "IDs continue after the stored ones");
    started.driver.end().await;
}
