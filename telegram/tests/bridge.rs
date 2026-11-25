//! The Telegram bridge against a fake Bot API server and a scripted fake ACP
//! agent. See `docs/develop/TelegramBridge.md#verification`.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

use alyosha_bridge::Bridge;
use alyosha_schedule::Schedule;
use alyosha_telegram::{Api, Settings, run};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
};

const OWNER: i64 = 42;

/// The fake Bot API: queued updates, recorded calls.
#[derive(Default)]
struct BotApi {
    updates: Mutex<VecDeque<Value>>,
    calls: Mutex<Vec<(String, Value)>>,
    /// Answer the next sendMessage with 429.
    rate_limit_once: Mutex<bool>,
    next_message: Mutex<i64>,
}

impl BotApi {
    fn calls(&self, method: &str) -> Vec<Value> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(name, _)| name == method)
            .map(|(_, body)| body.clone())
            .collect()
    }

    fn texts(&self) -> Vec<String> {
        self.calls("sendMessage")
            .iter()
            .map(|body| body["text"].as_str().unwrap().to_owned())
            .collect()
    }

    fn push(&self, update: Value) {
        self.updates.lock().unwrap().push_back(update);
    }

    async fn serve(self: Arc<Self>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let api = self.clone();
                tokio::spawn(async move {
                    let mut buffer = Vec::new();
                    let mut chunk = [0u8; 8192];
                    let end = loop {
                        let read = socket.read(&mut chunk).await.unwrap_or(0);
                        if read == 0 {
                            return;
                        }
                        buffer.extend_from_slice(&chunk[..read]);
                        if let Some(end) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                            break end + 4;
                        }
                    };
                    let head = String::from_utf8_lossy(&buffer[..end]).to_string();
                    let length: usize = head
                        .lines()
                        .find_map(|line| {
                            line.to_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse().unwrap())
                        })
                        .unwrap_or(0);
                    while buffer.len() < end + length {
                        let read = socket.read(&mut chunk).await.unwrap_or(0);
                        if read == 0 {
                            break;
                        }
                        buffer.extend_from_slice(&chunk[..read]);
                    }
                    let body: Value =
                        serde_json::from_slice(&buffer[end..end + length]).unwrap_or(Value::Null);
                    let method = head
                        .split_whitespace()
                        .nth(1)
                        .unwrap()
                        .rsplit('/')
                        .next()
                        .unwrap()
                        .to_owned();
                    let (status, reply) = api.answer(&method, body).await;
                    let text = reply.to_string();
                    let response = format!(
                        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}",
                        text.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                });
            }
        });
        url
    }

    async fn answer(&self, method: &str, body: Value) -> (u16, Value) {
        if method == "getUpdates" {
            for _ in 0..10 {
                let batch: Vec<Value> = self.updates.lock().unwrap().drain(..).collect();
                if !batch.is_empty() {
                    return (200, json!({"ok": true, "result": batch}));
                }
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
            return (200, json!({"ok": true, "result": []}));
        }
        if method == "sendMessage" {
            let mut limited = self.rate_limit_once.lock().unwrap();
            if *limited {
                *limited = false;
                return (
                    429,
                    json!({"ok": false, "error_code": 429, "description": "Too Many Requests", "parameters": {"retry_after": 1}}),
                );
            }
        }
        self.calls.lock().unwrap().push((method.to_owned(), body));
        let mut next = self.next_message.lock().unwrap();
        *next += 1;
        (200, json!({"ok": true, "result": {"message_id": *next}}))
    }
}

fn message(id: i64, from: i64, text: &str) -> Value {
    json!({"update_id": id, "message": {"message_id": id, "from": {"id": from}, "chat": {"id": from, "type": "private"}, "text": text}})
}

/// A scripted ACP agent: replies by the prompt's text.
async fn agent(
    read: tokio::io::ReadHalf<tokio::io::DuplexStream>,
    mut write: tokio::io::WriteHalf<tokio::io::DuplexStream>,
    log: Arc<Mutex<Vec<Value>>>,
) {
    let mut lines = BufReader::new(read).lines();
    let send = |value: Value| format!("{value}\n");
    let mut waiting_permission: Option<Value> = None;
    while let Ok(Some(line)) = lines.next_line().await {
        let message: Value = serde_json::from_str(&line).unwrap();
        log.lock().unwrap().push(message.clone());
        let id = message["id"].clone();
        let update = |update: Value| json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s1", "update": update}});
        let chunk = |text: &str| {
            update(
                json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}}),
            )
        };
        let mut out = String::new();
        match message["method"].as_str() {
            Some("initialize") => {
                out += &send(json!({"jsonrpc": "2.0", "id": id, "result": {"protocolVersion": 1}}))
            }
            Some("session/new") => {
                out += &send(json!({"jsonrpc": "2.0", "id": id, "result": {"sessionId": "s1"}}))
            }
            Some("session/cancel") => {}
            Some("session/prompt") => {
                let text = message["params"]["prompt"][0]["text"]
                    .as_str()
                    .unwrap()
                    .to_owned();
                match text.as_str() {
                    "hello" => out += &send(chunk("Hi there.")),
                    "tools" => {
                        for value in [
                            update(
                                json!({"sessionUpdate": "tool_call", "toolCallId": "a", "title": "read_file a.txt", "kind": "read", "status": "in_progress"}),
                            ),
                            update(
                                json!({"sessionUpdate": "tool_call_update", "toolCallId": "a", "status": "completed"}),
                            ),
                            update(
                                json!({"sessionUpdate": "tool_call", "toolCallId": "b", "title": "read_file b.txt", "kind": "read", "status": "in_progress"}),
                            ),
                            update(
                                json!({"sessionUpdate": "tool_call_update", "toolCallId": "b", "status": "failed"}),
                            ),
                            chunk("done"),
                        ] {
                            out += &send(value);
                        }
                    }
                    "long" => {
                        let long: String = (0..90)
                            .map(|n| format!("{n:03} {}\n", "x".repeat(96)))
                            .collect();
                        out += &send(chunk(&long));
                    }
                    "approve" => {
                        out += &send(
                            json!({"jsonrpc": "2.0", "id": 100, "method": "session/request_permission", "params": {
                                "sessionId": "s1",
                                "toolCall": {"toolCallId": "c1", "title": "Approve? google.send", "content": [{"type": "content", "content": {"type": "text", "text": "to: anna"}}]},
                                "options": [{"optionId": "0", "name": "Approve", "kind": "allow_once"}, {"optionId": "1", "name": "Decline", "kind": "reject_once"}],
                            }}),
                        );
                        waiting_permission = Some(id.clone());
                        write.write_all(out.as_bytes()).await.unwrap();
                        continue;
                    }
                    "exit" => return,
                    _ => out += &send(chunk("?")),
                }
                out += &send(
                    json!({"jsonrpc": "2.0", "id": id, "result": {"stopReason": "end_turn"}}),
                );
            }
            None if message["id"] == 100 => {
                // The permission answer: finish the waiting prompt.
                if let Some(prompt) = waiting_permission.take() {
                    let outcome = message["result"]["outcome"]["outcome"]
                        .as_str()
                        .unwrap()
                        .to_owned();
                    out += &send(chunk(&format!("permission {outcome}")));
                    let reason = if outcome == "cancelled" {
                        "cancelled"
                    } else {
                        "end_turn"
                    };
                    out += &send(
                        json!({"jsonrpc": "2.0", "id": prompt, "result": {"stopReason": reason}}),
                    );
                }
            }
            _ => {}
        }
        write.write_all(out.as_bytes()).await.unwrap();
    }
}

struct Harness {
    api: Arc<BotApi>,
    log: Arc<Mutex<Vec<Value>>>,
    task: tokio::task::JoinHandle<u8>,
    _root: tempfile::TempDir,
}

async fn start() -> Harness {
    let api = Arc::new(BotApi::default());
    let url = api.clone().serve().await;
    let (ours, theirs) = tokio::io::duplex(1 << 20);
    let (agent_read, agent_write) = tokio::io::split(ours);
    let (bridge_read, bridge_write) = tokio::io::split(theirs);
    let log = Arc::new(Mutex::new(Vec::new()));
    tokio::spawn(agent(agent_read, agent_write, log.clone()));
    let (bridge, events) = Bridge::connect(bridge_read, bridge_write);
    let root = tempfile::tempdir().unwrap();
    let settings = Settings {
        owner_id: OWNER,
        workspace: root.path().to_owned(),
        schedule: Schedule::new(root.path()),
        timezone: chrono_tz::Europe::Amsterdam,
    };
    let task = tokio::spawn(run(Api::new(&url, "TOKEN"), bridge, events, settings));
    Harness {
        api,
        log,
        task,
        _root: root,
    }
}

async fn wait_for(check: impl Fn() -> bool) {
    for _ in 0..200 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("the condition did not hold within 5 seconds");
}

fn prompts(log: &Mutex<Vec<Value>>) -> Vec<String> {
    log.lock()
        .unwrap()
        .iter()
        .filter(|message| message["method"] == "session/prompt")
        .map(|message| {
            message["params"]["prompt"][0]["text"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn owner_messages_reach_alyosha_and_strangers_are_dropped() {
    let harness = start().await;
    harness.api.push(message(1, 7, "from a stranger"));
    harness.api.push(message(2, OWNER, "hello"));
    let api = harness.api.clone();
    wait_for(|| api.texts().contains(&"Hi there.".to_owned())).await;
    assert_eq!(prompts(&harness.log), ["hello"]);
    let reply = &harness
        .api
        .calls("sendMessage")
        .into_iter()
        .find(|body| body["text"] == "Hi there.")
        .unwrap();
    assert!(reply.get("parse_mode").is_none(), "plain text");
    assert_eq!(
        harness.api.calls("setMyCommands")[0]["commands"]
            .as_array()
            .unwrap()
            .len(),
        5
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tool_calls_show_in_one_working_message() {
    let harness = start().await;
    harness.api.push(message(1, OWNER, "tools"));
    let api = harness.api.clone();
    wait_for(|| api.texts().contains(&"done".to_owned())).await;
    let working: Vec<String> = harness
        .api
        .texts()
        .into_iter()
        .filter(|text| text.starts_with("Working…"))
        .collect();
    assert_eq!(working.len(), 1, "one Working… message");
    let edits = harness.api.calls("editMessageText");
    let last = edits
        .last()
        .map(|body| body["text"].as_str().unwrap().to_owned())
        .unwrap_or_default();
    assert_eq!(last, "Working…\nread_file a.txt\nread_file b.txt (failed)");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn long_replies_split_and_rate_limits_are_waited_out() {
    let harness = start().await;
    *harness.api.rate_limit_once.lock().unwrap() = true;
    harness.api.push(message(1, OWNER, "long"));
    let api = harness.api.clone();
    wait_for(|| api.texts().len() >= 3).await;
    let texts = harness.api.texts();
    assert_eq!(texts.len(), 3);
    assert!(texts.iter().all(|text| text.chars().count() < 4000));
    assert!(texts[0].starts_with("000 ") && texts[2].ends_with(&"x".repeat(96)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn photos_get_the_text_only_reply() {
    let harness = start().await;
    harness.api.push(json!({"update_id": 1, "message": {"message_id": 1, "from": {"id": OWNER}, "chat": {"id": OWNER, "type": "private"}, "photo": [{}]}}));
    let api = harness.api.clone();
    wait_for(|| {
        api.texts()
            .contains(&"Only text messages work for now.".to_owned())
    })
    .await;
    assert!(prompts(&harness.log).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn approvals_use_buttons_and_cancel_comes_first() {
    let harness = start().await;
    harness.api.push(message(1, OWNER, "approve"));
    let api = harness.api.clone();
    wait_for(|| {
        api.texts()
            .iter()
            .any(|text| text.starts_with("Approve? google.send"))
    })
    .await;
    let request = harness
        .api
        .calls("sendMessage")
        .into_iter()
        .find(|body| body["text"] == "Approve? google.send\n\nto: anna")
        .unwrap();
    let buttons = &request["reply_markup"]["inline_keyboard"];
    assert_eq!(buttons[0][0]["text"], "Approve");
    assert_eq!(buttons[1][0]["callback_data"], "r1:1");
    // A stranger's press changes nothing; the owner's answers.
    harness.api.push(
        json!({"update_id": 2, "callback_query": {"id": "q1", "from": {"id": 7}, "data": "r1:0"}}),
    );
    harness.api.push(json!({"update_id": 3, "callback_query": {"id": "q2", "from": {"id": OWNER}, "data": "r1:0"}}));
    wait_for(|| api.texts().contains(&"permission selected".to_owned())).await;
    assert!(harness.api.calls("editMessageText").iter().any(|body| {
        body["text"]
            .as_str()
            .unwrap()
            .ends_with("Answered: Approve")
    }));

    harness.api.push(message(4, OWNER, "approve"));
    wait_for(|| {
        api.texts()
            .iter()
            .filter(|text| text.starts_with("Approve?"))
            .count()
            == 2
    })
    .await;
    harness.api.push(message(5, OWNER, "/cancel"));
    wait_for(|| {
        api.texts()
            .contains(&"permission cancelled\n\nStopped.".to_owned())
    })
    .await;
    let log = harness.log.lock().unwrap();
    let cancel = log
        .iter()
        .rposition(|message| message["method"] == "session/cancel")
        .unwrap();
    let answer = log
        .iter()
        .rposition(|message| message["id"] == 100 && message.get("result").is_some())
        .unwrap();
    assert!(cancel < answer, "session/cancel goes first");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_exited_agent_is_reported_and_the_bridge_fails() {
    let harness = start().await;
    harness.api.push(message(1, OWNER, "exit"));
    let code = tokio::time::timeout(Duration::from_secs(10), harness.task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(code, 1);
    assert!(
        harness
            .api
            .texts()
            .contains(&"Alyosha stopped unexpectedly. Restarting with a new session.".to_owned())
    );
}
