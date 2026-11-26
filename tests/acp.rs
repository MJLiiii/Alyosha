//! `alyosha acp` with an in-process ACP client and the scripted model. See
//! `docs/Channels.md#verification`.

mod support;

use std::{
    fs,
    sync::{Arc, Mutex},
    time::Duration,
};

use alyosha::{
    acp::{Mode, ServeOptions, serve},
    clock::Clock,
    llm_client::ModelError,
    session_store::{MemoryStore, RESUMED_LINE, SessionStore},
};
use alyosha_config::{Config, Paths};
use chrono::{DateTime, TimeZone, Utc};
use serde_json::{Value, json};
use support::{ScriptedModel, call, text};
use tokio::io::{
    AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, Lines, ReadHalf, WriteHalf,
};

struct MovableClock(Mutex<DateTime<Utc>>);

impl Clock for MovableClock {
    fn now(&self) -> DateTime<Utc> {
        *self.0.lock().unwrap()
    }
}

struct Client {
    _root: Arc<tempfile::TempDir>,
    paths: Paths,
    store: Arc<MemoryStore>,
    mode: Mode,
    workspace: String,
    projects: std::path::PathBuf,
    writer: WriteHalf<DuplexStream>,
    lines: Lines<BufReader<ReadHalf<DuplexStream>>>,
    next_id: u64,
    /// Every message received, for the validity check.
    seen: Vec<Value>,
    clock: Arc<MovableClock>,
    server: tokio::task::JoinHandle<()>,
}

impl Client {
    async fn start(model: Arc<ScriptedModel>) -> Self {
        Self::start_as(model, Mode::Owner).await
    }

    async fn start_as(model: Arc<ScriptedModel>, mode: Mode) -> Self {
        let root = tempfile::tempdir().unwrap();
        let base = root.path().canonicalize().unwrap();
        let paths = Paths::new(
            &base,
            Some(base.join("config").as_os_str()),
            Some(base.join("data").as_os_str()),
        );
        let workspace = base.join("Projects/ws");
        fs::create_dir_all(&workspace).unwrap();
        fs::write(workspace.join("a.txt"), "alpha").unwrap();
        let clock = Arc::new(MovableClock(Mutex::new(
            Utc.with_ymd_and_hms(2026, 11, 25, 9, 2, 0).unwrap(),
        )));
        let store = MemoryStore::new();
        let (server, writer, read) = spawn_server(&paths, &store, model, mode, &clock);
        Self {
            _root: Arc::new(root),
            paths,
            store,
            mode,
            workspace: workspace.display().to_string(),
            projects: base.join("Projects"),
            writer,
            lines: BufReader::new(read).lines(),
            next_id: 1,
            seen: Vec::new(),
            clock,
            server,
        }
    }

    /// Closes the connection, waits for the process to end, and starts a
    /// new one on the same store, as after a restart.
    async fn reconnect(&mut self, model: Arc<ScriptedModel>) {
        self.writer.shutdown().await.unwrap();
        let (server, writer, read) =
            spawn_server(&self.paths, &self.store, model, self.mode, &self.clock);
        let old = std::mem::replace(&mut self.server, server);
        tokio::time::timeout(Duration::from_secs(10), old)
            .await
            .unwrap()
            .unwrap();
        self.writer = writer;
        self.lines = BufReader::new(read).lines();
        self.next_id = 1;
    }

    async fn send(&mut self, message: Value) {
        self.writer
            .write_all(format!("{message}\n").as_bytes())
            .await
            .unwrap();
    }

    async fn request(&mut self, method: &str, params: Value) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
        id
    }

    async fn next(&mut self) -> Value {
        let line = tokio::time::timeout(Duration::from_secs(10), self.lines.next_line())
            .await
            .expect("a message within 10 seconds")
            .unwrap()
            .expect("the connection is open");
        let message: Value = serde_json::from_str(&line).expect("every line is JSON");
        assert_eq!(message["jsonrpc"], "2.0", "{line}");
        self.seen.push(message.clone());
        message
    }

    /// Messages up to and including the response to `id`.
    async fn until_response(&mut self, id: u64) -> Vec<Value> {
        let mut messages = Vec::new();
        loop {
            let message = self.next().await;
            let done = message["id"] == json!(id) && message.get("method").is_none();
            messages.push(message);
            if done {
                return messages;
            }
        }
    }

    async fn initialize(&mut self, client_name: Option<&str>) -> Value {
        let mut params = json!({"protocolVersion": 1, "clientCapabilities": {}});
        if let Some(name) = client_name {
            params["clientInfo"] = json!({"name": name, "version": "1"});
        }
        let id = self.request("initialize", params).await;
        self.until_response(id).await.pop().unwrap()
    }

    async fn new_session(&mut self) -> String {
        let workspace = self.workspace.clone();
        let id = self
            .request("session/new", json!({"cwd": workspace, "mcpServers": []}))
            .await;
        let response = self.until_response(id).await.pop().unwrap();
        response["result"]["sessionId"]
            .as_str()
            .expect("a session ID")
            .to_owned()
    }

    /// `session/new` under a key, which becomes the session ID.
    async fn new_keyed_session(&mut self, key: &str) -> String {
        let workspace = self.workspace.clone();
        let id = self
            .request(
                "session/new",
                json!({"cwd": workspace, "mcpServers": [], "_meta": {"alyosha": {"sessionKey": key}}}),
            )
            .await;
        let response = self.until_response(id).await.pop().unwrap();
        response["result"]["sessionId"].as_str().unwrap().to_owned()
    }

    async fn prompt(&mut self, session: &str, text: &str) -> u64 {
        self.request(
            "session/prompt",
            json!({"sessionId": session, "prompt": [{"type": "text", "text": text}]}),
        )
        .await
    }
}

fn spawn_server(
    paths: &Paths,
    store: &Arc<MemoryStore>,
    model: Arc<ScriptedModel>,
    mode: Mode,
    clock: &Arc<MovableClock>,
) -> (
    tokio::task::JoinHandle<()>,
    WriteHalf<DuplexStream>,
    ReadHalf<DuplexStream>,
) {
    let (ours, theirs) = tokio::io::duplex(1 << 20);
    let (their_read, their_write) = tokio::io::split(theirs);
    let options = ServeOptions {
        paths: paths.clone(),
        load_config: Arc::new(|| Ok(Config::default())),
        model: Arc::new(move |_: &Config| {
            model.clone() as Arc<dyn alyosha::llm_client::ModelClient>
        }),
        clock: clock.clone(),
        log_file: None,
        store: store.clone() as Arc<dyn SessionStore>,
        mode,
    };
    let server = tokio::spawn(serve(their_read, their_write, options));
    let (read, writer) = tokio::io::split(ours);
    (server, writer, read)
}

fn updates(messages: &[Value]) -> Vec<String> {
    messages
        .iter()
        .filter(|message| message["method"] == "session/update")
        .map(|message| {
            let update = &message["params"]["update"];
            match update["sessionUpdate"].as_str().unwrap() {
                "tool_call" => format!(
                    "tool_call {} {}",
                    update["title"].as_str().unwrap(),
                    update["kind"].as_str().unwrap()
                ),
                "tool_call_update" => {
                    format!("tool_call_update {}", update["status"].as_str().unwrap())
                }
                "agent_message_chunk" => {
                    format!("chunk {}", update["content"]["text"].as_str().unwrap())
                }
                "user_message_chunk" => {
                    format!("user {}", update["content"]["text"].as_str().unwrap())
                }
                other => other.to_owned(),
            }
        })
        .collect()
}

#[tokio::test]
async fn initialize_reports_the_capabilities() {
    let mut client = Client::start(ScriptedModel::new()).await;
    let response = client.initialize(None).await;
    let result = &response["result"];
    assert_eq!(result["protocolVersion"], 1);
    assert_eq!(result["agentCapabilities"]["loadSession"], true);
    assert_eq!(
        result["agentCapabilities"]["promptCapabilities"],
        json!({"image": false, "audio": false, "embeddedContext": false})
    );
    assert_eq!(result["authMethods"], json!([]));
}

#[tokio::test]
async fn new_sessions_need_an_existing_absolute_folder() {
    let mut client = Client::start(ScriptedModel::new()).await;
    client.initialize(None).await;
    for cwd in ["relative/path", "/nonexistent/folder"] {
        let id = client
            .request("session/new", json!({"cwd": cwd, "mcpServers": []}))
            .await;
        let response = client.until_response(id).await.pop().unwrap();
        assert_eq!(response["error"]["code"], -32602, "{cwd}");
    }
    let workspace = client.workspace.clone();
    let id = client
        .request("session/new", json!({"cwd": workspace, "mcpServers": [{"name": "bridge", "command": "evil", "args": [], "env": []}]}))
        .await;
    let messages = client.until_response(id).await;
    assert!(messages.last().unwrap()["result"]["sessionId"].is_string());
    let commands = client.next().await;
    let names: Vec<&str> = commands["params"]["update"]["availableCommands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|command| command["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["compact", "job"]);
    let _ = &client.projects;
}

/// The opening user entry of a request's last turn.
fn last_user_text(request: &alyosha::llm_client::ModelRequest) -> String {
    request
        .history
        .iter()
        .rev()
        .find_map(|entry| match entry {
            alyosha::agent_state::HistoryEntry::User(user) => Some(user.text.clone()),
            _ => None,
        })
        .unwrap()
}

#[tokio::test]
async fn guest_sessions_keep_notes_for_each_sender_only() {
    let model = ScriptedModel::new();
    model.reply(vec![call(
        "call_n",
        "user_memory",
        json!({"notes": "Likes tea."}),
    )]);
    model.reply(vec![text("Noted.")]);
    model.reply(vec![text("Hello Anna.")]);
    model.reply(vec![text("You like tea.")]);
    let mut client = Client::start_as(model.clone(), Mode::Guest).await;
    client.initialize(Some("alyosha-telegram")).await;
    let session = client.new_session().await;
    let commands = client.next().await;
    assert_eq!(
        commands["params"]["update"]["availableCommands"],
        json!([]),
        "guests get no commands"
    );

    // A prompt without a sender is refused.
    let id = client.prompt(&session, "hi").await;
    let response = client.until_response(id).await.pop().unwrap();
    assert_eq!(response["error"]["code"], -32602);
    for bad in ["../x", "telegram"] {
        let id = client
            .request("session/prompt", json!({"sessionId": session, "prompt": [{"type": "text", "text": "hi"}], "_meta": {"alyosha": {"sender": {"id": bad, "name": "X"}}}}))
            .await;
        let response = client.until_response(id).await.pop().unwrap();
        assert_eq!(response["error"]["code"], -32602, "{bad}");
    }

    let from = |id: &str, name: &str, text: &str| json!({"sessionId": session, "prompt": [{"type": "text", "text": text}], "_meta": {"alyosha": {"sender": {"id": id, "name": name}}}});
    // Commands are plain text here.
    let id = client
        .request(
            "session/prompt",
            from("telegram-1", "Bob", "/job ls; I like tea"),
        )
        .await;
    let messages = client.until_response(id).await;
    assert_eq!(messages.last().unwrap()["result"]["stopReason"], "end_turn");
    let id = client
        .request("session/prompt", from("telegram-2", "Anna", "hi"))
        .await;
    client.until_response(id).await;
    let id = client
        .request(
            "session/prompt",
            from("telegram-1", "Bob", "what do I like?"),
        )
        .await;
    client.until_response(id).await;

    let requests = model.requests();
    let tools: Vec<&str> = requests[0]
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect();
    assert_eq!(tools, ["user_memory"]);
    let instruction = &requests[0].system_instruction;
    assert!(!instruction.contains("## Memory"), "{instruction}");
    assert!(
        instruction
            .trim_end()
            .ends_with("People write to you in a group chat through alyosha-telegram."),
        "{instruction}"
    );
    let first = last_user_text(&requests[0]);
    assert!(
        first.ends_with(
            "Notes on this person (telegram-1): No notes yet.\n\nBob: /job ls; I like tea"
        ),
        "{first}"
    );
    let anna = last_user_text(&requests[2]);
    assert!(
        anna.ends_with("Notes on this person (telegram-2): No notes yet.\n\nAnna: hi"),
        "{anna}"
    );
    let again = last_user_text(&requests[3]);
    assert!(
        again.contains("Likes tea.\n\nBob: what do I like?"),
        "{again}"
    );
    let users = client
        ._root
        .path()
        .canonicalize()
        .unwrap()
        .join("data/alyosha/memory/users");
    let bob = fs::read_to_string(users.join("telegram-1.md")).unwrap();
    assert!(
        bob.starts_with("---\nname: Bob\n") && bob.ends_with("Likes tea."),
        "{bob}"
    );
    assert!(!users.join("telegram-2.md").exists());
}

#[tokio::test]
async fn a_turn_reports_tool_calls_then_the_reply() {
    let model = ScriptedModel::new();
    model.reply(vec![
        call("call_a", "read_file", json!({"path": "a.txt"})),
        call("call_b", "read_file", json!({"path": "b.txt"})),
    ]);
    model.reply(vec![text("a.txt contains alpha. b.txt was missing.")]);
    let mut client = Client::start(model.clone()).await;
    client.initialize(Some("alyosha-telegram")).await;
    let session = client.new_session().await;
    client.next().await; // available_commands_update
    let id = client.prompt(&session, "Read a.txt and b.txt.").await;
    let messages = client.until_response(id).await;
    // A startup warning, such as a missing sandbox here, comes before the reply.
    let shown: Vec<String> = updates(&messages)
        .into_iter()
        .filter(|update| !update.contains("sandbox is unavailable"))
        .collect();
    assert_eq!(
        shown,
        [
            "tool_call read_file a.txt read",
            "tool_call_update completed",
            "tool_call read_file b.txt read",
            "tool_call_update failed",
            "chunk a.txt contains alpha. b.txt was missing.",
        ]
    );
    assert_eq!(messages.last().unwrap()["result"]["stopReason"], "end_turn");
    let instruction = &model.requests()[0].system_instruction;
    assert!(
        instruction
            .trim_end()
            .ends_with("The owner is talking to you through alyosha-telegram."),
        "{instruction}"
    );
}

#[tokio::test]
async fn prompts_sent_together_are_answered_in_order() {
    let model = ScriptedModel::new();
    model.reply(vec![text("first reply")]);
    model.reply(vec![text("second reply")]);
    let mut client = Client::start(model.clone()).await;
    client.initialize(None).await;
    let session = client.new_session().await;
    let first = client.prompt(&session, "one").await;
    let second = client.prompt(&session, "two").await;
    let messages = client.until_response(second).await;
    let first_response = messages
        .iter()
        .position(|message| message["id"] == json!(first) && message.get("result").is_some())
        .unwrap();
    let second_response = messages.len() - 1;
    assert!(first_response < second_response);
    assert!(updates(&messages).contains(&"chunk first reply".to_owned()));
    assert!(updates(&messages).contains(&"chunk second reply".to_owned()));
    assert!(
        model.requests()[0]
            .system_instruction
            .ends_with("No skills installed.\n"),
        "no channel line without clientInfo"
    );
}

#[tokio::test]
async fn cancel_ends_the_prompt_after_its_updates() {
    let model = ScriptedModel::new();
    let gate = model.reply_gated(vec![call("c", "read_file", json!({"path": "a.txt"}))]);
    let mut client = Client::start(model).await;
    client.initialize(None).await;
    let session = client.new_session().await;
    let id = client.prompt(&session, "Read it.").await;
    gate.started().await;
    client
        .send(
            json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": session}}),
        )
        .await;
    let messages = client.until_response(id).await;
    assert_eq!(
        messages.last().unwrap()["result"]["stopReason"],
        "cancelled"
    );
}

#[tokio::test]
async fn compact_during_a_turn_is_busy_and_a_failure_is_an_error() {
    let model = ScriptedModel::new();
    let gate = model.reply_gated(vec![text("slow")]);
    model.fail(ModelError::Timeout("no reply".into()));
    model.reply(vec![text("fine again")]);
    let mut client = Client::start(model).await;
    client.initialize(None).await;
    let session = client.new_session().await;
    let slow = client.prompt(&session, "slow one").await;
    gate.started().await;
    let compact = client.prompt(&session, "/compact").await;
    let messages = client.until_response(compact).await;
    assert!(
        updates(&messages)
            .iter()
            .any(|update| update.contains("try /compact again"))
    );
    gate.release();
    client.until_response(slow).await;

    let failing = client.prompt(&session, "fail").await;
    let response = client.until_response(failing).await.pop().unwrap();
    assert!(
        response["error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("timeout")
    );
    let next = client.prompt(&session, "again").await;
    let messages = client.until_response(next).await;
    assert_eq!(messages.last().unwrap()["result"]["stopReason"], "end_turn");
}

#[tokio::test]
async fn a_new_session_cancels_waiting_prompts() {
    let model = ScriptedModel::new();
    let gate = model.reply_gated(vec![text("never sent")]);
    let mut client = Client::start(model).await;
    client.initialize(None).await;
    let session = client.new_session().await;
    let running = client.prompt(&session, "first").await;
    let waiting = client.prompt(&session, "second").await;
    gate.started().await;
    let workspace = client.workspace.clone();
    let renewed = client
        .request("session/new", json!({"cwd": workspace, "mcpServers": []}))
        .await;
    let messages = client.until_response(renewed).await;
    let cancelled = |id: u64| {
        messages.iter().any(|message| {
            message["id"] == json!(id) && message["result"]["stopReason"] == "cancelled"
        })
    };
    assert!(cancelled(running) && cancelled(waiting));
    assert_ne!(
        messages.last().unwrap()["result"]["sessionId"],
        json!(session)
    );
}

#[tokio::test]
async fn job_prompts_answer_at_once() {
    let mut client = Client::start(ScriptedModel::new()).await;
    client.initialize(None).await;
    let session = client.new_session().await;
    let id = client.prompt(&session, "/job").await;
    let messages = client.until_response(id).await;
    assert!(
        updates(&messages)
            .iter()
            .any(|update| update.contains("/job <command>"))
    );
    assert_eq!(messages.last().unwrap()["result"]["stopReason"], "end_turn");
}

#[tokio::test]
async fn load_of_an_unknown_key_is_not_found() {
    let mut client = Client::start(ScriptedModel::new()).await;
    client.initialize(None).await;
    let workspace = client.workspace.clone();
    let id = client
        .request(
            "session/load",
            json!({"sessionId": "owner:telegram", "cwd": workspace, "mcpServers": []}),
        )
        .await;
    let response = client.until_response(id).await.pop().unwrap();
    assert_eq!(response["error"]["code"], -32002);
}

#[tokio::test]
async fn a_restarted_process_loads_the_session_by_its_key() {
    let model = ScriptedModel::new();
    model.reply(vec![text("Noted: blue.")]);
    model.reply(vec![text("Noted: Tuesday.")]);
    let mut client = Client::start(model).await;
    client.initialize(None).await;
    let session = client.new_keyed_session("owner:telegram").await;
    assert_eq!(session, "owner:telegram");
    for message in ["my colour is blue", "my day is Tuesday"] {
        let id = client.prompt(&session, message).await;
        client.until_response(id).await;
    }

    let model = ScriptedModel::new();
    model.reply(vec![text("Blue, and Tuesday.")]);
    client.reconnect(model.clone()).await;
    client.initialize(None).await;
    let workspace = client.workspace.clone();
    let id = client
        .request(
            "session/load",
            json!({"sessionId": "owner:telegram", "cwd": workspace, "mcpServers": []}),
        )
        .await;
    let messages = client.until_response(id).await;
    assert_eq!(
        updates(&messages),
        [
            "user my colour is blue",
            "chunk Noted: blue.",
            "user my day is Tuesday",
            "chunk Noted: Tuesday.",
        ],
        "the conversation is replayed without clock lines"
    );
    assert_eq!(messages.last().unwrap()["result"], json!({}));
    let commands = client.next().await;
    assert_eq!(
        commands["params"]["update"]["sessionUpdate"],
        "available_commands_update"
    );

    let id = client
        .prompt("owner:telegram", "what did I tell you?")
        .await;
    let messages = client.until_response(id).await;
    let chunks = updates(&messages);
    // Other startup warnings, such as a missing sandbox, may come between.
    assert_eq!(chunks.first().unwrap(), &format!("chunk {RESUMED_LINE}"));
    assert_eq!(chunks.last().unwrap(), "chunk Blue, and Tuesday.");
    let request = model.requests().pop().unwrap();
    assert_eq!(
        request.history.len(),
        5,
        "the earlier turns and the new message"
    );
}

#[tokio::test]
async fn closing_input_ends_the_process_and_every_line_is_json_rpc() {
    let mut client = Client::start(ScriptedModel::new()).await;
    client.initialize(None).await;
    client.new_session().await;
    client.writer.shutdown().await.unwrap();
    drop(client.writer);
    tokio::time::timeout(Duration::from_secs(10), client.server)
        .await
        .unwrap()
        .unwrap();
    for message in &client.seen {
        assert!(
            message.get("method").is_some()
                || message.get("result").is_some()
                || message.get("error").is_some()
        );
    }
}

#[tokio::test]
async fn trusted_group_sessions_are_full_sessions_with_their_own_channel_line() {
    let model = ScriptedModel::new();
    model.reply(vec![call("call_r", "read_file", json!({"path": "a.txt"}))]);
    model.reply(vec![text("It says alpha.")]);
    let mut client = Client::start_as(model.clone(), Mode::Group).await;
    client.initialize(Some("alyosha-qq")).await;
    let session = client.new_session().await;
    let commands = client.next().await;
    let names: Vec<&str> = commands["params"]["update"]["availableCommands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|command| command["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["compact", "job"]);
    // No sender in `_meta` is needed: the bridge names the sender in the text.
    let id = client.prompt(&session, "Bob: what is in a.txt?").await;
    let messages = client.until_response(id).await;
    assert_eq!(messages.last().unwrap()["result"]["stopReason"], "end_turn");
    let requests = model.requests();
    assert!(
        requests[0]
            .tools
            .iter()
            .any(|tool| tool.name == "read_file"),
        "a trusted group has the owner's tools"
    );
    assert!(last_user_text(&requests[0]).ends_with("Bob: what is in a.txt?"));
    let instruction = &requests[0].system_instruction;
    assert!(
        instruction.trim_end().ends_with(
            "People in a group chat your owner trusts write to you through alyosha-qq. Each message starts with the sender's display name."
        ),
        "{instruction}"
    );
}
