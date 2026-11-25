//! QQ's API and WebSocket gateway, and the bridge's main loop.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use alyosha_bridge::{Bridge, Event, Permission};
use alyosha_schedule::Schedule;
use chrono::Utc;
use chrono_tz::Tz;
use futures_util::{SinkExt, StreamExt, future::BoxFuture};
use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc};
use tokio_tungstenite::tungstenite::Message;

use crate::{
    ReplyTo, Seen, SendError, Sender, deliver, parse_answer, parse_token, permission_text,
    reply_parts,
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
        let token = self.token().await.map_err(|error| SendError {
            code: -1,
            message: error.to_string(),
        })?;
        let response = self
            .http
            .post(format!("{}/v2/users/{openid}/messages", self.api))
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
            return Ok(());
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

/// A private message from the gateway.
#[derive(Debug, Clone)]
pub struct C2cMessage {
    pub id: String,
    pub openid: String,
    pub content: String,
    pub has_attachments: bool,
}

/// Keeps a WebSocket connection to the gateway and forwards private
/// messages. Returns only on a fatal error.
pub async fn listen(api: Arc<QqApi>, messages: mpsc::UnboundedSender<C2cMessage>) -> GatewayError {
    let mut session: Option<(String, Option<u64>)> = None;
    let mut backoff = Duration::from_secs(1);
    loop {
        match connect_once(&api, &messages, &mut session).await {
            Ok(()) => backoff = Duration::from_secs(1),
            Err(Stop::Fatal(error)) => return error,
            Err(Stop::Reidentify) => session = None,
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
    messages: &mpsc::UnboundedSender<C2cMessage>,
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
                        }
                        Some("C2C_MESSAGE_CREATE") => {
                            let d = &payload["d"];
                            let _ = messages.send(C2cMessage {
                                id: d["id"].as_str().unwrap_or_default().to_owned(),
                                openid: d["author"]["user_openid"].as_str().unwrap_or_default().to_owned(),
                                content: d["content"].as_str().unwrap_or_default().trim().to_owned(),
                                has_attachments: d["attachments"].as_array().is_some_and(|items| !items.is_empty()),
                            });
                        }
                        _ => {}
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

/// The bridge's settings, from `[qq]`.
pub struct Settings {
    pub owner_openid: String,
    pub workspace: PathBuf,
    pub reply_chunk: usize,
    pub schedule: Schedule,
    pub timezone: Tz,
}

struct OpenRequest {
    permission: Permission,
    options: Vec<String>,
}

/// Runs the bridge until `alyosha acp` exits, the gateway fails for good, or
/// SIGTERM. Returns the exit status.
pub async fn run(
    api: Arc<QqApi>,
    bridge: Bridge,
    mut events: mpsc::UnboundedReceiver<Event>,
    mut messages: mpsc::UnboundedReceiver<C2cMessage>,
    settings: Settings,
) -> u8 {
    if let Err(error) = bridge.initialize("alyosha-qq").await {
        tracing::error!(error = %error, "initialize failed");
        return 1;
    }
    if let Err(error) = bridge.new_session(&settings.workspace).await {
        tracing::error!(error = %error, "session/new failed");
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
    let mut requests: HashMap<u64, OpenRequest> = HashMap::new();
    let mut expired_notice = false;
    let mut next_job = 1u64;
    let mut typing = tokio::time::interval(Duration::from_secs(50));
    let mut typing_seq = 100u32;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("a SIGTERM handler");
    loop {
        tokio::select! {
            message = messages.recv() => {
                let Some(message) = message else { return 1 };
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
                if message.content.is_empty() && message.has_attachments {
                    let _ = deliver(&owner, &["Only text messages work for now.".to_owned()], &mut reply).await;
                    continue;
                }
                // An answer to an open request.
                if let Some((&id, open)) = requests.iter().next()
                    && let Some(index) = parse_answer(&message.content, open.options.len())
                {
                    let option_id = open.permission.options[index].0.clone();
                    let name = open.options[index].clone();
                    let key = open.permission.key.clone();
                    bridge.forget_permission(&open.permission.tool_call_id);
                    requests.remove(&id);
                    bridge.answer(id, Some(&option_id));
                    let _ = deliver(&owner, &[format!("Answered: {name}")], &mut reply).await;
                    // Later replies for that prompt answer this message.
                    if let Some(key) = key {
                        replies.lock().await.insert(key, reply);
                    }
                    continue;
                }
                match message.content.as_str() {
                    "/new" => {
                        let text = match bridge.new_session(&settings.workspace).await {
                            Ok(_) => "New session started.".to_owned(),
                            Err(error) => format!("Failed: {error}"),
                        };
                        let _ = deliver(&owner, &[text], &mut reply).await;
                    }
                    "/cancel" => {
                        let open: Vec<u64> = requests.drain().map(|(id, _)| id).collect();
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
                        let parts = reply_parts(&finished.reply(), chunk);
                        if let Some(rest) = deliver(&owner, &parts, &mut reply).await {
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
                            requests.insert(permission.request, OpenRequest { permission, options });
                        } else {
                            // Undeliverable: answer at once rather than keep it.
                            bridge.answer(permission.request, None);
                        }
                    }
                    Event::PermissionExpired { tool_call_id } => {
                        requests.retain(|_, open| open.permission.tool_call_id != tool_call_id);
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
                        let _ = deliver(&owner, &["Alyosha stopped unexpectedly. Restarting with a new session.".to_owned()], &mut ReplyTo::scheduled()).await;
                        return 1;
                    }
                }
            }
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
                bridge.shutdown().await;
                return 0;
            }
        }
    }
}
