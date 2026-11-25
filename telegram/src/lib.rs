//! `alyosha-telegram`: relays private messages between the owner's Telegram
//! account and one `alyosha acp` child.
//!
//! It long-polls the Bot API, so it opens no ports, and answers only the
//! owner in a private chat. See `docs/develop/TelegramBridge.md`.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use alyosha_bridge::{Bridge, Event, Permission, split, working_text};
use alyosha_schedule::Schedule;
use chrono::Utc;
use chrono_tz::Tz;
use serde_json::{Value, json};
use tokio::sync::mpsc;

pub const HELP: &str = "Write to Alyosha here. Commands:\n/new start a fresh session\n/cancel stop the current turn\n/compact summarize older conversation\n/job <command> start a background job\n/tasks list scheduled tasks";
const PART_LIMIT: usize = 4000;
const EDIT_EVERY: Duration = Duration::from_secs(3);

/// The Bot API, with retries: it waits out a `429` and backs off on network
/// errors up to a minute, so no reply is dropped by a temporary error.
#[derive(Clone)]
pub struct Api {
    base: String,
    http: reqwest::Client,
}

impl Api {
    /// `base` is `https://api.telegram.org`, or a fake server's address.
    pub fn new(base: &str, token: &str) -> Self {
        Self {
            base: format!("{}/bot{token}", base.trim_end_matches('/')),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(60))
                .build()
                .expect("valid client settings"),
        }
    }

    pub async fn call(&self, method: &str, body: Value) -> Result<Value, String> {
        let mut backoff = Duration::from_secs(1);
        loop {
            let response = self
                .http
                .post(format!("{}/{method}", self.base))
                .json(&body)
                .send()
                .await;
            match response {
                Ok(response) => {
                    let status = response.status();
                    let reply: Value = response.json().await.unwrap_or(Value::Null);
                    if reply["ok"] == true {
                        return Ok(reply["result"].clone());
                    }
                    if status.as_u16() == 429 {
                        let wait = reply["parameters"]["retry_after"].as_u64().unwrap_or(5);
                        tracing::warn!(method, wait, "Telegram asked to retry later");
                        tokio::time::sleep(Duration::from_secs(wait)).await;
                        continue;
                    }
                    if status.is_server_error() {
                        tracing::warn!(
                            method,
                            status = status.as_u16(),
                            "Telegram server error; retrying"
                        );
                    } else {
                        let description = reply["description"]
                            .as_str()
                            .unwrap_or("unknown error")
                            .to_owned();
                        tracing::warn!(
                            method,
                            error_code = reply["error_code"].as_i64().unwrap_or_default(),
                            "Telegram refused a request"
                        );
                        return Err(description);
                    }
                }
                Err(error) => tracing::warn!(method, error = %error, "network error; retrying"),
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(60));
        }
    }

    pub async fn send(
        &self,
        chat: i64,
        text: &str,
        keyboard: Option<Value>,
    ) -> Result<i64, String> {
        let mut body = json!({"chat_id": chat, "text": text});
        if let Some(keyboard) = keyboard {
            body["reply_markup"] = keyboard;
        }
        self.call("sendMessage", body)
            .await
            .map(|message| message["message_id"].as_i64().unwrap_or_default())
    }

    /// Sends a reply split into parts of at most 4,000 characters.
    pub async fn send_long(&self, chat: i64, text: &str, keyboard: Option<Value>) -> Vec<i64> {
        let parts = split(text, PART_LIMIT);
        let last = parts.len() - 1;
        let mut ids = Vec::new();
        for (index, part) in parts.iter().enumerate() {
            let markup = if index == last {
                keyboard.clone()
            } else {
                None
            };
            if let Ok(id) = self.send(chat, part, markup).await {
                ids.push(id);
            }
        }
        ids
    }

    pub async fn edit(&self, chat: i64, message: i64, text: &str) {
        let _ = self
            .call(
                "editMessageText",
                json!({"chat_id": chat, "message_id": message, "text": text}),
            )
            .await;
    }
}

/// The bridge's settings, from `[telegram]`.
pub struct Settings {
    pub owner_id: i64,
    pub workspace: PathBuf,
    pub schedule: Schedule,
    pub timezone: Tz,
}

/// A **Working…** message and its throttled edits.
struct Working {
    message: Option<i64>,
    last_edit: Instant,
    pending: Option<String>,
}

/// An approval or question waiting for a button.
struct OpenRequest {
    number: u32,
    permission: Permission,
    message: Option<i64>,
    text: String,
}

/// Classifies an incoming update: the owner's private message, the owner's
/// button press, or something to drop.
pub enum Incoming {
    Text {
        message_id: i64,
        text: String,
    },
    NotText,
    Button {
        query_id: String,
        data: String,
    },
    Ignored {
        sender: Option<i64>,
        chat_type: String,
    },
}

pub fn classify(update: &Value, owner: i64) -> Incoming {
    if let Some(query) = update.get("callback_query") {
        let from = query["from"]["id"].as_i64();
        if from == Some(owner) {
            return Incoming::Button {
                query_id: query["id"].as_str().unwrap_or_default().to_owned(),
                data: query["data"].as_str().unwrap_or_default().to_owned(),
            };
        }
        return Incoming::Ignored {
            sender: from,
            chat_type: "callback".into(),
        };
    }
    let message = &update["message"];
    let sender = message["from"]["id"].as_i64();
    let chat_type = message["chat"]["type"]
        .as_str()
        .unwrap_or("unknown")
        .to_owned();
    if chat_type != "private" || sender != Some(owner) {
        return Incoming::Ignored { sender, chat_type };
    }
    match message["text"].as_str() {
        Some(text) => Incoming::Text {
            message_id: message["message_id"].as_i64().unwrap_or_default(),
            text: text.to_owned(),
        },
        None => Incoming::NotText,
    }
}

/// Runs the bridge until `alyosha acp` exits or SIGTERM. Returns the exit
/// status.
pub async fn run(
    api: Api,
    bridge: Bridge,
    mut events: mpsc::UnboundedReceiver<Event>,
    settings: Settings,
) -> u8 {
    let chat = settings.owner_id;
    if let Err(error) = bridge.initialize("alyosha-telegram").await {
        tracing::error!(error = %error, "initialize failed");
        return 1;
    }
    if let Err(error) = bridge.new_session(&settings.workspace).await {
        tracing::error!(error = %error, "session/new failed");
        let _ = api
            .send(chat, &format!("Alyosha could not start: {error}"), None)
            .await;
        return 1;
    }
    let commands = json!({"commands": [
        {"command": "new", "description": "Start a fresh session"},
        {"command": "cancel", "description": "Stop the current turn"},
        {"command": "compact", "description": "Summarize older conversation"},
        {"command": "job", "description": "Run a shell command as a background job"},
        {"command": "tasks", "description": "List scheduled tasks"},
    ]});
    let _ = api.call("setMyCommands", commands).await;

    let show_api = api.clone();
    let show: Arc<dyn Fn(&alyosha_schedule::DueTask) + Send + Sync> = Arc::new(move |task| {
        let api = show_api.clone();
        let text = task.shown.clone();
        tokio::spawn(async move {
            let _ = api.send(chat, &text, None).await;
        });
    });
    tokio::spawn(alyosha_bridge::runner::run(
        settings.schedule.clone(),
        "telegram".into(),
        settings.timezone,
        bridge.clone(),
        show,
    ));

    let (updates_sender, mut updates) = mpsc::unbounded_channel::<Value>();
    let poll_api = api.clone();
    tokio::spawn(async move {
        let mut offset = 0i64;
        loop {
            let result = poll_api
                .call(
                    "getUpdates",
                    json!({"offset": offset, "timeout": 30, "allowed_updates": ["message", "callback_query"]}),
                )
                .await;
            let Ok(Value::Array(batch)) = result else {
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            };
            for update in batch {
                offset = offset.max(update["update_id"].as_i64().unwrap_or_default() + 1);
                if updates_sender.send(update).is_err() {
                    return;
                }
            }
        }
    });

    let mut working: HashMap<String, Working> = HashMap::new();
    let mut requests: HashMap<u64, OpenRequest> = HashMap::new();
    let mut next_request = 1u32;
    let mut next_job = 1u64;
    let mut typing = tokio::time::interval(Duration::from_secs(5));
    let mut flush = tokio::time::interval(Duration::from_millis(500));
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("a SIGTERM handler");

    loop {
        tokio::select! {
            update = updates.recv() => {
                let Some(update) = update else { continue };
                match classify(&update, chat) {
                    Incoming::Ignored { sender, chat_type } => {
                        tracing::info!(sender = sender.unwrap_or_default(), chat_type = %chat_type, "dropped a message from someone else");
                    }
                    Incoming::NotText => { let _ = api.send(chat, "Only text messages work for now.", None).await; }
                    Incoming::Button { query_id, data } => {
                        let _ = api.call("answerCallbackQuery", json!({"callback_query_id": query_id})).await;
                        let Some((number, option)) = data.strip_prefix('r').and_then(|rest| rest.split_once(':')) else { continue };
                        let found = requests.iter().find(|(_, open)| open.number.to_string() == number).map(|(id, _)| *id);
                        let Some(id) = found else { continue };
                        let open = requests.remove(&id).expect("found above");
                        let name = open.permission.options.iter().find(|(option_id, _)| option_id == option).map(|(_, name)| name.clone());
                        let Some(name) = name else { continue };
                        bridge.forget_permission(&open.permission.tool_call_id);
                        bridge.answer(id, Some(option));
                        if let Some(message) = open.message {
                            api.edit(chat, message, &format!("{}\n\nAnswered: {name}", open.text)).await;
                        }
                    }
                    Incoming::Text { message_id, text } => {
                        let trimmed = text.trim();
                        match trimmed {
                            "/start" => { let _ = api.send(chat, HELP, None).await; }
                            "/new" => {
                                match bridge.new_session(&settings.workspace).await {
                                    Ok(_) => { let _ = api.send(chat, "New session started.", None).await; }
                                    Err(error) => { let _ = api.send(chat, &format!("Failed: {error}"), None).await; }
                                }
                            }
                            "/cancel" => {
                                let open: Vec<u64> = requests.keys().copied().collect();
                                bridge.cancel_with_requests(&open);
                                for (_, open) in requests.drain() {
                                    if let Some(message) = open.message {
                                        api.edit(chat, message, &format!("{}\n\nCancelled.", open.text)).await;
                                    }
                                }
                            }
                            "/tasks" => {
                                let reply = match settings.schedule.read() {
                                    Ok(tasks) => alyosha_schedule::tasks_text(&tasks, Utc::now().with_timezone(&settings.timezone)),
                                    Err(error) => error.to_string(),
                                };
                                let _ = api.send_long(chat, &reply, None).await;
                            }
                            _ => bridge.prompt(&format!("m{message_id}"), &text),
                        }
                    }
                }
            }
            event = events.recv() => {
                let Some(event) = event else { return 1 };
                match event {
                    Event::Tools { key, tools } => {
                        let text = working_text(&tools);
                        let entry = working.entry(key).or_insert(Working { message: None, last_edit: Instant::now(), pending: None });
                        if entry.message.is_none() {
                            entry.message = api.send(chat, &text, None).await.ok();
                            entry.last_edit = Instant::now();
                        } else {
                            entry.pending = Some(text);
                        }
                    }
                    Event::Finished(finished) => {
                        if let Some(view) = working.remove(&finished.key)
                            && let (Some(message), Some(text)) = (view.message, view.pending)
                        {
                            api.edit(chat, message, &text).await;
                        }
                        api.send_long(chat, &finished.reply(), None).await;
                    }
                    Event::Permission(permission) => {
                        let number = next_request;
                        next_request += 1;
                        let text = if permission.body.is_empty() { permission.title.clone() } else { format!("{}\n\n{}", permission.title, permission.body) };
                        let rows: Vec<Value> = permission.options.iter().map(|(id, name)| json!([{"text": name, "callback_data": format!("r{number}:{id}")}])).collect();
                        let ids = api.send_long(chat, &text, Some(json!({"inline_keyboard": rows}))).await;
                        let request = permission.request;
                        if ids.is_empty() {
                            // Undeliverable: Alyosha treats a cancelled answer as such.
                            bridge.answer(request, None);
                            continue;
                        }
                        let last_part = split(&text, PART_LIMIT).pop().unwrap_or_default();
                        requests.insert(request, OpenRequest { number, permission, message: ids.last().copied(), text: last_part });
                    }
                    Event::PermissionExpired { tool_call_id } => {
                        let found = requests.iter().find(|(_, open)| open.permission.tool_call_id == tool_call_id).map(|(id, _)| *id);
                        if let Some(open) = found.and_then(|id| requests.remove(&id))
                            && let Some(message) = open.message
                        {
                            api.edit(chat, message, &format!("{}\n\nExpired — not approved.", open.text)).await;
                        }
                    }
                    Event::JobFinished { text, .. } => {
                        let _ = api.send(chat, "⚙ Job finished:", None).await;
                        bridge.prompt(&format!("job{next_job}"), &text);
                        next_job += 1;
                    }
                    Event::Exited => {
                        let _ = api.send(chat, "Alyosha stopped unexpectedly. Restarting with a new session.", None).await;
                        return 1;
                    }
                }
            }
            _ = typing.tick() => {
                if !bridge.open_keys().is_empty() {
                    let _ = api.call("sendChatAction", json!({"chat_id": chat, "action": "typing"})).await;
                }
            }
            _ = flush.tick() => {
                for view in working.values_mut() {
                    if view.last_edit.elapsed() >= EDIT_EVERY
                        && let (Some(message), Some(text)) = (view.message, view.pending.take())
                    {
                        api.edit(chat, message, &text).await;
                        view.last_edit = Instant::now();
                    }
                }
            }
            _ = terminate.recv() => {
                bridge.shutdown().await;
                return 0;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_owner_in_a_private_chat_gets_through() {
        let owner = 42;
        let private = json!({"message": {"message_id": 1, "from": {"id": 42}, "chat": {"type": "private"}, "text": "hi"}});
        assert!(matches!(classify(&private, owner), Incoming::Text { text, .. } if text == "hi"));
        let stranger = json!({"message": {"message_id": 2, "from": {"id": 7}, "chat": {"type": "private"}, "text": "hi"}});
        assert!(matches!(
            classify(&stranger, owner),
            Incoming::Ignored {
                sender: Some(7),
                ..
            }
        ));
        let group = json!({"message": {"message_id": 3, "from": {"id": 42}, "chat": {"type": "group"}, "text": "hi"}});
        assert!(
            matches!(classify(&group, owner), Incoming::Ignored { chat_type, .. } if chat_type == "group")
        );
        let photo = json!({"message": {"message_id": 4, "from": {"id": 42}, "chat": {"type": "private"}, "photo": []}});
        assert!(matches!(classify(&photo, owner), Incoming::NotText));
        let press = json!({"callback_query": {"id": "q", "from": {"id": 42}, "data": "r1:0"}});
        assert!(matches!(classify(&press, owner), Incoming::Button { data, .. } if data == "r1:0"));
        let other_press = json!({"callback_query": {"id": "q", "from": {"id": 9}, "data": "r1:0"}});
        assert!(matches!(
            classify(&other_press, owner),
            Incoming::Ignored { .. }
        ));
    }
}
