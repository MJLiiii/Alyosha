//! The ACP side shared by Alyosha's chat bridges: starting `alyosha acp`,
//! `initialize`, opening a session by key with `session/load` or
//! `session/new`, sending prompts, attributing updates to
//! the oldest open prompt, collecting each prompt's tool calls, text, and
//! outcome, and the scheduled-task runner.
//!
//! No bridge depends on Alyosha's library; this crate speaks ACP's
//! newline-delimited JSON-RPC to the child directly. See
//! `docs/TelegramBridge.md` and `docs/Schedule.md#the-runner`.

pub mod attachments;
pub mod guests;
pub mod markdown;
pub mod runner;

use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
    process::{Child, Command},
    sync::{mpsc, oneshot},
};

/// One tool call of a prompt, as the chat shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolLine {
    pub id: String,
    pub title: String,
    pub failed: bool,
}

/// How a prompt ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// `stopReason` from the response.
    Stopped(String),
    /// A JSON-RPC error message.
    Failed(String),
}

/// A file `send_file` gave the prompt, to send after its reply.
/// See `docs/Media.md#sending-files-out`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutFile {
    /// Alyosha's copy, which the bridge deletes once sent or given up on.
    pub path: PathBuf,
    pub name: String,
    pub mime: String,
    pub size: u64,
    pub caption: Option<String>,
}

impl OutFile {
    /// Deletes Alyosha's copy.
    pub fn remove(&self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// A finished prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finished {
    pub key: String,
    pub text: String,
    pub tools: Vec<ToolLine>,
    pub outcome: Outcome,
    /// Files to send after the reply, in order.
    pub files: Vec<OutFile>,
}

impl Finished {
    /// The reply text, by the bridges' table: the collected text, **Done.**
    /// for an empty `end_turn`, **Stopped.** after a cancelled turn, the stop
    /// reason otherwise, and **Failed:** with the error.
    pub fn reply(&self) -> String {
        let text = self.text.trim_end();
        let join = |suffix: &str| {
            if text.is_empty() {
                suffix.to_owned()
            } else {
                format!("{text}\n\n{suffix}")
            }
        };
        match &self.outcome {
            Outcome::Stopped(reason) if reason == "end_turn" => {
                if text.is_empty() {
                    "Done.".to_owned()
                } else {
                    text.to_owned()
                }
            }
            Outcome::Stopped(reason) if reason == "cancelled" => join("Stopped."),
            Outcome::Stopped(reason) => join(reason),
            Outcome::Failed(message) => join(&format!("Failed: {message}")),
        }
    }
}

/// An approval or question from Alyosha.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Permission {
    /// The JSON-RPC request ID to answer.
    pub request: u64,
    pub tool_call_id: String,
    pub title: String,
    pub body: String,
    /// `(optionId, name)` in order; the last is the negative one.
    pub options: Vec<(String, String)>,
    /// The open prompt it belongs to, if any.
    pub key: Option<String>,
}

/// What the bridge hears from Alyosha.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A prompt's tool calls changed; for **Working…** messages.
    Tools {
        key: String,
        tools: Vec<ToolLine>,
    },
    Finished(Finished),
    Permission(Permission),
    /// Alyosha stopped waiting for this call's request.
    PermissionExpired {
        tool_call_id: String,
    },
    JobFinished {
        job_id: String,
        text: String,
    },
    /// `alyosha acp` closed its output.
    Exited,
}

#[derive(Default)]
struct Prompt {
    key: String,
    text: String,
    tools: Vec<ToolLine>,
    files: Vec<OutFile>,
}

#[derive(Default)]
struct Shared {
    session: Option<String>,
    prompts: VecDeque<Prompt>,
    /// The JSON-RPC ID of each open prompt's request.
    prompt_requests: HashMap<u64, String>,
    pending: HashMap<u64, oneshot::Sender<Result<Value, RpcError>>>,
    open_permissions: Vec<String>,
    /// While `session/load` replays the conversation, its updates are
    /// dropped, so they never reach a chat.
    loading: bool,
}

/// A JSON-RPC error from `alyosha acp`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RpcError {
    code: i64,
    message: String,
}

/// The error `session/load` gives for a key with no session.
const RESOURCE_NOT_FOUND: i64 = -32002;

/// A connection to `alyosha acp`.
#[derive(Clone)]
pub struct Bridge {
    out: mpsc::UnboundedSender<String>,
    shared: Arc<Mutex<Shared>>,
    next_id: Arc<AtomicU64>,
    events: mpsc::UnboundedSender<Event>,
    child: Arc<tokio::sync::Mutex<Option<Child>>>,
}

impl Bridge {
    /// Starts `program acp` with piped standard input and output. Its
    /// standard error goes to the bridge's own.
    pub fn spawn(program: &Path) -> std::io::Result<(Self, mpsc::UnboundedReceiver<Event>)> {
        Self::spawn_with(program, &["acp"])
    }

    /// Starts `program` with `args`, such as `acp --guest`, like `spawn`.
    pub fn spawn_with(
        program: &Path,
        args: &[&str],
    ) -> std::io::Result<(Self, mpsc::UnboundedReceiver<Event>)> {
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()?;
        let stdin = child.stdin.take().expect("piped");
        let stdout = child.stdout.take().expect("piped");
        let (bridge, events) = Self::connect(stdout, stdin);
        *bridge.child.try_lock().expect("fresh") = Some(child);
        Ok((bridge, events))
    }

    /// Speaks ACP over any reader and writer, for tests.
    pub fn connect<R, W>(reader: R, writer: W) -> (Self, mpsc::UnboundedReceiver<Event>)
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (out, mut lines) = mpsc::unbounded_channel::<String>();
        let (events, receiver) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut writer = writer;
            while let Some(line) = lines.recv().await {
                if writer
                    .write_all(format!("{line}\n").as_bytes())
                    .await
                    .is_err()
                {
                    break;
                }
                let _ = writer.flush().await;
            }
            let _ = writer.shutdown().await;
        });
        let bridge = Self {
            out,
            shared: Arc::default(),
            next_id: Arc::new(AtomicU64::new(1)),
            events,
            child: Arc::default(),
        };
        let reader_bridge = bridge.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(reader).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if let Ok(message) = serde_json::from_str::<Value>(&line) {
                    reader_bridge.receive(message);
                }
            }
            let open: Vec<String> = reader_bridge
                .lock()
                .prompt_requests
                .drain()
                .map(|(_, key)| key)
                .collect();
            for key in open {
                reader_bridge.finish_prompt(key, Outcome::Failed("alyosha acp stopped".into()));
            }
            let _ = reader_bridge.events.send(Event::Exited);
        });
        (bridge, receiver)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.shared.lock().unwrap_or_else(PoisonError::into_inner)
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        self.request_coded(method, params)
            .await
            .map_err(|error| error.message)
    }

    async fn request_coded(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        self.lock().pending.insert(id, sender);
        let _ = self.out.send(
            json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string(),
        );
        receiver.await.map_err(|_| RpcError {
            code: 0,
            message: "alyosha acp stopped".to_owned(),
        })?
    }

    fn notify(&self, method: &str, params: Value) {
        let _ = self
            .out
            .send(json!({"jsonrpc": "2.0", "method": method, "params": params}).to_string());
    }

    /// `initialize` with this bridge's program name.
    pub async fn initialize(&self, client_name: &str) -> Result<(), String> {
        self.request(
            "initialize",
            json!({"protocolVersion": 1, "clientCapabilities": {}, "clientInfo": {"name": client_name, "version": "1"}}),
        )
        .await
        .map(drop)
    }

    /// `session/new` under `key` in the workspace; waits until Alyosha's
    /// startup ends. The old session stays saved under the key, and this
    /// one becomes its current session.
    pub async fn new_session(&self, workspace: &Path, key: &str) -> Result<String, String> {
        let result = self
            .request(
                "session/new",
                json!({
                    "cwd": workspace.display().to_string(),
                    "mcpServers": [],
                    "_meta": {"alyosha": {"sessionKey": key}},
                }),
            )
            .await?;
        let session = result["sessionId"]
            .as_str()
            .ok_or("no sessionId")?
            .to_owned();
        self.lock().session = Some(session.clone());
        Ok(session)
    }

    /// Resumes the key's current session with `session/load`, or starts one
    /// with `session/new` if the key has none. The conversation `session/load`
    /// replays is dropped. Returns whether a session was resumed.
    /// See `docs/SessionStore.md#bridges`.
    pub async fn open_session(&self, workspace: &Path, key: &str) -> Result<bool, String> {
        self.lock().loading = true;
        let loaded = self
            .request_coded(
                "session/load",
                json!({"sessionId": key, "cwd": workspace.display().to_string(), "mcpServers": []}),
            )
            .await;
        self.lock().loading = false;
        match loaded {
            Ok(_) => {
                self.lock().session = Some(key.to_owned());
                Ok(true)
            }
            Err(error) if error.code == RESOURCE_NOT_FOUND => {
                self.new_session(workspace, key).await.map(|_| false)
            }
            Err(error) => Err(error.message),
        }
    }

    /// Sends a prompt at once. Its updates and response arrive as events
    /// tagged with `key`.
    pub fn prompt(&self, key: &str, text: &str) {
        self.send_prompt(key, text, Vec::new(), None);
    }

    /// Sends a prompt with attachment blocks after its text.
    pub fn prompt_with(&self, key: &str, text: &str, blocks: Vec<Value>) {
        self.send_prompt(key, text, blocks, None);
    }

    /// Sends a group member's prompt to a guest session, with its sender in
    /// `_meta`. See `docs/Channels.md#guest-mode`.
    pub fn prompt_from(
        &self,
        key: &str,
        text: &str,
        blocks: Vec<Value>,
        sender_id: &str,
        sender_name: &str,
    ) {
        let meta = json!({"alyosha": {"sender": {"id": sender_id, "name": sender_name}}});
        self.send_prompt(key, text, blocks, Some(meta));
    }

    fn send_prompt(&self, key: &str, text: &str, blocks: Vec<Value>, meta: Option<Value>) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let session = {
            let mut shared = self.lock();
            shared.prompts.push_back(Prompt {
                key: key.to_owned(),
                ..Prompt::default()
            });
            shared.prompt_requests.insert(id, key.to_owned());
            shared.session.clone().unwrap_or_default()
        };
        let mut prompt = vec![json!({"type": "text", "text": text})];
        prompt.extend(attachments::limit(blocks));
        let mut params = json!({
            "sessionId": session,
            "prompt": prompt,
        });
        if let Some(meta) = meta {
            params["_meta"] = meta;
        }
        let _ = self.out.send(
            json!({"jsonrpc": "2.0", "id": id, "method": "session/prompt", "params": params})
                .to_string(),
        );
    }

    /// Closes a prompt in the same step as its response, so later updates
    /// go to the next prompt.
    fn finish_prompt(&self, key: String, outcome: Outcome) {
        let prompt = {
            let mut shared = self.lock();
            let position = shared.prompts.iter().position(|prompt| prompt.key == key);
            position.and_then(|position| shared.prompts.remove(position))
        };
        let prompt = prompt.unwrap_or_default();
        let _ = self.events.send(Event::Finished(Finished {
            key,
            text: prompt.text,
            tools: prompt.tools,
            outcome,
            files: prompt.files,
        }));
    }

    /// The keys of prompts still open, oldest first.
    pub fn open_keys(&self) -> Vec<String> {
        self.lock()
            .prompts
            .iter()
            .map(|prompt| prompt.key.clone())
            .collect()
    }

    /// `session/cancel` for the current session.
    pub fn cancel(&self) {
        let session = self.lock().session.clone().unwrap_or_default();
        self.notify("session/cancel", json!({"sessionId": session}));
    }

    /// Answers a permission request: `Some(optionId)` selects, `None`
    /// cancels.
    pub fn answer(&self, request: u64, option: Option<&str>) {
        let outcome = match option {
            Some(option) => json!({"outcome": "selected", "optionId": option}),
            None => json!({"outcome": "cancelled"}),
        };
        let _ = self.out.send(
            json!({"jsonrpc": "2.0", "id": request, "result": {"outcome": outcome}}).to_string(),
        );
    }

    /// `/cancel`: `session/cancel` first, then every open request answered
    /// `cancelled`, as ACP requires.
    pub fn cancel_with_requests(&self, open_requests: &[u64]) {
        self.cancel();
        for request in open_requests {
            self.answer(*request, None);
        }
    }

    /// Closes the child's input, waits up to 10 seconds, then kills it.
    pub async fn shutdown(&self) {
        let _ = self.out.send(String::new());
        if let Some(mut child) = self.child.lock().await.take() {
            drop(child.stdin.take());
            if tokio::time::timeout(Duration::from_secs(10), child.wait())
                .await
                .is_err()
            {
                let _ = child.kill().await;
            }
        }
    }

    fn receive(&self, message: Value) {
        let method = message["method"].as_str();
        match (method, message.get("id")) {
            (None, Some(id)) => {
                let Some(id) = id.as_u64() else { return };
                let prompt = self.lock().prompt_requests.remove(&id);
                if let Some(key) = prompt {
                    let outcome = match message.get("error") {
                        Some(error) => {
                            Outcome::Failed(error["message"].as_str().unwrap_or("error").to_owned())
                        }
                        None => Outcome::Stopped(
                            message["result"]["stopReason"]
                                .as_str()
                                .unwrap_or("end_turn")
                                .to_owned(),
                        ),
                    };
                    self.finish_prompt(key, outcome);
                    return;
                }
                let waiter = self.lock().pending.remove(&id);
                if let Some(waiter) = waiter {
                    let result = match message.get("error") {
                        Some(error) => Err(RpcError {
                            code: error["code"].as_i64().unwrap_or_default(),
                            message: error["message"].as_str().unwrap_or("error").to_owned(),
                        }),
                        None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
                    };
                    let _ = waiter.send(result);
                }
            }
            (Some("session/request_permission"), Some(id)) => {
                let params = &message["params"];
                let tool_call = &params["toolCall"];
                let body = tool_call["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|content| content["content"]["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                let options = params["options"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|option| {
                        (
                            option["optionId"].as_str().unwrap_or_default().to_owned(),
                            option["name"].as_str().unwrap_or_default().to_owned(),
                        )
                    })
                    .collect();
                let tool_call_id = tool_call["toolCallId"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                let key = {
                    let mut shared = self.lock();
                    shared.open_permissions.push(tool_call_id.clone());
                    shared.prompts.front().map(|prompt| prompt.key.clone())
                };
                let _ = self.events.send(Event::Permission(Permission {
                    request: id.as_u64().unwrap_or_default(),
                    tool_call_id,
                    title: tool_call["title"].as_str().unwrap_or_default().to_owned(),
                    body,
                    options,
                    key,
                }));
            }
            (Some("session/update"), None) => self.update(&message["params"]["update"]),
            (Some("_alyosha/job_finished"), None) => {
                let params = &message["params"];
                let _ = self.events.send(Event::JobFinished {
                    job_id: params["jobId"].as_str().unwrap_or_default().to_owned(),
                    text: params["text"].as_str().unwrap_or_default().to_owned(),
                });
            }
            (Some(_), Some(id)) => {
                let reply = json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "method not found"}});
                let _ = self.out.send(reply.to_string());
            }
            _ => {}
        }
    }

    /// Attributes an update to the oldest open prompt.
    fn update(&self, update: &Value) {
        let mut shared = self.lock();
        if shared.loading {
            return;
        }
        let kind = update["sessionUpdate"].as_str().unwrap_or_default();
        if kind == "tool_call_update" && update["status"] == "failed" {
            let id = update["toolCallId"].as_str().unwrap_or_default();
            if let Some(position) = shared.open_permissions.iter().position(|open| open == id) {
                shared.open_permissions.remove(position);
                let _ = self.events.send(Event::PermissionExpired {
                    tool_call_id: id.to_owned(),
                });
            }
        }
        let Some(prompt) = shared.prompts.front_mut() else {
            return;
        };
        match kind {
            "agent_message_chunk" if update["content"]["type"] == "resource_link" => {
                let link = &update["content"];
                if let Some(path) = link["uri"].as_str().and_then(|uri| uri.strip_prefix("file://")) {
                    prompt.files.push(OutFile {
                        path: PathBuf::from(path),
                        name: link["name"].as_str().unwrap_or("file").to_owned(),
                        mime: link["mimeType"]
                            .as_str()
                            .unwrap_or("application/octet-stream")
                            .to_owned(),
                        size: link["size"].as_u64().unwrap_or_default(),
                        caption: link["description"].as_str().map(str::to_owned),
                    });
                }
            }
            "agent_message_chunk" => {
                if let Some(text) = update["content"]["text"].as_str() {
                    if !prompt.text.is_empty() && !prompt.text.ends_with('\n') {
                        prompt.text.push_str("\n\n");
                    }
                    prompt.text.push_str(text);
                }
            }
            "tool_call" => {
                prompt.tools.push(ToolLine {
                    id: update["toolCallId"].as_str().unwrap_or_default().to_owned(),
                    title: update["title"].as_str().unwrap_or_default().to_owned(),
                    failed: false,
                });
                let event = Event::Tools {
                    key: prompt.key.clone(),
                    tools: prompt.tools.clone(),
                };
                let _ = self.events.send(event);
            }
            "tool_call_update" => {
                let id = update["toolCallId"].as_str().unwrap_or_default();
                if update["status"] == "failed"
                    && let Some(line) = prompt.tools.iter_mut().find(|line| line.id == id)
                {
                    line.failed = true;
                    let event = Event::Tools {
                        key: prompt.key.clone(),
                        tools: prompt.tools.clone(),
                    };
                    let _ = self.events.send(event);
                }
            }
            _ => {}
        }
    }

    /// Marks a permission request as answered, so a later `failed` update is
    /// not reported as an expiry.
    pub fn forget_permission(&self, tool_call_id: &str) {
        self.lock()
            .open_permissions
            .retain(|open| open != tool_call_id);
    }
}

/// Splits text into parts of at most `limit` characters, at the last line
/// break before the limit, or at the limit if a line is longer.
pub fn split(text: &str, limit: usize) -> Vec<String> {
    let mut parts = Vec::new();
    let mut rest: &str = text;
    while rest.chars().count() > limit {
        let cut_at = rest
            .char_indices()
            .nth(limit)
            .map_or(rest.len(), |(index, _)| index);
        let head = &rest[..cut_at];
        let cut = match head.rfind('\n') {
            Some(index) if index > 0 => index,
            _ => cut_at,
        };
        parts.push(rest[..cut].to_owned());
        rest = rest[cut..].strip_prefix('\n').unwrap_or(&rest[cut..]);
    }
    if !rest.is_empty() || parts.is_empty() {
        parts.push(rest.to_owned());
    }
    parts
}

/// The **Working…** message: the call titles, failed ones marked, at most
/// the last 20 lines.
pub fn working_text(tools: &[ToolLine]) -> String {
    let lines: Vec<String> = tools
        .iter()
        .map(|line| {
            if line.failed {
                format!("{} (failed)", line.title)
            } else {
                line.title.clone()
            }
        })
        .collect();
    let start = lines.len().saturating_sub(20);
    format!("Working…\n{}", lines[start..].join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_text_splits_at_line_breaks() {
        let text = format!(
            "{}\n{}\n{}",
            "a".repeat(3000),
            "b".repeat(3000),
            "c".repeat(3000)
        );
        let parts = split(&text, 4000);
        assert_eq!(parts.len(), 3);
        assert!(parts.iter().all(|part| part.chars().count() < 4000));
        let long_line = "x".repeat(9000);
        assert_eq!(
            split(&long_line, 4000)
                .iter()
                .map(String::len)
                .collect::<Vec<_>>(),
            [4000, 4000, 1000]
        );
        assert_eq!(split("", 10), [""]);
    }

    #[test]
    fn replies_follow_the_table() {
        let finished = |text: &str, outcome: Outcome| Finished {
            key: "k".into(),
            text: text.into(),
            tools: Vec::new(),
            outcome,
            files: Vec::new(),
        };
        assert_eq!(
            finished("", Outcome::Stopped("end_turn".into())).reply(),
            "Done."
        );
        assert_eq!(
            finished("Hi", Outcome::Stopped("end_turn".into())).reply(),
            "Hi"
        );
        assert_eq!(
            finished("Partial", Outcome::Stopped("cancelled".into())).reply(),
            "Partial\n\nStopped."
        );
        assert_eq!(
            finished("", Outcome::Failed("timeout".into())).reply(),
            "Failed: timeout"
        );
    }

    #[test]
    fn the_working_message_marks_failures_and_keeps_twenty_lines() {
        let tools: Vec<ToolLine> = (0..25)
            .map(|n| ToolLine {
                id: n.to_string(),
                title: format!("call {n}"),
                failed: n == 24,
            })
            .collect();
        let text = working_text(&tools);
        assert_eq!(text.lines().count(), 21);
        assert!(text.ends_with("call 24 (failed)"));
        assert!(text.contains("\ncall 5\n"));
    }

    /// A fake agent over in-memory pipes.
    async fn fake() -> (
        Bridge,
        mpsc::UnboundedReceiver<Event>,
        tokio::io::Lines<BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>>,
        tokio::io::WriteHalf<tokio::io::DuplexStream>,
    ) {
        let (ours, theirs) = tokio::io::duplex(1 << 16);
        let (read, write) = tokio::io::split(theirs);
        let (bridge, events) = Bridge::connect(read, write);
        let (agent_read, agent_write) = tokio::io::split(ours);
        (
            bridge,
            events,
            BufReader::new(agent_read).lines(),
            agent_write,
        )
    }

    async fn read(
        agent_in: &mut tokio::io::Lines<BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>>,
    ) -> Value {
        serde_json::from_str(&agent_in.next_line().await.unwrap().unwrap()).unwrap()
    }

    async fn write(agent_out: &mut tokio::io::WriteHalf<tokio::io::DuplexStream>, message: Value) {
        agent_out
            .write_all(format!("{message}\n").as_bytes())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn open_session_falls_back_to_new_under_the_key() {
        let (bridge, _events, mut agent_in, mut agent_out) = fake().await;
        let opening = tokio::spawn({
            let bridge = bridge.clone();
            async move { bridge.open_session(Path::new("/w"), "owner:qq").await }
        });
        let load = read(&mut agent_in).await;
        assert_eq!(load["method"], "session/load");
        assert_eq!(load["params"]["sessionId"], "owner:qq");
        write(&mut agent_out, json!({"jsonrpc": "2.0", "id": load["id"], "error": {"code": -32002, "message": "Resource not found"}})).await;
        let new = read(&mut agent_in).await;
        assert_eq!(new["method"], "session/new");
        assert_eq!(new["params"]["_meta"]["alyosha"]["sessionKey"], "owner:qq");
        write(
            &mut agent_out,
            json!({"jsonrpc": "2.0", "id": new["id"], "result": {"sessionId": "owner:qq"}}),
        )
        .await;
        assert_eq!(opening.await.unwrap(), Ok(false));
        assert_eq!(bridge.lock().session.as_deref(), Some("owner:qq"));
    }

    #[tokio::test]
    async fn the_replay_of_a_loaded_session_never_reaches_a_prompt() {
        let (bridge, mut events, mut agent_in, mut agent_out) = fake().await;
        let opening = tokio::spawn({
            let bridge = bridge.clone();
            async move { bridge.open_session(Path::new("/w"), "owner:qq").await }
        });
        let load = read(&mut agent_in).await;
        let update = |text: &str| json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "owner:qq", "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}}}});
        write(&mut agent_out, update("an old reply")).await;
        write(
            &mut agent_out,
            json!({"jsonrpc": "2.0", "id": load["id"], "result": {}}),
        )
        .await;
        assert_eq!(opening.await.unwrap(), Ok(true));
        bridge.prompt("m1", "hello");
        let prompt = read(&mut agent_in).await;
        assert_eq!(prompt["params"]["sessionId"], "owner:qq");
        write(&mut agent_out, update("new")).await;
        write(
            &mut agent_out,
            json!({"jsonrpc": "2.0", "id": prompt["id"], "result": {"stopReason": "end_turn"}}),
        )
        .await;
        loop {
            if let Some(Event::Finished(finished)) = events.recv().await {
                assert_eq!(finished.text, "new");
                break;
            }
        }
    }

    #[tokio::test]
    async fn updates_go_to_the_oldest_open_prompt() {
        let (bridge, mut events, mut agent_in, mut agent_out) = fake().await;
        bridge.lock().session = Some("s1".into());
        bridge.prompt("m1", "first");
        bridge.prompt("m2", "second");
        let first: Value =
            serde_json::from_str(&agent_in.next_line().await.unwrap().unwrap()).unwrap();
        let second: Value =
            serde_json::from_str(&agent_in.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(first["params"]["prompt"][0]["text"], "first");
        for message in [
            json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s1", "update": {"sessionUpdate": "tool_call", "toolCallId": "a", "title": "read_file a.txt"}}}),
            json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s1", "update": {"sessionUpdate": "tool_call_update", "toolCallId": "a", "status": "failed"}}}),
            json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s1", "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "one"}}}}),
            json!({"jsonrpc": "2.0", "id": first["id"], "result": {"stopReason": "end_turn"}}),
            json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s1", "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "two"}}}}),
            json!({"jsonrpc": "2.0", "id": second["id"], "error": {"code": -32603, "message": "timeout"}}),
        ] {
            agent_out
                .write_all(format!("{message}\n").as_bytes())
                .await
                .unwrap();
        }
        let mut finished = Vec::new();
        while finished.len() < 2 {
            if let Some(Event::Finished(done)) = events.recv().await {
                finished.push(done);
            }
        }
        assert_eq!(finished[0].key, "m1");
        assert_eq!(finished[0].reply(), "one");
        assert!(finished[0].tools[0].failed);
        assert_eq!(finished[1].reply(), "two\n\nFailed: timeout");
    }

    #[tokio::test]
    async fn cancel_goes_before_the_open_requests_answers() {
        let (bridge, _events, mut agent_in, _agent_out) = fake().await;
        bridge.lock().session = Some("s1".into());
        bridge.cancel_with_requests(&[7]);
        let first: Value =
            serde_json::from_str(&agent_in.next_line().await.unwrap().unwrap()).unwrap();
        let second: Value =
            serde_json::from_str(&agent_in.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(first["method"], "session/cancel");
        assert_eq!(second["id"], 7);
        assert_eq!(second["result"]["outcome"]["outcome"], "cancelled");
    }
}
