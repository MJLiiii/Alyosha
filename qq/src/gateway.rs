//! QQ's API and WebSocket gateway, and the bridge's main loop.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use alyosha_bridge::{
    Bridge, Event, Permission,
    guests::{GroupCommand, GroupSpawners, GuestEvent, GuestPool, IDLE_LIMIT},
};
use alyosha_schedule::Schedule;
use chrono::Utc;
use chrono_tz::Tz;
use futures_util::{SinkExt, StreamExt, future::BoxFuture};
use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc};
use tokio_tungstenite::tungstenite::Message;

use crate::{
    MarkdownRefused, ReplyTo, Seen, SendError, Sender, deliver, deliver_reply,
    media::{Chat, send_files},
    parse_answer, parse_token, permission_text, reply_parts,
};

/// `GROUP_AND_C2C_EVENT`.
pub const INTENTS: u64 = 1 << 25;
/// Close codes that mean the bot may not connect at all.
const FATAL_CLOSES: [u16; 4] = [4013, 4014, 4914, 4915];

#[derive(Debug)]
pub enum GatewayError {
    Token(String),
    Fatal(String),
}

impl std::fmt::Display for GatewayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GatewayError::Token(message) => write!(f, "the access token request failed: {message}"),
            GatewayError::Fatal(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for GatewayError {}

/// QQ's HTTP API with a refreshed access token.
pub struct QqApi {
    http: reqwest::Client,
    api: String,
    token_url: String,
    app_id: String,
    secret: String,
    token: Mutex<Option<(String, Instant)>>,
}

impl QqApi {
    pub fn new(api: &str, token_url: &str, app_id: &str, secret: &str) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .expect("valid client settings"),
            api: api.trim_end_matches('/').to_owned(),
            token_url: token_url.to_owned(),
            app_id: app_id.to_owned(),
            secret: secret.to_owned(),
            token: Mutex::new(None),
        }
    }

    /// The access token, renewed in the last 60 seconds of its life.
    pub async fn token(&self) -> Result<String, GatewayError> {
        let mut token = self.token.lock().await;
        if let Some((value, expires)) = token.as_ref()
            && Instant::now() + Duration::from_secs(60) < *expires
        {
            return Ok(value.clone());
        }
        let reply: Value = self
            .http
            .post(&self.token_url)
            .json(&json!({"appId": self.app_id, "clientSecret": self.secret}))
            .send()
            .await
            .map_err(|error| GatewayError::Token(error.to_string()))?
            .json()
            .await
            .map_err(|error| GatewayError::Token(error.to_string()))?;
        let (value, life) = parse_token(&reply)?;
        *token = Some((value.clone(), Instant::now() + life));
        Ok(value)
    }

    async fn get(&self, path: &str) -> Result<Value, String> {
        let token = self.token().await.map_err(|error| error.to_string())?;
        self.http
            .get(format!("{}{path}", self.api))
            .header("Authorization", format!("QQBot {token}"))
            .send()
            .await
            .map_err(|error| error.to_string())?
            .json()
            .await
            .map_err(|error| error.to_string())
    }

    /// Sends one message to a user.
    pub async fn send_to(&self, openid: &str, body: Value) -> Result<(), SendError> {
        self.send_message(&format!("/v2/users/{openid}/messages"), body)
            .await
    }

    /// Sends one message to a group.
    pub async fn send_group(&self, group_openid: &str, body: Value) -> Result<(), SendError> {
        self.send_message(&format!("/v2/groups/{group_openid}/messages"), body)
            .await
    }

    /// The HTTP client, for downloads.
    pub(crate) fn http(&self) -> &reqwest::Client {
        &self.http
    }

    async fn send_message(&self, path: &str, body: Value) -> Result<(), SendError> {
        self.post(path, body).await.map(|_| ())
    }

    /// Posts JSON to the API and returns QQ's reply.
    pub(crate) async fn post(&self, path: &str, body: Value) -> Result<Value, SendError> {
        let token = self.token().await.map_err(|error| SendError {
            code: -1,
            message: error.to_string(),
        })?;
        let response = self
            .http
            .post(format!("{}{path}", self.api))
            .header("Authorization", format!("QQBot {token}"))
            .json(&body)
            .send()
            .await
            .map_err(|error| SendError {
                code: -1,
                message: error.to_string(),
            })?;
        let status = response.status();
        let reply: Value = response.json().await.unwrap_or(Value::Null);
        let code = reply["err_code"]
            .as_i64()
            .or(reply["code"].as_i64())
            .unwrap_or(0);
        if status.is_success() && code == 0 {
            return Ok(reply);
        }
        tracing::warn!(
            code,
            trace_id = reply["trace_id"].as_str().unwrap_or_default(),
            "QQ refused a message"
        );
        Err(SendError {
            code,
            message: reply["message"].as_str().unwrap_or_default().to_owned(),
        })
    }
}

/// Sends to the owner.
pub struct ToOwner {
    pub api: Arc<QqApi>,
    pub openid: String,
}

impl Sender for ToOwner {
    fn send(&self, body: Value) -> BoxFuture<'_, Result<(), SendError>> {
        Box::pin(self.api.send_to(&self.openid, body))
    }
}

/// Sends to a group.
pub struct ToGroup {
    pub api: Arc<QqApi>,
    pub group_openid: String,
}

impl Sender for ToGroup {
    fn send(&self, body: Value) -> BoxFuture<'_, Result<(), SendError>> {
        Box::pin(self.api.send_group(&self.group_openid, body))
    }
}

/// A message from the gateway.
#[derive(Debug, Clone)]
pub enum QqMessage {
    C2c(C2cMessage),
    Group(GroupMessage),
}

/// A group message that @-mentions the bot; QQ has removed the @.
#[derive(Debug, Clone)]
pub struct GroupMessage {
    pub id: String,
    pub group_openid: String,
    pub member_openid: String,
    /// The member's nickname, or `QQ user` when QQ sends none.
    pub name: String,
    pub content: String,
    /// `d.attachments` as QQ sent them.
    pub attachments: Vec<Value>,
}

/// A private message from the gateway.
#[derive(Debug, Clone)]
pub struct C2cMessage {
    pub id: String,
    pub openid: String,
    pub content: String,
    /// `d.attachments` as QQ sent them.
    pub attachments: Vec<Value>,
}

fn attachments_of(d: &Value) -> Vec<Value> {
    d["attachments"].as_array().cloned().unwrap_or_default()
}

/// Keeps a WebSocket connection to the gateway and forwards private
/// messages. Returns only on a fatal error.
pub async fn listen(api: Arc<QqApi>, messages: mpsc::UnboundedSender<QqMessage>) -> GatewayError {
    let mut session: Option<(String, Option<u64>)> = None;
    let mut backoff = Duration::from_secs(1);
    loop {
        match connect_once(&api, &messages, &mut session).await {
            Ok(()) => backoff = Duration::from_secs(1),
            Err(Stop::Fatal(error)) => return error,
            Err(Stop::Reidentify) => {
                tracing::info!("the QQ gateway session ended; identifying again");
                session = None;
            }
            Err(Stop::Wait) => tokio::time::sleep(Duration::from_secs(10)).await,
            Err(Stop::Retry(reason)) => {
                tracing::warn!(reason = %reason, "the QQ gateway connection failed; retrying");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(60));
            }
        }
    }
}

enum Stop {
    Fatal(GatewayError),
    Reidentify,
    Wait,
    Retry(String),
}

async fn connect_once(
    api: &QqApi,
    messages: &mpsc::UnboundedSender<QqMessage>,
    session: &mut Option<(String, Option<u64>)>,
) -> Result<(), Stop> {
    let url = api.get("/gateway").await.map_err(Stop::Retry)?["url"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| Stop::Retry("no gateway url".into()))?;
    let (socket, _) = tokio_tungstenite::connect_async(url.as_str())
        .await
        .map_err(|error| Stop::Retry(error.to_string()))?;
    let (mut write, mut read) = socket.split();
    let mut interval = Duration::from_secs(40);
    let mut heartbeat = tokio::time::interval(interval);
    let mut last_seq = session.as_ref().and_then(|(_, seq)| *seq);
    loop {
        tokio::select! {
            frame = read.next() => {
                let frame = match frame {
                    Some(Ok(frame)) => frame,
                    Some(Err(error)) => return Err(Stop::Retry(error.to_string())),
                    None => return Err(Stop::Retry("closed".into())),
                };
                let payload: Value = match frame {
                    Message::Text(text) => serde_json::from_str(text.as_str()).unwrap_or(Value::Null),
                    Message::Close(close) => {
                        let code = close.map(|frame| u16::from(frame.code)).unwrap_or(1000);
                        return Err(match code {
                            4006 | 4007 => Stop::Reidentify,
                            4008 => Stop::Wait,
                            code if FATAL_CLOSES.contains(&code) => Stop::Fatal(GatewayError::Fatal(format!(
                                "QQ refused the connection with close code {code}: the intent, permission, or bot is not allowed"
                            ))),
                            code => Stop::Retry(format!("closed with {code}")),
                        });
                    }
                    _ => continue,
                };
                if let Some(seq) = payload["s"].as_u64() {
                    last_seq = Some(seq);
                    if let Some((_, stored)) = session.as_mut() {
                        *stored = Some(seq);
                    }
                }
                match payload["op"].as_u64() {
                    Some(10) => {
                        interval = Duration::from_millis(payload["d"]["heartbeat_interval"].as_u64().unwrap_or(40_000));
                        heartbeat = tokio::time::interval_at(tokio::time::Instant::now() + interval, interval);
                        let token = api.token().await.map_err(|error| Stop::Retry(error.to_string()))?;
                        let hello = match session.as_ref() {
                            Some((session_id, seq)) => json!({"op": 6, "d": {"token": format!("QQBot {token}"), "session_id": session_id, "seq": seq}}),
                            None => json!({"op": 2, "d": {"token": format!("QQBot {token}"), "intents": INTENTS, "shard": [0, 1]}}),
                        };
                        write.send(Message::Text(hello.to_string().into())).await.map_err(|error| Stop::Retry(error.to_string()))?;
                    }
                    Some(0) => match payload["t"].as_str() {
                        Some("READY") => {
                            let id = payload["d"]["session_id"].as_str().unwrap_or_default().to_owned();
                            *session = Some((id, last_seq));
                            tracing::info!(bot = payload["d"]["user"]["username"].as_str().unwrap_or_default(), "connected to the QQ gateway");
                        }
                        Some("RESUMED") => tracing::info!("resumed the QQ gateway session"),
                        Some("C2C_MESSAGE_CREATE") => {
                            let d = &payload["d"];
                            let _ = messages.send(QqMessage::C2c(C2cMessage {
                                id: d["id"].as_str().unwrap_or_default().to_owned(),
                                openid: d["author"]["user_openid"].as_str().unwrap_or_default().to_owned(),
                                content: d["content"].as_str().unwrap_or_default().trim().to_owned(),
                                attachments: attachments_of(d),
                            }));
                        }
                        Some("GROUP_AT_MESSAGE_CREATE") => {
                            let _ = messages.send(QqMessage::Group(group_message(&payload["d"])));
                        }
                        // A group with "receive all messages" on sends every
                        // message this way; only those that @ the bot count.
                        Some("GROUP_MESSAGE_CREATE") => match full_mode_mention(&payload["d"]) {
                            Some(message) => { let _ = messages.send(QqMessage::Group(message)); }
                            None => tracing::info!(
                                group = payload["d"]["group_openid"].as_str().unwrap_or("-"),
                                mentions = payload["d"]["mentions"].as_array().map_or(0, Vec::len),
                                "a group message not addressed to the bot"
                            ),
                        },
                        // Only the type: events QQ sends that the bridge does
                        // not handle, such as a group turning the bot off.
                        other => tracing::info!(event = other.unwrap_or("none"), group = payload["d"]["group_openid"].as_str().unwrap_or("-"), "an unhandled QQ event"),
                    },
                    Some(7) => return Err(Stop::Retry("asked to reconnect".into())),
                    Some(9) => return Err(Stop::Reidentify),
                    _ => {}
                }
            }
            _ = heartbeat.tick() => {
                let beat = json!({"op": 1, "d": last_seq});
                write.send(Message::Text(beat.to_string().into())).await.map_err(|error| Stop::Retry(error.to_string()))?;
            }
        }
    }
}

/// Reads a `GROUP_MESSAGE_CREATE` dispatch's `d`: a group message in
/// "receive all messages" mode. It is a mention only when `mentions` has an
/// entry with `is_you`; that entry's `<@id>` tokens are removed from the
/// text, which `GROUP_AT_MESSAGE_CREATE` does by itself.
pub fn full_mode_mention(d: &Value) -> Option<GroupMessage> {
    let mentions = d["mentions"].as_array()?;
    let bot = mentions.iter().find(|mention| mention["is_you"] == true)?;
    let mut message = group_message(d);
    for key in ["member_openid", "id"] {
        if let Some(id) = bot[key].as_str().filter(|id| !id.is_empty()) {
            message.content = message.content.replace(&format!("<@{id}>"), "");
            message.content = message.content.replace(&format!("<@!{id}>"), "");
        }
    }
    message.content = message.content.trim().to_owned();
    Some(message)
}

/// Reads a `GROUP_AT_MESSAGE_CREATE` dispatch's `d`.
pub fn group_message(d: &Value) -> GroupMessage {
    let name = d["author"]["member_nickname"]
        .as_str()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or("QQ user");
    GroupMessage {
        id: d["id"].as_str().unwrap_or_default().to_owned(),
        group_openid: d["group_openid"].as_str().unwrap_or_default().to_owned(),
        member_openid: d["author"]["member_openid"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        name: name.to_owned(),
        content: d["content"].as_str().unwrap_or_default().trim().to_owned(),
        attachments: attachments_of(d),
    }
}

/// The bridge's settings, from `[qq]`.
pub struct Settings {
    pub owner_openid: String,
    /// The owner's `member_openid`s in trusted groups.
    pub owner_member_openids: Vec<String>,
    /// The owner's working folder.
    pub workspace: PathBuf,
    /// Alyosha's working folders, where groups' folders are.
    pub work_dir: PathBuf,
    pub reply_chunk: usize,
    pub schedule: Schedule,
    pub timezone: Tz,
    /// Starts groups' sessions, and names the trusted groups.
    pub groups: GroupSpawners,
}

struct OpenRequest {
    /// The trusted group whose session asked, or `None` for the owner's.
    group: Option<String>,
    permission: Permission,
    options: Vec<String>,
}

/// `/cancel` for a trusted group: its session's turn stops and its open
/// requests end cancelled.
fn cancel_group(pool: &GuestPool, requests: &mut Vec<OpenRequest>, group: &str) {
    let theirs: Vec<u64> = requests
        .iter()
        .filter(|open| open.group.as_deref() == Some(group))
        .map(|open| open.permission.request)
        .collect();
    requests.retain(|open| open.group.as_deref() != Some(group));
    pool.cancel(group, &theirs);
}

/// Runs the bridge until `alyosha acp` exits, the gateway fails for good, or
/// SIGTERM. Returns the exit status.
pub async fn run(
    api: Arc<QqApi>,
    bridge: Bridge,
    mut events: mpsc::UnboundedReceiver<Event>,
    mut messages: mpsc::UnboundedReceiver<QqMessage>,
    settings: Settings,
) -> u8 {
    if let Err(error) = bridge.initialize("alyosha-qq").await {
        tracing::error!(error = %error, "initialize failed");
        return 1;
    }
    if let Err(error) = bridge.open_session(&settings.workspace, "owner:qq").await {
        tracing::error!(error = %error, "the session could not be opened");
        return 1;
    }
    let owner = ToOwner {
        api: api.clone(),
        openid: settings.owner_openid.clone(),
    };
    let chunk = settings.reply_chunk;
    let replies: Arc<Mutex<HashMap<String, ReplyTo>>> = Arc::default();
    let kept: Arc<Mutex<Vec<String>>> = Arc::default();
    {
        // Scheduled tasks: show the prompt, then run it like a message.
        let owner = Arc::new(ToOwner {
            api: api.clone(),
            openid: settings.owner_openid.clone(),
        });
        let kept = kept.clone();
        let replies = replies.clone();
        let show: Arc<dyn Fn(&alyosha_schedule::DueTask) + Send + Sync> = Arc::new(move |task| {
            let owner = owner.clone();
            let kept = kept.clone();
            let replies = replies.clone();
            let text = task.shown.clone();
            let key = alyosha_bridge::runner::task_key(&task.id);
            tokio::spawn(async move {
                replies.lock().await.insert(key, ReplyTo::scheduled());
                if let Some(rest) =
                    deliver(owner.as_ref(), &[text], &mut ReplyTo::scheduled()).await
                {
                    kept.lock().await.push(rest);
                }
            });
        });
        tokio::spawn(alyosha_bridge::runner::run(
            settings.schedule.clone(),
            "qq".into(),
            settings.timezone,
            bridge.clone(),
            show,
        ));
    }
    let seen = Seen::default();
    let markdown = MarkdownRefused::default();
    let (mut pool, mut guest_events) = GuestPool::new(
        settings.groups.clone(),
        "alyosha-qq",
        settings.work_dir.clone(),
    );
    // Each open group prompt's group and the reply context of its mention.
    let mut group_replies: HashMap<String, (String, ReplyTo)> = HashMap::new();
    // Each group's latest mention, which a finished job's reply answers.
    let mut last_mention: HashMap<String, String> = HashMap::new();
    let mut idle_check = tokio::time::interval(Duration::from_secs(60));
    // Open requests from every session, oldest first.
    let mut requests: Vec<OpenRequest> = Vec::new();
    let mut expired_notice = false;
    let mut next_job = 1u64;
    let mut typing = tokio::time::interval(Duration::from_secs(50));
    let mut typing_seq = 100u32;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("a SIGTERM handler");
    loop {
        tokio::select! {
            message = messages.recv() => {
                let message = match message {
                    None => return 1,
                    Some(QqMessage::C2c(message)) => message,
                    Some(QqMessage::Group(message)) => {
                        if !seen.first(&message.id, Instant::now()) {
                            continue;
                        }
                        let reply = ReplyTo::message(&message.id);
                        let blocks = api.blocks(&message.attachments).await;
                        tracing::info!(group = %message.group_openid, member_openid = %message.member_openid, trusted = pool.is_trusted(&message.group_openid), "a group mention");
                        last_mention.insert(message.group_openid.clone(), message.id.clone());
                        let key = format!("g{}:{}", message.group_openid, message.id);
                        // Commands in trusted groups: docs/Channels.md#commands-in-trusted-groups
                        if pool.is_trusted(&message.group_openid) && blocks.is_empty()
                            && let Some(command) = GuestPool::group_command(&message.content)
                        {
                            let group = message.group_openid.clone();
                            let to = ToGroup { api: api.clone(), group_openid: group.clone() };
                            let mut reply = reply;
                            match command {
                                GroupCommand::New => {
                                    let text = match pool.new_session(&group).await {
                                        Ok(()) => "New session started.".to_owned(),
                                        Err(error) => format!("Failed: {error}"),
                                    };
                                    let _ = deliver(&to, &[text], &mut reply).await;
                                }
                                GroupCommand::Cancel => cancel_group(&pool, &mut requests, &group),
                                GroupCommand::Session => {
                                    group_replies.insert(key.clone(), (group.clone(), reply));
                                    if let Err(error) = pool.command(&group, &key, &message.content).await {
                                        tracing::warn!(group = %group, error = %error, "the group session did not start");
                                        group_replies.remove(&key);
                                    }
                                }
                            }
                            continue;
                        }
                        group_replies.insert(key.clone(), (message.group_openid.clone(), reply));
                        let user = format!("qq-{}", message.member_openid);
                        if let Err(error) = pool.prompt(&message.group_openid, &key, &message.content, blocks, &user, &message.name, settings.owner_member_openids.contains(&message.member_openid)).await {
                            tracing::warn!(group = %message.group_openid, error = %error, "the group session did not start");
                            group_replies.remove(&key);
                        }
                        continue;
                    }
                };
                if !seen.first(&message.id, Instant::now()) {
                    continue;
                }
                if settings.owner_openid.is_empty() || message.openid != settings.owner_openid {
                    tracing::info!(user_openid = %message.openid, "dropped a message from someone else");
                    continue;
                }
                let mut reply = ReplyTo::message(&message.id);
                // Replies that could not be delivered go first.
                let waiting: Vec<String> = kept.lock().await.drain(..).collect();
                for text in waiting {
                    if let Some(rest) = deliver(&owner, &reply_parts(&text, chunk), &mut reply).await {
                        kept.lock().await.push(rest);
                    }
                }
                if expired_notice {
                    expired_notice = false;
                    let _ = deliver(&owner, &["Expired — not approved.".to_owned()], &mut reply).await;
                }
                if !message.attachments.is_empty() {
                    let blocks = api.blocks(&message.attachments).await;
                    let key = format!("m{}", message.id);
                    replies.lock().await.insert(key.clone(), reply);
                    bridge.prompt_with(&key, &message.content, blocks);
                    continue;
                }
                // An answer to an open request.
                if let Some(open) = requests.first()
                    && let Some(index) = parse_answer(&message.content, open.options.len())
                {
                    let open = requests.remove(0);
                    let option_id = open.permission.options[index].0.clone();
                    let name = open.options[index].clone();
                    let asker = match &open.group {
                        None => Some(&bridge),
                        Some(group) => pool.bridge(group),
                    };
                    if let Some(asker) = asker {
                        asker.forget_permission(&open.permission.tool_call_id);
                        asker.answer(open.permission.request, Some(&option_id));
                    }
                    let _ = deliver(&owner, &[format!("Answered: {name}")], &mut reply).await;
                    // Later replies for that prompt answer this message.
                    if open.group.is_none() && let Some(key) = open.permission.key {
                        replies.lock().await.insert(key, reply);
                    }
                    continue;
                }
                // `/new <name>` and `/cancel <name>` act on a trusted group.
                if let Some((command @ ("/new" | "/cancel"), name)) = message.content.trim().split_once(' ') {
                    let name = name.trim();
                    let text = match pool.trusted_named(name) {
                        None => format!("No trusted group named {name}."),
                        Some(group) if command == "/new" => match pool.new_session(&group).await {
                            Ok(()) => format!("New session started in group {name}."),
                            Err(error) => format!("Failed: {error}"),
                        },
                        Some(group) => {
                            cancel_group(&pool, &mut requests, &group);
                            format!("Cancelled in group {name}.")
                        }
                    };
                    let _ = deliver(&owner, &[text], &mut reply).await;
                    continue;
                }
                match message.content.as_str() {
                    "/new" => {
                        let text = match bridge.new_session(&settings.workspace, "owner:qq").await {
                            Ok(_) => "New session started.".to_owned(),
                            Err(error) => format!("Failed: {error}"),
                        };
                        let _ = deliver(&owner, &[text], &mut reply).await;
                    }
                    "/cancel" => {
                        // Only the private session's; groups' requests stay open.
                        let open: Vec<u64> = requests.iter().filter(|open| open.group.is_none()).map(|open| open.permission.request).collect();
                        requests.retain(|open| open.group.is_some());
                        bridge.cancel_with_requests(&open);
                    }
                    "/tasks" => {
                        let text = match settings.schedule.read() {
                            Ok(tasks) => alyosha_schedule::tasks_text(&tasks, Utc::now().with_timezone(&settings.timezone)),
                            Err(error) => error.to_string(),
                        };
                        let _ = deliver(&owner, &reply_parts(&text, chunk), &mut reply).await;
                    }
                    text => {
                        let key = format!("m{}", message.id);
                        replies.lock().await.insert(key.clone(), reply);
                        bridge.prompt(&key, text);
                    }
                }
            }
            event = events.recv() => {
                let Some(event) = event else { return 1 };
                match event {
                    Event::Tools { .. } => {}
                    Event::Finished(finished) => {
                        let mut reply = replies.lock().await.remove(&finished.key).unwrap_or_else(ReplyTo::scheduled);
                        if let Some(rest) = deliver_reply(&owner, &finished.reply(), chunk, &mut reply, &markdown).await {
                            kept.lock().await.push(rest);
                        }
                        let problems = send_files(&api, &owner, Chat::User(&settings.owner_openid), &finished.files, &mut reply).await;
                        if !problems.is_empty()
                            && let Some(rest) = deliver(&owner, &problems, &mut reply).await
                        {
                            kept.lock().await.push(rest);
                        }
                    }
                    Event::Permission(permission) => {
                        let options: Vec<String> = permission.options.iter().map(|(_, name)| name.clone()).collect();
                        let text = permission_text(&permission.title, &permission.body, &options, chunk);
                        let delivered = match permission.key.as_ref() {
                            Some(key) => {
                                let mut map = replies.lock().await;
                                let reply = map.entry(key.clone()).or_insert_with(ReplyTo::scheduled);
                                deliver(&owner, &[text], reply).await.is_none()
                            }
                            None => deliver(&owner, &[text], &mut ReplyTo::scheduled()).await.is_none(),
                        };
                        if delivered {
                            requests.push(OpenRequest { group: None, permission, options });
                        } else {
                            // Undeliverable: answer at once rather than keep it.
                            bridge.answer(permission.request, None);
                        }
                    }
                    Event::PermissionExpired { tool_call_id } => {
                        requests.retain(|open| open.group.is_some() || open.permission.tool_call_id != tool_call_id);
                        expired_notice = true;
                    }
                    Event::JobFinished { text, .. } => {
                        let key = format!("job{next_job}");
                        next_job += 1;
                        if let Some(rest) = deliver(&owner, &["⚙ Job finished:".to_owned()], &mut ReplyTo::scheduled()).await {
                            kept.lock().await.push(rest);
                        }
                        replies.lock().await.insert(key.clone(), ReplyTo::scheduled());
                        bridge.prompt(&key, &text);
                    }
                    Event::Exited => {
                        let _ = deliver(&owner, &["Alyosha stopped unexpectedly. Restarting.".to_owned()], &mut ReplyTo::scheduled()).await;
                        return 1;
                    }
                }
            }
            guest = guest_events.recv() => {
                let Some(GuestEvent { group, guest, event }) = guest else { continue };
                match event {
                    Event::Finished(finished) => {
                        pool.touch(&group);
                        if let Some((group_openid, mut reply)) = group_replies.remove(&finished.key) {
                            let to = ToGroup { api: api.clone(), group_openid: group_openid.clone() };
                            if deliver_reply(&to, &finished.reply(), chunk, &mut reply, &markdown).await.is_some() {
                                tracing::warn!(group = %group, "a group reply failed; dropped");
                            }
                            let problems = send_files(&api, &to, Chat::Group(&group_openid), &finished.files, &mut reply).await;
                            if !problems.is_empty() && deliver(&to, &problems, &mut reply).await.is_some() {
                                tracing::warn!(group = %group, "a group reply failed; dropped");
                            }
                        } else {
                            for file in &finished.files {
                                file.remove();
                            }
                        }
                    }
                    Event::Exited => pool.exited(&group, guest),
                    // Only a trusted group's session asks the owner.
                    Event::Permission(permission) if pool.is_trusted(&group) => {
                        let options: Vec<String> = permission.options.iter().map(|(_, name)| name.clone()).collect();
                        let text = permission_text(&permission.title, &permission.body, &options, chunk);
                        let text = format!("In group {}:\n\n{text}", pool.name(&group));
                        if deliver(&owner, &[text], &mut ReplyTo::scheduled()).await.is_none() {
                            requests.push(OpenRequest { group: Some(group.clone()), permission, options });
                        } else if let Some(asker) = pool.bridge(&group) {
                            // Undeliverable: answer at once rather than keep it.
                            asker.answer(permission.request, None);
                        }
                    }
                    Event::PermissionExpired { tool_call_id } => {
                        let before = requests.len();
                        requests.retain(|open| open.group.as_deref() != Some(group.as_str()) || open.permission.tool_call_id != tool_call_id);
                        expired_notice |= requests.len() < before;
                    }
                    Event::JobFinished { text, .. } if pool.is_trusted(&group) => {
                        let key = format!("gjob{next_job}");
                        next_job += 1;
                        let reply = last_mention.get(&group).map_or_else(ReplyTo::scheduled, |id| ReplyTo::message(id));
                        group_replies.insert(key.clone(), (group.clone(), reply));
                        if !pool.prompt_job(&group, &key, &text) {
                            group_replies.remove(&key);
                        }
                    }
                    // Guest sessions have no approver or jobs, and no group
                    // gets tool calls shown.
                    _ => {}
                }
            }
            _ = idle_check.tick() => pool.stop_idle(IDLE_LIMIT).await,
            _ = typing.tick() => {
                let open = bridge.open_keys();
                let map = replies.lock().await;
                for key in open {
                    if let Some(ReplyTo { msg_id: Some(msg_id), passive_closed: false, .. }) = map.get(&key) {
                        typing_seq += 1;
                        let body = json!({"msg_type": 6, "input_notify": {"input_type": 1, "input_second": 60}, "msg_id": msg_id, "msg_seq": typing_seq});
                        let _ = owner.send(body).await;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelling_a_group_drops_only_its_requests() {
        let request = |group: Option<&str>, id: u64| OpenRequest {
            group: group.map(str::to_owned),
            permission: Permission {
                request: id,
                tool_call_id: format!("c{id}"),
                title: String::new(),
                body: String::new(),
                options: Vec::new(),
                key: None,
            },
            options: Vec::new(),
        };
        let mut requests = vec![
            request(None, 1),
            request(Some("G1"), 2),
            request(Some("G2"), 3),
            request(Some("G1"), 4),
        ];
        let (pool, _events) =
            GuestPool::new(GroupSpawners::default(), "alyosha-qq", std::env::temp_dir());
        cancel_group(&pool, &mut requests, "G1");
        let left: Vec<u64> = requests
            .iter()
            .map(|open| open.permission.request)
            .collect();
        assert_eq!(left, [1, 3]);
    }

    #[test]
    fn group_mentions_carry_the_group_member_and_name() {
        let d = json!({
            "id": "m1",
            "group_openid": "G1",
            "content": " what is 2+2? ",
            "author": {"member_openid": "M7", "member_nickname": "Bob"},
            "attachments": [],
        });
        let message = group_message(&d);
        assert_eq!(
            (
                message.id.as_str(),
                message.group_openid.as_str(),
                message.member_openid.as_str(),
                message.name.as_str(),
                message.content.as_str(),
            ),
            ("m1", "G1", "M7", "Bob", "what is 2+2?")
        );
        assert!(message.attachments.is_empty());
        let unnamed = group_message(
            &json!({"id": "m2", "group_openid": "G1", "author": {"member_openid": "M8"}, "content": "", "attachments": [{"url": "x"}]}),
        );
        assert_eq!(unnamed.name, "QQ user");
        assert!(unnamed.content.is_empty() && unnamed.attachments.len() == 1);
    }

    #[test]
    fn full_mode_messages_count_only_when_they_mention_the_bot() {
        let message = |content: &str, mentions: Value| json!({"id": "m3", "group_openid": "G2", "content": content, "author": {"member_openid": "M7", "member_nickname": "Bob"}, "mentions": mentions});
        let plain = message("大家早上好", Value::Null);
        assert!(full_mode_mention(&plain).is_none());
        let other = message(
            "<@M9> 你好",
            json!([{"member_openid": "M9", "is_you": false}]),
        );
        assert!(full_mode_mention(&other).is_none());
        let ours = message(
            "<@B1> 你好 <@M9>",
            json!([{"member_openid": "B1", "id": "B1", "is_you": true, "bot": true}, {"member_openid": "M9", "is_you": false}]),
        );
        let found = full_mode_mention(&ours).unwrap();
        assert_eq!(found.content, "你好 <@M9>");
        assert_eq!(
            (found.group_openid.as_str(), found.name.as_str()),
            ("G2", "Bob")
        );
    }
}
