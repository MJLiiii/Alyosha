//! `alyosha acp`: one Agent Client Protocol connection over standard input
//! and output, driving the same session driver as the TUI.
//!
//! Standard output carries only protocol messages: newline-delimited
//! JSON-RPC 2.0, protocol version 1. Sessions are saved in the session store
//! under the key the client names, and `session/load` resumes them. See
//! `docs/develop/Channels.md` and `docs/develop/SessionStore.md#acp`.

use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
};

use alyosha_config::{Config, Paths};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
    sync::{mpsc, oneshot},
};
use tokio_util::sync::CancellationToken;

use crate::{
    BoxFuture,
    agent_loop::{JobDelivery, Persistence, SessionDriver, SessionEvent, Status},
    agent_state::{HistoryEntry, MessageId, Sender, TurnOutcome},
    approvals::{Approver, OwnerAnswer, OwnerRequest},
    clock::Clock,
    llm_client::ModelClient,
    session::{self, FrontEnd},
    session_store::{PgStore, SessionStore, StoreError},
    user_notes::valid_user_id,
};

pub const PROTOCOL_VERSION: u64 = 1;
/// The JSON-RPC error for `session/load` of a key with no session.
pub const RESOURCE_NOT_FOUND: i64 = -32002;

/// Makes the model client for a session from its configuration.
pub type ModelFactory = Arc<dyn Fn(&Config) -> Arc<dyn ModelClient> + Send + Sync>;

/// What `serve` needs besides the connection.
pub struct ServeOptions {
    pub paths: Paths,
    /// Reads `config.toml`; called at each session start.
    pub load_config: Arc<dyn Fn() -> Result<Config, String> + Send + Sync>,
    /// The client for a session.
    pub model: ModelFactory,
    pub clock: Arc<dyn Clock>,
    /// The log file that startup warnings name.
    pub log_file: Option<PathBuf>,
    /// Where sessions are saved and found again.
    pub store: Arc<dyn SessionStore>,
    /// Whose session this process serves.
    pub mode: Mode,
}

/// Whose session `alyosha acp` serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// `alyosha acp`: the owner's session.
    Owner,
    /// `alyosha acp --guest`: an untrusted group chat's guest session.
    /// See `docs/develop/Channels.md#guest-mode`.
    Guest,
    /// `alyosha acp --group`: a trusted group chat's full session.
    /// See `docs/develop/Channels.md#group-mode`.
    Group,
}

/// The connection's writer: one line per message, in order.
#[derive(Clone)]
struct Out {
    lines: mpsc::UnboundedSender<String>,
    next_id: Arc<AtomicU64>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
}

impl Out {
    fn send(&self, message: Value) {
        let _ = self.lines.send(message.to_string());
    }

    fn respond(&self, id: &Value, result: Value) {
        self.send(json!({"jsonrpc": "2.0", "id": id, "result": result}));
    }

    fn error(&self, id: &Value, code: i64, message: &str) {
        self.send(json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}));
    }

    fn notify(&self, method: &str, params: Value) {
        self.send(json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    fn update(&self, session_id: &str, update: Value) {
        self.notify(
            "session/update",
            json!({"sessionId": session_id, "update": update}),
        );
    }

    /// Sends a request to the client and returns the waiter for its result.
    fn request(&self, method: &str, params: Value) -> oneshot::Receiver<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id, sender);
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        receiver
    }
}

/// Asks through `session/request_permission`.
struct AcpApprover {
    out: Out,
    session_id: Arc<Mutex<String>>,
    open_requests: Arc<AtomicU64>,
}

impl Approver for AcpApprover {
    fn ask(&self, request: OwnerRequest, cancel: CancellationToken) -> BoxFuture<'_, OwnerAnswer> {
        Box::pin(async move {
            let session_id = self
                .session_id
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            let last = request.options.len().saturating_sub(1);
            let options: Vec<Value> = request
                .options
                .iter()
                .enumerate()
                .map(|(index, name)| {
                    json!({
                        "optionId": index.to_string(),
                        "name": name,
                        "kind": if index == last { "reject_once" } else { "allow_once" },
                    })
                })
                .collect();
            let mut tool_call = json!({"toolCallId": request.call_id, "title": request.title});
            if !request.body.is_empty() {
                tool_call["content"] =
                    json!([{"type": "content", "content": {"type": "text", "text": request.body}}]);
            }
            let reply = self.out.request(
                "session/request_permission",
                json!({"sessionId": session_id, "toolCall": tool_call, "options": options}),
            );
            self.open_requests.fetch_add(1, Ordering::SeqCst);
            // Dropped by the time limit: tell the client the request expired.
            struct Expiry<'a> {
                approver: &'a AcpApprover,
                session_id: String,
                call_id: String,
                cancel: CancellationToken,
                answered: bool,
            }
            impl Drop for Expiry<'_> {
                fn drop(&mut self) {
                    self.approver.open_requests.fetch_sub(1, Ordering::SeqCst);
                    if !self.answered && !self.cancel.is_cancelled() {
                        self.approver.out.update(
                            &self.session_id,
                            json!({"sessionUpdate": "tool_call_update", "toolCallId": self.call_id, "status": "failed"}),
                        );
                    }
                }
            }
            let mut expiry = Expiry {
                approver: self,
                session_id,
                call_id: request.call_id.clone(),
                cancel: cancel.clone(),
                answered: false,
            };
            let answer = tokio::select! {
                reply = reply => reply.ok(),
                () = cancel.cancelled() => None,
            };
            expiry.answered = true;
            if cancel.is_cancelled() {
                return OwnerAnswer::Cancelled;
            }
            let Some(reply) = answer else {
                return OwnerAnswer::Undeliverable;
            };
            match reply["outcome"]["outcome"].as_str() {
                Some("selected") => reply["outcome"]["optionId"]
                    .as_str()
                    .and_then(|id| id.parse().ok())
                    .map_or(OwnerAnswer::Undeliverable, OwnerAnswer::Chosen),
                _ => OwnerAnswer::Undeliverable,
            }
        })
    }
}

/// What a prompt's JSON-RPC request is waiting for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Waiting {
    Turn(MessageId),
    Compact,
}

struct Open {
    session_id: String,
    driver: SessionDriver,
    first_reply_sent: bool,
    warnings: Vec<String>,
    prompts: VecDeque<(Value, Waiting)>,
    status_counter: u64,
    compacting_status: Option<String>,
}

struct Agent {
    options: ServeOptions,
    out: Out,
    client_name: Mutex<Option<String>>,
    session: tokio::sync::Mutex<Option<Open>>,
    session_id: Arc<Mutex<String>>,
    open_requests: Arc<AtomicU64>,
}

/// Serves one connection until the input closes.
pub async fn serve<R, W>(input: R, output: W, options: ServeOptions)
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (lines, mut outgoing) = mpsc::unbounded_channel::<String>();
    let writer = tokio::spawn(async move {
        let mut output = output;
        while let Some(line) = outgoing.recv().await {
            if output
                .write_all(format!("{line}\n").as_bytes())
                .await
                .is_err()
            {
                break;
            }
            let _ = output.flush().await;
        }
    });
    let agent = Arc::new(Agent {
        options,
        out: Out {
            lines,
            next_id: Arc::new(AtomicU64::new(1)),
            pending: Arc::default(),
        },
        client_name: Mutex::new(None),
        session: tokio::sync::Mutex::new(None),
        session_id: Arc::default(),
        open_requests: Arc::default(),
    });
    let mut reader = BufReader::new(input).lines();
    while let Ok(Some(line)) = reader.next_line().await {
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let agent = agent.clone();
        // Requests are handled concurrently: a long session/new does not
        // block a cancel, and a prompt stays open until its turn closes.
        tokio::spawn(async move { agent.handle(message).await });
    }
    agent.end_session().await;
    drop(agent);
    let _ = writer.await;
}

impl Agent {
    async fn handle(self: Arc<Self>, message: Value) {
        let id = message.get("id").cloned();
        let method = message["method"].as_str().map(str::to_owned);
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        match (method.as_deref(), id) {
            // A response to one of our requests.
            (None, Some(id)) => {
                if let Some(id) = id.as_u64() {
                    let waiter = self
                        .out
                        .pending
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .remove(&id);
                    if let Some(waiter) = waiter {
                        let _ = waiter.send(message.get("result").cloned().unwrap_or(Value::Null));
                    }
                }
            }
            (Some("initialize"), Some(id)) => {
                if let Some(name) = params["clientInfo"]["name"].as_str() {
                    *self
                        .client_name
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner) = Some(name.to_owned());
                }
                self.out.respond(
                    &id,
                    json!({
                        "protocolVersion": PROTOCOL_VERSION,
                        "agentCapabilities": {
                            "loadSession": true,
                            "promptCapabilities": {"image": false, "audio": false, "embeddedContext": false},
                            "mcpCapabilities": {"http": false, "sse": false},
                        },
                        "authMethods": [],
                        "agentInfo": {"name": "alyosha", "version": env!("CARGO_PKG_VERSION")},
                    }),
                );
            }
            (Some("session/new"), Some(id)) => self.new_session(id, &params).await,
            (Some("session/load"), Some(id)) => self.load_session(id, &params).await,
            (Some("session/prompt"), Some(id)) => self.prompt(id, &params).await,
            (Some("session/cancel"), None) => {
                if let Some(open) = self.session.lock().await.as_ref()
                    && params["sessionId"].as_str() == Some(&open.session_id)
                {
                    open.driver.stop();
                }
            }
            (Some(other), Some(id)) => {
                self.out
                    .error(&id, -32601, &format!("method not found: {other}"))
            }
            _ => {}
        }
    }

    fn channel_line(&self) -> Option<String> {
        self.client_name
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .map(|name| {
                match self.options.mode {
                    Mode::Owner => format!("The owner is talking to you through {name}."),
                    Mode::Guest => format!("People write to you in a group chat through {name}."),
                    Mode::Group => format!(
                        "People in a group chat your owner trusts write to you through {name}. Each message starts with the sender's display name."
                    ),
                }
            })
    }

    /// Checks `cwd` and answers the request with an error if it is unusable.
    fn workspace(&self, id: &Value, params: &Value) -> Option<PathBuf> {
        let Some(cwd) = params["cwd"]
            .as_str()
            .map(PathBuf::from)
            .filter(|cwd| cwd.is_absolute())
        else {
            self.out.error(id, -32602, "cwd must be an absolute path");
            return None;
        };
        match session::resolve_workspace(&cwd) {
            Ok(workspace) => Some(workspace),
            Err(error) => {
                self.out.error(id, -32602, &error.to_string());
                None
            }
        }
    }

    /// `session/new`: a new session under the key in
    /// `_meta.alyosha.sessionKey`, which becomes the `sessionId`.
    async fn new_session(self: &Arc<Self>, id: Value, params: &Value) {
        let Some(workspace) = self.workspace(&id, params) else {
            return;
        };
        let key = params["_meta"]["alyosha"]["sessionKey"]
            .as_str()
            .filter(|key| !key.is_empty())
            .map_or_else(
                || format!("acp:{}", crate::environment::random_suffix()),
                str::to_owned,
            );
        self.end_session().await;
        let stored = match self.options.store.create(&key, &workspace).await {
            Ok(stored) => stored,
            Err(error) => {
                tracing::warn!(key = %key, error = %error, "the session store refused the session");
                self.out.error(&id, -32603, &error.to_string());
                return;
            }
        };
        let persistence = Persistence {
            store: self.options.store.clone(),
            stored,
            resumed: false,
        };
        match self.open(&workspace, &key, persistence).await {
            Ok(()) => {
                self.out.respond(&id, json!({"sessionId": key}));
                self.available_commands(&key);
            }
            Err(error) => self.out.error(&id, -32603, &error),
        }
    }

    /// `session/load`: resumes the current session of the key given as the
    /// `sessionId`, replaying its conversation before the response.
    async fn load_session(self: &Arc<Self>, id: Value, params: &Value) {
        let Some(key) = params["sessionId"].as_str().filter(|key| !key.is_empty()) else {
            self.out.error(&id, -32602, "sessionId is required");
            return;
        };
        let Some(workspace) = self.workspace(&id, params) else {
            return;
        };
        self.end_session().await;
        let stored = match self.options.store.load_current(key).await {
            Ok(Some(stored)) => stored,
            Ok(None) => {
                self.out
                    .error(&id, RESOURCE_NOT_FOUND, "Resource not found");
                return;
            }
            Err(error) => {
                tracing::warn!(key = %key, error = %error, "the session store refused the session");
                self.out.error(&id, -32603, &error.to_string());
                return;
            }
        };
        let persistence = Persistence {
            store: self.options.store.clone(),
            stored,
            resumed: true,
        };
        if let Err(error) = self.open(&workspace, key, persistence).await {
            self.out.error(&id, -32603, &error);
            return;
        }
        let history = match self.session.lock().await.as_ref() {
            Some(open) => open.driver.snapshot().history,
            None => Vec::new(),
        };
        for entry in &history {
            match entry {
                HistoryEntry::User(user) => {
                    // The clock line is for the model only.
                    let text = user.text.split_once('\n').map_or("", |(_, rest)| rest);
                    self.out.update(
                        key,
                        json!({"sessionUpdate": "user_message_chunk", "content": {"type": "text", "text": text}}),
                    );
                }
                HistoryEntry::Assistant(reply) => {
                    let text = reply.text();
                    if !text.is_empty() {
                        self.out.update(key, chunk(&text));
                    }
                }
                HistoryEntry::ToolResult(_) => {}
            }
        }
        self.out.respond(&id, json!({}));
        self.available_commands(key);
    }

    fn available_commands(&self, session_id: &str) {
        if self.options.mode == Mode::Guest {
            self.out.update(
                session_id,
                json!({"sessionUpdate": "available_commands_update", "availableCommands": []}),
            );
            return;
        }
        self.out.update(
            session_id,
            json!({"sessionUpdate": "available_commands_update", "availableCommands": [
                {"name": "compact", "description": "Summarize older conversation to free context."},
                {"name": "job", "description": "Run a shell command as a background job", "input": {"hint": "command"}},
            ]}),
        );
    }

    /// Starts a session and its event pump.
    fn open<'a>(
        self: &'a Arc<Self>,
        workspace: &'a std::path::Path,
        session_id: &'a str,
        persistence: Persistence,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(self.open_inner(workspace, session_id, persistence))
    }

    async fn open_inner(
        self: &Arc<Self>,
        workspace: &std::path::Path,
        session_id: &str,
        persistence: Persistence,
    ) -> Result<(), String> {
        let store = persistence.store.clone();
        let stored_id = persistence.stored.id;
        let config = match (self.options.load_config)() {
            Ok(config) => config,
            Err(error) => {
                let _ = store.close(stored_id).await;
                return Err(error);
            }
        };
        *self
            .session_id
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = session_id.to_owned();
        let started = if self.options.mode == Mode::Guest {
            session::start_guest(
                &config,
                &self.options.paths,
                (self.options.model)(&config),
                self.channel_line(),
                self.options.clock.clone(),
                Some(persistence),
            )
            .await
            .map(|mut started| {
                // Startup warnings stay in the log, never in a group.
                started.warnings.clear();
                started
            })
            .map_err(|error| error.to_string())
        } else {
            self.open_owner(&config, workspace, persistence).await
        };
        let started = match started {
            Ok(started) => started,
            Err(error) => {
                let _ = store.close(stored_id).await;
                return Err(error);
            }
        };
        *self.session.lock().await = Some(Open {
            session_id: session_id.to_owned(),
            driver: started.driver,
            first_reply_sent: false,
            warnings: started.warnings,
            prompts: VecDeque::new(),
            status_counter: 0,
            compacting_status: None,
        });
        let agent = self.clone();
        let mut events = started.events;
        tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                agent.on_event(event).await;
            }
        });
        Ok(())
    }

    async fn open_owner(
        self: &Arc<Self>,
        config: &Config,
        workspace: &std::path::Path,
        persistence: Persistence,
    ) -> Result<session::Started, String> {
        session::start_with_clock(
            config,
            &self.options.paths,
            workspace,
            (self.options.model)(config),
            FrontEnd {
                approver: Some(Arc::new(AcpApprover {
                    out: self.out.clone(),
                    session_id: self.session_id.clone(),
                    open_requests: self.open_requests.clone(),
                })),
                job_delivery: JobDelivery::Report,
                channel_line: self.channel_line(),
                log_file: self.options.log_file.clone(),
                on_loading_dev_shell: None,
                persistence: Some(persistence),
            },
            self.options.clock.clone(),
        )
        .await
        .map_err(|error| error.to_string())
    }

    /// Ends the open session: queued prompts end cancelled, MCP servers
    /// stop, the session's logs are removed, and its lock is released.
    async fn end_session(&self) {
        let open = self.session.lock().await.take();
        if let Some(mut open) = open {
            open.driver.end().await;
            for (id, _) in open.prompts.drain(..) {
                self.out.respond(&id, json!({"stopReason": "cancelled"}));
            }
        }
    }

    async fn prompt(self: &Arc<Self>, id: Value, params: &Value) {
        let mut guard = self.session.lock().await;
        let Some(open) = guard
            .as_mut()
            .filter(|open| params["sessionId"].as_str() == Some(&open.session_id))
        else {
            self.out.error(&id, -32602, "unknown sessionId");
            return;
        };
        let mut parts = Vec::new();
        for block in params["prompt"].as_array().into_iter().flatten() {
            match block["type"].as_str() {
                Some("text") => parts.push(block["text"].as_str().unwrap_or_default().to_owned()),
                Some("resource_link") => parts.push(format!(
                    "{} {}",
                    block["name"].as_str().unwrap_or_default(),
                    block["uri"].as_str().unwrap_or_default()
                )),
                _ => {}
            }
        }
        let text = parts.join("\n\n");
        if self.options.mode == Mode::Guest {
            // A group member's message: never a command, always with its sender.
            let sender = &params["_meta"]["alyosha"]["sender"];
            let id_text = sender["id"].as_str().unwrap_or_default();
            if !valid_user_id(id_text) {
                self.out.error(
                    &id,
                    -32602,
                    "a guest prompt needs _meta.alyosha.sender with a valid id",
                );
                return;
            }
            let sender = Sender {
                id: id_text.to_owned(),
                name: sender["name"].as_str().unwrap_or_default().to_owned(),
            };
            let message_id = open.driver.submit_from(text, sender);
            open.prompts.push_back((id, Waiting::Turn(message_id)));
            return;
        }
        let trimmed = text.trim();
        if trimmed == "/compact" {
            open.prompts.push_back((id, Waiting::Compact));
            open.driver.compact();
        } else if trimmed == "/job" || trimmed.starts_with("/job ") || trimmed.starts_with("/job\n")
        {
            let status = open
                .driver
                .start_job(trimmed.strip_prefix("/job").unwrap_or_default());
            let session_id = open.session_id.clone();
            drop(guard);
            self.out.update(&session_id, chunk(&status));
            self.out.respond(&id, json!({"stopReason": "end_turn"}));
        } else {
            // Submit under the lock, so the ID is mapped before its turn ends.
            let message_id = open.driver.submit(text);
            open.prompts.push_back((id, Waiting::Turn(message_id)));
        }
    }

    async fn on_event(&self, event: SessionEvent) {
        let mut guard = self.session.lock().await;
        let Some(open) = guard.as_mut() else { return };
        let session_id = open.session_id.clone();
        match event {
            SessionEvent::ToolStarted { call_id, tool, title } => {
                let kind = match tool.as_str() {
                    "read_file" => "read",
                    "write_file" | "edit_file" => "edit",
                    "run_command" => "execute",
                    _ => "other",
                };
                self.out.update(
                    &session_id,
                    json!({"sessionUpdate": "tool_call", "toolCallId": call_id.0, "title": title, "kind": kind, "status": "in_progress"}),
                );
            }
            SessionEvent::ToolFinished { call_id, ok } => self.out.update(
                &session_id,
                json!({"sessionUpdate": "tool_call_update", "toolCallId": call_id.0, "status": if ok { "completed" } else { "failed" }}),
            ),
            SessionEvent::Notice(text) => {
                open.status_counter += 1;
                let id = format!("alyosha-status-{}", open.status_counter);
                self.out.update(
                    &session_id,
                    json!({"sessionUpdate": "tool_call", "toolCallId": id, "title": text, "kind": "think", "status": "in_progress"}),
                );
                if text == "Compacting context…" {
                    open.compacting_status = Some(id);
                } else {
                    self.out.update(&session_id, json!({"sessionUpdate": "tool_call_update", "toolCallId": id, "status": "completed"}));
                }
            }
            SessionEvent::Status(status) => {
                if status != Status::Compacting
                    && let Some(id) = open.compacting_status.take()
                {
                    self.out.update(&session_id, json!({"sessionUpdate": "tool_call_update", "toolCallId": id, "status": "completed"}));
                }
            }
            SessionEvent::AssistantText(text) | SessionEvent::Reply { text, .. } => {
                self.send_warnings(open);
                self.out.update(&session_id, chunk(&text));
            }
            SessionEvent::TurnEnded { turn_id, outcome } => {
                if let Some(id) = open.compacting_status.take() {
                    self.out.update(&session_id, json!({"sessionUpdate": "tool_call_update", "toolCallId": id, "status": "failed"}));
                }
                let Some(position) = open.prompts.iter().position(|(_, waiting)| *waiting == Waiting::Turn(turn_id)) else {
                    return;
                };
                let (id, _) = open.prompts.remove(position).expect("found above");
                match outcome {
                    TurnOutcome::Completed => {
                        self.send_warnings(open);
                        self.out.respond(&id, json!({"stopReason": "end_turn"}));
                    }
                    TurnOutcome::Stopped => self.out.respond(&id, json!({"stopReason": "cancelled"})),
                    TurnOutcome::Failed(reason) => self.out.error(&id, -32603, &reason),
                }
            }
            SessionEvent::Discarded(message_id) => {
                if let Some(position) = open.prompts.iter().position(|(_, waiting)| *waiting == Waiting::Turn(message_id)) {
                    let (id, _) = open.prompts.remove(position).expect("found above");
                    self.out.respond(&id, json!({"stopReason": "cancelled"}));
                }
            }
            SessionEvent::CommandStatus(status) => {
                if let Some(position) = open.prompts.iter().position(|(_, waiting)| *waiting == Waiting::Compact) {
                    let (id, _) = open.prompts.remove(position).expect("found above");
                    self.out.update(&session_id, chunk(&status));
                    self.out.respond(&id, json!({"stopReason": "end_turn"}));
                }
            }
            SessionEvent::JobFinished { job_id, text } => self.out.notify(
                "_alyosha/job_finished",
                json!({"sessionId": session_id, "jobId": job_id, "text": text}),
            ),
            SessionEvent::Queued(_)
            | SessionEvent::JobMessageQueued { .. }
            | SessionEvent::TurnStarted(_)
            | SessionEvent::Jobs(_) => {}
        }
    }

    /// Startup warnings go before the first reply, each as its own chunk.
    fn send_warnings(&self, open: &mut Open) {
        if open.first_reply_sent {
            return;
        }
        open.first_reply_sent = true;
        for warning in open.warnings.drain(..) {
            self.out.update(&open.session_id, chunk(&warning));
        }
    }
}

fn chunk(text: &str) -> Value {
    json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}})
}

/// Runs `alyosha acp`, or `alyosha acp --guest` or `--group` for a group
/// chat, on standard input and output. Returns the exit status.
pub fn run(mode: Mode) -> u8 {
    let paths = match Paths::from_env() {
        Ok(paths) => paths,
        Err(error) => {
            eprintln!("alyosha acp: {error}");
            return 2;
        }
    };
    let config = match alyosha_config::load_from(&paths.config_file()) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("alyosha acp: {error}");
            return 2;
        }
    };
    let program = match mode {
        Mode::Owner => "alyosha-acp",
        Mode::Guest => "alyosha-acp-guest",
        Mode::Group => "alyosha-acp-group",
    };
    let log_file = alyosha_config::logging::init(program, config.timezone, &paths).ok();
    let runtime = tokio::runtime::Runtime::new().expect("a Tokio runtime starts");
    let store = match runtime.block_on(PgStore::connect_default()) {
        Ok(store) => store,
        Err(error) => {
            tracing::error!(error = %error, "could not reach the session database");
            eprintln!("alyosha acp: {}", database_error(&error));
            return 1;
        }
    };
    let config_paths = paths.clone();
    let options = ServeOptions {
        paths: paths.clone(),
        load_config: Arc::new(move || {
            alyosha_config::load_from(&config_paths.config_file())
                .map_err(|error| error.to_string())
        }),
        model: {
            let paths = paths.clone();
            Arc::new(move |config: &Config| session::model_client(config, &paths))
        },
        clock: Arc::new(crate::clock::SystemClock),
        log_file,
        store,
        mode,
    };
    runtime.block_on(serve(tokio::io::stdin(), tokio::io::stdout(), options));
    0
}

/// The message for a session database that cannot be reached.
pub fn database_error(error: &StoreError) -> String {
    let target = std::env::var("ALYOSHA_DATABASE").unwrap_or_else(|_| "/run/postgresql".to_owned());
    format!(
        "Could not reach the session database at {target}. Check `systemctl status postgresql`. ({error})"
    )
}
