//! `alyosha-telegram`: relays private messages between the owner's Telegram
//! account and one `alyosha acp` child.
//!
//! It long-polls the Bot API, so it opens no ports. In private chats it
//! answers only the owner. In groups it answers anyone who mentions the bot,
//! through that group's session: a full one in a trusted group, a guest one
//! elsewhere. See `docs/develop/TelegramBridge.md`.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use alyosha_bridge::{
    Bridge, Event, Permission,
    guests::{GroupSpawners, GuestEvent, GuestPool, IDLE_LIMIT},
    split, working_text,
};
use alyosha_schedule::Schedule;
use chrono::Utc;
use chrono_tz::Tz;
use serde_json::{Value, json};
use tokio::sync::mpsc;

pub const HELP: &str = "Write to Alyosha here. Commands:\n/new start a fresh session\n/cancel stop the current turn\n/compact summarize older conversation\n/job <command> start a background job\n/tasks list scheduled tasks";
const PART_LIMIT: usize = 4000;
const EDIT_EVERY: Duration = Duration::from_secs(3);
/// How much of a replied-to message a group prompt carries.
const QUOTE_LIMIT: usize = 500;
pub const TEXT_ONLY: &str = "Only text messages work for now.";

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

    /// Sends a reply in a group, split like any reply, each part answering
    /// `reply_to`. Failures are logged by `call` and dropped.
    pub async fn send_reply(&self, chat: i64, text: &str, reply_to: i64) {
        for part in split(text, PART_LIMIT) {
            let body = json!({
                "chat_id": chat,
                "text": part,
                "reply_parameters": {"message_id": reply_to, "allow_sending_without_reply": true},
            });
            if self.call("sendMessage", body).await.is_err() {
                tracing::warn!(chat, "a group reply failed; dropped");
                return;
            }
        }
    }

    /// Sends a model reply with its Markdown rendered: each part as
    /// Telegram HTML, or as plain text if Telegram refuses the HTML. In a
    /// group, each part answers `reply_to`. Returns the sent message IDs.
    pub async fn send_rendered(&self, chat: i64, text: &str, reply_to: Option<i64>) -> Vec<i64> {
        let mut ids = Vec::new();
        for part in alyosha_bridge::markdown::split_markdown(text, PART_LIMIT) {
            let mut body = json!({
                "chat_id": chat,
                "text": alyosha_bridge::markdown::to_telegram_html(&part),
                "parse_mode": "HTML",
                "link_preview_options": {"is_disabled": true},
            });
            if let Some(message) = reply_to {
                body["reply_parameters"] =
                    json!({"message_id": message, "allow_sending_without_reply": true});
            }
            let sent = match self.call("sendMessage", body.clone()).await {
                Ok(sent) => Ok(sent),
                Err(error) => {
                    tracing::warn!(chat, error = %error, "Telegram refused the formatted reply; sending plain text");
                    body["text"] = json!(alyosha_bridge::markdown::to_plain(&part));
                    if let Some(map) = body.as_object_mut() {
                        map.remove("parse_mode");
                    }
                    self.call("sendMessage", body).await
                }
            };
            match sent {
                Ok(message) => ids.push(message["message_id"].as_i64().unwrap_or_default()),
                Err(_) if reply_to.is_some() => {
                    tracing::warn!(chat, "a group reply failed; dropped");
                    break;
                }
                Err(_) => {}
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
    /// Starts groups' sessions, and names the trusted groups.
    pub groups: GroupSpawners,
}

/// The bot's own account, from `getMe`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bot {
    pub id: i64,
    pub username: String,
}

/// A **Working…** message and its throttled edits.
struct Working {
    message: Option<i64>,
    last_edit: Instant,
    pending: Option<String>,
}

/// An approval or question waiting for a button.
struct OpenRequest {
    /// The trusted group whose session asked, or `None` for the owner's.
    group: Option<String>,
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
    /// A group message that mentions the bot or replies to it. `text` is
    /// `None` for a photo, file, or other message without text.
    Group {
        chat: i64,
        message_id: i64,
        sender: i64,
        name: String,
        text: Option<String>,
    },
    /// A group message not addressed to the bot.
    Unaddressed,
}

/// The sender's display name: first and last name, or the username.
fn display_name(user: &Value) -> String {
    let full = [user["first_name"].as_str(), user["last_name"].as_str()]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ");
    if !full.trim().is_empty() {
        return full.trim().to_owned();
    }
    user["username"].as_str().unwrap_or("someone").to_owned()
}

/// A group message for the bot: mentioned by username or `text_mention`, or
/// a reply to the bot. The mentions are removed. A reply to someone else
/// brings the start of their message along.
fn group_message(message: &Value, bot: &Bot) -> Incoming {
    let (text, entities, has_text) = match message["text"].as_str() {
        Some(text) => (text, &message["entities"], true),
        None => (
            message["caption"].as_str().unwrap_or_default(),
            &message["caption_entities"],
            false,
        ),
    };
    // Entity offsets count UTF-16 code units.
    let units: Vec<u16> = text.encode_utf16().collect();
    let handle = format!("@{}", bot.username);
    let mut mentions: Vec<(usize, usize)> = entities
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|entity| {
            let start = usize::try_from(entity["offset"].as_u64()?).ok()?;
            let end = start + usize::try_from(entity["length"].as_u64()?).ok()?;
            let slice = units.get(start..end)?;
            let mentioned = match entity["type"].as_str() {
                Some("mention") => String::from_utf16_lossy(slice).eq_ignore_ascii_case(&handle),
                Some("text_mention") => entity["user"]["id"].as_i64() == Some(bot.id),
                _ => false,
            };
            mentioned.then_some((start, end))
        })
        .collect();
    let replied = &message["reply_to_message"];
    let to_bot = replied["from"]["id"].as_i64() == Some(bot.id);
    if mentions.is_empty() && !to_bot {
        return Incoming::Unaddressed;
    }
    mentions.sort_unstable();
    let mut kept = Vec::with_capacity(units.len());
    let mut position = 0;
    for (start, end) in mentions {
        if start >= position {
            kept.extend_from_slice(&units[position..start]);
            position = end;
        }
    }
    kept.extend_from_slice(&units[position.min(units.len())..]);
    let mut body = String::from_utf16_lossy(&kept).trim().to_owned();
    if !to_bot && let Some(quoted) = replied["text"].as_str() {
        let start: String = quoted.chars().take(QUOTE_LIMIT).collect();
        body = format!(
            "(Replying to {}: \"{start}\")\n{body}",
            display_name(&replied["from"])
        );
    }
    Incoming::Group {
        chat: message["chat"]["id"].as_i64().unwrap_or_default(),
        message_id: message["message_id"].as_i64().unwrap_or_default(),
        sender: message["from"]["id"].as_i64().unwrap_or_default(),
        name: display_name(&message["from"]),
        text: has_text.then_some(body),
    }
}

pub fn classify(update: &Value, owner: i64, bot: &Bot) -> Incoming {
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
    if chat_type == "group" || chat_type == "supergroup" {
        return group_message(message, bot);
    }
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
    if let Err(error) = bridge
        .open_session(&settings.workspace, "owner:telegram")
        .await
    {
        tracing::error!(error = %error, "the session could not be opened");
        let _ = api
            .send(chat, &format!("Alyosha could not start: {error}"), None)
            .await;
        return 1;
    }
    let bot = match api.call("getMe", json!({})).await {
        Ok(me) => Bot {
            id: me["id"].as_i64().unwrap_or_default(),
            username: me["username"].as_str().unwrap_or_default().to_owned(),
        },
        Err(error) => {
            tracing::error!(error = %error, "getMe failed");
            return 1;
        }
    };
    let (mut pool, mut guest_events) = GuestPool::new(
        settings.groups.clone(),
        "alyosha-telegram",
        settings.workspace.clone(),
    );
    // Each open group prompt's chat and the message it answers, if any.
    let mut group_replies: HashMap<String, (i64, Option<i64>)> = HashMap::new();
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
    // Open requests by their button number, from every session.
    let mut requests: HashMap<u32, OpenRequest> = HashMap::new();
    let mut next_request = 1u32;
    let mut next_job = 1u64;
    let mut typing = tokio::time::interval(Duration::from_secs(5));
    let mut flush = tokio::time::interval(Duration::from_millis(500));
    let mut idle_check = tokio::time::interval(Duration::from_secs(60));
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("a SIGTERM handler");

    loop {
        tokio::select! {
            update = updates.recv() => {
                let Some(update) = update else { continue };
                match classify(&update, chat, &bot) {
                    Incoming::Unaddressed => {}
                    Incoming::Group { chat: group, message_id, sender, name, text } => {
                        let Some(text) = text else {
                            api.send_reply(group, TEXT_ONLY, message_id).await;
                            continue;
                        };
                        let group_id = group.to_string();
                        tracing::info!(group, trusted = pool.is_trusted(&group_id), "a group mention");
                        let key = format!("g{group}:{message_id}");
                        group_replies.insert(key.clone(), (group, Some(message_id)));
                        let user = format!("telegram-{sender}");
                        if let Err(error) = pool.prompt(&group_id, &key, &text, &user, &name).await {
                            tracing::warn!(group, error = %error, "the group session did not start");
                            group_replies.remove(&key);
                        }
                    }
                    Incoming::Ignored { sender, chat_type } => {
                        tracing::info!(sender = sender.unwrap_or_default(), chat_type = %chat_type, "dropped a message from someone else");
                    }
                    Incoming::NotText => { let _ = api.send(chat, TEXT_ONLY, None).await; }
                    Incoming::Button { query_id, data } => {
                        let _ = api.call("answerCallbackQuery", json!({"callback_query_id": query_id})).await;
                        let Some((number, option)) = data.strip_prefix('r').and_then(|rest| rest.split_once(':')) else { continue };
                        let Some(number) = number.parse::<u32>().ok() else { continue };
                        let Some(open) = requests.get(&number) else { continue };
                        let name = open.permission.options.iter().find(|(option_id, _)| option_id == option).map(|(_, name)| name.clone());
                        let Some(name) = name else { continue };
                        let open = requests.remove(&number).expect("found above");
                        let asker = match &open.group {
                            None => Some(&bridge),
                            Some(group) => pool.bridge(group),
                        };
                        if let Some(asker) = asker {
                            asker.forget_permission(&open.permission.tool_call_id);
                            asker.answer(open.permission.request, Some(option));
                        }
                        if let Some(message) = open.message {
                            api.edit(chat, message, &format!("{}\n\nAnswered: {name}", open.text)).await;
                        }
                    }
                    Incoming::Text { message_id, text } => {
                        let trimmed = text.trim();
                        match trimmed {
                            "/start" => { let _ = api.send(chat, HELP, None).await; }
                            "/new" => {
                                match bridge.new_session(&settings.workspace, "owner:telegram").await {
                                    Ok(_) => { let _ = api.send(chat, "New session started.", None).await; }
                                    Err(error) => { let _ = api.send(chat, &format!("Failed: {error}"), None).await; }
                                }
                            }
                            "/cancel" => {
                                // Only the private session's; groups' requests stay open.
                                let numbers: Vec<u32> = requests.iter().filter(|(_, open)| open.group.is_none()).map(|(number, _)| *number).collect();
                                let ours: Vec<OpenRequest> = numbers.iter().filter_map(|number| requests.remove(number)).collect();
                                let ids: Vec<u64> = ours.iter().map(|open| open.permission.request).collect();
                                bridge.cancel_with_requests(&ids);
                                for open in ours {
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
                        api.send_rendered(chat, &finished.reply(), None).await;
                    }
                    Event::Permission(permission) => {
                        let number = next_request;
                        next_request += 1;
                        let request = permission.request;
                        match ask_owner(&api, chat, number, None, permission).await {
                            Some(open) => { requests.insert(number, open); }
                            // Undeliverable: Alyosha treats a cancelled answer as such.
                            None => bridge.answer(request, None),
                        }
                    }
                    Event::PermissionExpired { tool_call_id } => {
                        expire(&api, chat, &mut requests, None, &tool_call_id).await;
                    }
                    Event::JobFinished { text, .. } => {
                        let _ = api.send(chat, "⚙ Job finished:", None).await;
                        bridge.prompt(&format!("job{next_job}"), &text);
                        next_job += 1;
                    }
                    Event::Exited => {
                        let _ = api.send(chat, "Alyosha stopped unexpectedly. Restarting.", None).await;
                        return 1;
                    }
                }
            }
            guest = guest_events.recv() => {
                let Some(GuestEvent { group, guest, event }) = guest else { continue };
                match event {
                    Event::Finished(finished) => {
                        pool.touch(&group);
                        if let Some((group_chat, message_id)) = group_replies.remove(&finished.key) {
                            api.send_rendered(group_chat, &finished.reply(), message_id).await;
                        }
                    }
                    Event::Exited => pool.exited(&group, guest),
                    // Only a trusted group's session asks the owner.
                    Event::Permission(permission) if pool.is_trusted(&group) => {
                        let number = next_request;
                        next_request += 1;
                        let request = permission.request;
                        match ask_owner(&api, chat, number, Some(&group), permission).await {
                            Some(open) => { requests.insert(number, open); }
                            None => if let Some(asker) = pool.bridge(&group) { asker.answer(request, None); },
                        }
                    }
                    Event::PermissionExpired { tool_call_id } => {
                        expire(&api, chat, &mut requests, Some(&group), &tool_call_id).await;
                    }
                    Event::JobFinished { text, .. } if pool.is_trusted(&group) => {
                        let Ok(group_chat) = group.parse::<i64>() else { continue };
                        let _ = api.send(group_chat, "⚙ Job finished:", None).await;
                        let key = format!("gjob{next_job}");
                        next_job += 1;
                        group_replies.insert(key.clone(), (group_chat, None));
                        if !pool.prompt_job(&group, &key, &text) {
                            group_replies.remove(&key);
                        }
                    }
                    // Guest sessions have no approver or jobs, and no group
                    // gets tool calls shown.
                    _ => {}
                }
            }
            _ = typing.tick() => {
                if !bridge.open_keys().is_empty() {
                    let _ = api.call("sendChatAction", json!({"chat_id": chat, "action": "typing"})).await;
                }
                for group in pool.busy_groups() {
                    let _ = api.call("sendChatAction", json!({"chat_id": group.parse::<i64>().unwrap_or_default(), "action": "typing"})).await;
                }
            }
            _ = idle_check.tick() => pool.stop_idle(IDLE_LIMIT).await,
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
                pool.shutdown().await;
                bridge.shutdown().await;
                return 0;
            }
        }
    }
}

/// Sends an approval or question to the owner's private chat, with a
/// keyboard numbered `number`, marked with the group that asked. Returns
/// `None` if it cannot be sent; the caller then answers it `cancelled`.
async fn ask_owner(
    api: &Api,
    chat: i64,
    number: u32,
    group: Option<&str>,
    permission: Permission,
) -> Option<OpenRequest> {
    let mut text = if permission.body.is_empty() {
        permission.title.clone()
    } else {
        format!("{}\n\n{}", permission.title, permission.body)
    };
    if let Some(group) = group {
        text = format!("In group {group}:\n\n{text}");
    }
    let rows: Vec<Value> = permission
        .options
        .iter()
        .map(|(id, name)| json!([{"text": name, "callback_data": format!("r{number}:{id}")}]))
        .collect();
    let ids = api
        .send_long(chat, &text, Some(json!({"inline_keyboard": rows})))
        .await;
    if ids.is_empty() {
        return None;
    }
    let last_part = split(&text, PART_LIMIT).pop().unwrap_or_default();
    Some(OpenRequest {
        group: group.map(str::to_owned),
        permission,
        message: ids.last().copied(),
        text: last_part,
    })
}

/// Marks the open request of `group`'s session for `tool_call_id` expired.
async fn expire(
    api: &Api,
    chat: i64,
    requests: &mut HashMap<u32, OpenRequest>,
    group: Option<&str>,
    tool_call_id: &str,
) {
    let found = requests
        .iter()
        .find(|(_, open)| {
            open.group.as_deref() == group && open.permission.tool_call_id == tool_call_id
        })
        .map(|(number, _)| *number);
    if let Some(open) = found.and_then(|number| requests.remove(&number))
        && let Some(message) = open.message
    {
        api.edit(
            chat,
            message,
            &format!("{}\n\nExpired — not approved.", open.text),
        )
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bot() -> Bot {
        Bot {
            id: 999,
            username: "AlyoshaBot".into(),
        }
    }

    #[test]
    fn group_messages_count_only_when_addressed_to_the_bot() {
        let group = |text: &str, entities: Value, reply: Value| json!({"message": {"message_id": 5, "from": {"id": 7, "first_name": "Bob", "last_name": "B"}, "chat": {"id": -100, "type": "supergroup"}, "text": text, "entities": entities, "reply_to_message": reply}});
        let plain = group("hello all", json!([]), Value::Null);
        assert!(matches!(
            classify(&plain, 42, &bot()),
            Incoming::Unaddressed
        ));
        // "é" is one UTF-16 unit and "😀" two, so offsets count units.
        let text = "😀é @alyoshabot what is 2+2?";
        let mention = group(
            text,
            json!([{"type": "mention", "offset": 4, "length": 11}]),
            Value::Null,
        );
        match classify(&mention, 42, &bot()) {
            Incoming::Group {
                chat,
                message_id,
                sender,
                name,
                text,
            } => {
                assert_eq!((chat, message_id, sender), (-100, 5, 7));
                assert_eq!(name, "Bob B");
                assert_eq!(text.as_deref(), Some("😀é  what is 2+2?"));
            }
            _ => panic!("a mention is for the bot"),
        }
        let other = group(
            "@someone else",
            json!([{"type": "mention", "offset": 0, "length": 8}]),
            Value::Null,
        );
        assert!(matches!(
            classify(&other, 42, &bot()),
            Incoming::Unaddressed
        ));
        let reply = group(
            "and why?",
            json!([]),
            json!({"from": {"id": 999}, "text": "4"}),
        );
        assert!(
            matches!(classify(&reply, 42, &bot()), Incoming::Group { text: Some(text), .. } if text == "and why?")
        );
        let quoted = group(
            "@AlyoshaBot is this true?",
            json!([{"type": "mention", "offset": 0, "length": 11}]),
            json!({"from": {"id": 8, "first_name": "Anna"}, "text": "The moon is cheese."}),
        );
        assert!(
            matches!(classify(&quoted, 42, &bot()), Incoming::Group { text: Some(text), .. }
            if text == "(Replying to Anna: \"The moon is cheese.\")\nis this true?")
        );
        let photo = json!({"message": {"message_id": 6, "from": {"id": 7}, "chat": {"id": -100, "type": "group"}, "photo": [], "caption": "@AlyoshaBot look", "caption_entities": [{"type": "mention", "offset": 0, "length": 11}]}});
        assert!(matches!(
            classify(&photo, 42, &bot()),
            Incoming::Group { text: None, .. }
        ));
    }

    #[test]
    fn only_the_owner_in_a_private_chat_gets_through() {
        let owner = 42;
        let private = json!({"message": {"message_id": 1, "from": {"id": 42}, "chat": {"type": "private"}, "text": "hi"}});
        assert!(
            matches!(classify(&private, owner, &bot()), Incoming::Text { text, .. } if text == "hi")
        );
        let stranger = json!({"message": {"message_id": 2, "from": {"id": 7}, "chat": {"type": "private"}, "text": "hi"}});
        assert!(matches!(
            classify(&stranger, owner, &bot()),
            Incoming::Ignored {
                sender: Some(7),
                ..
            }
        ));
        let group = json!({"message": {"message_id": 3, "from": {"id": 42}, "chat": {"type": "group"}, "text": "hi"}});
        assert!(
            matches!(classify(&group, owner, &bot()), Incoming::Unaddressed),
            "the owner is a guest in groups too"
        );
        let channel = json!({"message": {"message_id": 3, "from": {"id": 42}, "chat": {"type": "channel"}, "text": "hi"}});
        assert!(matches!(
            classify(&channel, owner, &bot()),
            Incoming::Ignored { .. }
        ));
        let photo = json!({"message": {"message_id": 4, "from": {"id": 42}, "chat": {"type": "private"}, "photo": []}});
        assert!(matches!(classify(&photo, owner, &bot()), Incoming::NotText));
        let press = json!({"callback_query": {"id": "q", "from": {"id": 42}, "data": "r1:0"}});
        assert!(
            matches!(classify(&press, owner, &bot()), Incoming::Button { data, .. } if data == "r1:0")
        );
        let other_press = json!({"callback_query": {"id": "q", "from": {"id": 9}, "data": "r1:0"}});
        assert!(matches!(
            classify(&other_press, owner, &bot()),
            Incoming::Ignored { .. }
        ));
    }
}
