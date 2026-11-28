//! MCP Client: starts the configured stdio MCP servers, registers their tools
//! at session start, and forwards calls.
//!
//! Servers are trusted child processes outside the shell sandbox, with the
//! workspace as their working directory and a cleared environment. This is a
//! small JSON-RPC 2.0 client of Alyosha's own over newline-delimited stdio: it
//! needs only the handshake, `tools/list`, `tools/call`, cancellation, and
//! `ping`. See `docs/MCP.md#implementation`.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::Path,
    process::Stdio,
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use alyosha_config::{McpConfig, McpServerConfig, Secrets};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, Command},
    sync::oneshot,
};

use crate::{
    environment::LogCapture,
    tools::{
        OutputKind, RegisteredTool, ToolContext, ToolDefinition, ToolError, ToolOutput, handler,
    },
};

/// Launch, handshake, and `tools/list` together.
pub const STARTUP_LIMIT: Duration = Duration::from_secs(30);
/// One call's deadline.
pub const CALL_LIMIT: Duration = Duration::from_secs(120);
const PROTOCOL_VERSION: &str = "2025-06-18";
const STDERR_LINE_LIMIT: usize = 1024;

type Pending = Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>;

/// One running server.
struct Server {
    name: String,
    stdin: tokio::sync::Mutex<Option<ChildStdin>>,
    pending: Arc<Pending>,
    next_id: AtomicU64,
    dead: Arc<AtomicBool>,
    child: tokio::sync::Mutex<Option<Child>>,
}

/// A tool a server offered and the configuration kept.
struct OfferedTool {
    server: Arc<Server>,
    original: String,
    definition: ToolDefinition,
}

/// The session's MCP servers.
#[derive(Default)]
pub struct McpManager {
    servers: Vec<Arc<Server>>,
    tools: Vec<OfferedTool>,
}

impl std::fmt::Debug for McpManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpManager")
            .field("servers", &self.servers.len())
            .finish()
    }
}

impl McpManager {
    /// Starts every server concurrently. A server that fails to start, or a
    /// tool that cannot be registered, is a warning.
    pub async fn start(
        config: &McpConfig,
        workspace: &Path,
        secrets: &Secrets,
    ) -> (Self, Vec<String>) {
        let starts = config.servers.iter().map(|(name, server)| async move {
            let result = tokio::time::timeout(
                STARTUP_LIMIT,
                start_server(name, server, workspace, secrets),
            )
            .await;
            (name, server, result)
        });
        let mut manager = Self::default();
        let mut warnings = Vec::new();
        let mut names = HashSet::new();
        for (name, server_config, result) in futures_util::future::join_all(starts).await {
            let (server, listed) = match result {
                Ok(Ok(started)) => started,
                Ok(Err(error)) => {
                    warnings.push(format!("{name}: failed to start: {error}"));
                    continue;
                }
                Err(_) => {
                    warnings.push(format!(
                        "{name}: did not finish starting within {} seconds",
                        STARTUP_LIMIT.as_secs()
                    ));
                    continue;
                }
            };
            let offered: HashSet<String> = listed
                .iter()
                .filter_map(|tool| tool["name"].as_str().map(str::to_owned))
                .collect();
            for asked in &server_config.ask {
                if !offered.contains(asked) {
                    warnings.push(format!(
                        "{name}: the server offers no tool {asked:?} listed in ask"
                    ));
                }
            }
            for tool in listed {
                let Some(original) = tool["name"].as_str() else {
                    continue;
                };
                if let Some(allowed) = &server_config.tools
                    && !allowed.iter().any(|allowed| allowed == original)
                {
                    continue;
                }
                let sanitized: String = original
                    .chars()
                    .map(|c| {
                        if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                            c
                        } else {
                            '_'
                        }
                    })
                    .collect();
                let model_name = format!("mcp__{name}__{sanitized}");
                if model_name.len() > 64 {
                    warnings.push(format!(
                        "{name}: skipped {original}: its name is longer than 64 bytes"
                    ));
                    continue;
                }
                if !names.insert(model_name.clone()) {
                    warnings.push(format!(
                        "{name}: skipped {original}: its name collides with another tool"
                    ));
                    continue;
                }
                let description = tool["description"]
                    .as_str()
                    .or(tool["title"].as_str())
                    .unwrap_or_default()
                    .to_owned();
                manager.tools.push(OfferedTool {
                    server: server.clone(),
                    original: original.to_owned(),
                    definition: ToolDefinition {
                        name: model_name,
                        description,
                        input_schema: tool
                            .get("inputSchema")
                            .cloned()
                            .unwrap_or_else(|| json!({"type": "object"})),
                        requires_approval: server_config.ask.iter().any(|asked| asked == original),
                    },
                });
            }
            manager.servers.push(server);
        }
        (manager, warnings)
    }

    /// Every kept tool, ready for the registry.
    pub fn tools(&self) -> Vec<RegisteredTool> {
        self.tools
            .iter()
            .map(|tool| {
                let server = tool.server.clone();
                let original = tool.original.clone();
                RegisteredTool {
                    definition: tool.definition.clone(),
                    handler: handler(move |arguments: Value, context: ToolContext| {
                        let server = server.clone();
                        let original = original.clone();
                        async move { call(&server, &original, arguments, &context).await }
                    }),
                }
            })
            .collect()
    }

    /// Closes each server's stdin, waits up to 5 seconds, then kills it.
    pub async fn shutdown(&self) {
        for server in &self.servers {
            server.stdin.lock().await.take();
        }
        for server in &self.servers {
            if let Some(mut child) = server.child.lock().await.take()
                && tokio::time::timeout(Duration::from_secs(5), child.wait())
                    .await
                    .is_err()
            {
                let _ = child.kill().await;
            }
        }
    }
}

/// The environment a server gets: `PATH`, `HOME`, Alyosha's path variables
/// and the XDG folders when set, its configured `env`, and the secrets its
/// `secret_env` names. A secret that is not set keeps the server from
/// starting.
fn server_environment(
    config: &McpServerConfig,
    secrets: &Secrets,
) -> Result<BTreeMap<String, String>, String> {
    let mut env = BTreeMap::new();
    let inherited = ["PATH", "HOME", "XDG_CONFIG_HOME", "XDG_DATA_HOME"]
        .into_iter()
        .chain(alyosha_config::PATH_VARIABLES);
    for name in inherited {
        if let Ok(value) = std::env::var(name) {
            env.insert(name.to_owned(), value);
        }
    }
    for (name, value) in &config.env {
        env.insert(name.clone(), value.clone());
    }
    for (name, secret) in &config.secret_env {
        let value = secrets.read(secret).map_err(|error| error.to_string())?;
        env.insert(name.clone(), value.expose().to_owned());
    }
    Ok(env)
}

async fn start_server(
    name: &str,
    config: &McpServerConfig,
    workspace: &Path,
    secrets: &Secrets,
) -> Result<(Arc<Server>, Vec<Value>), String> {
    let env = server_environment(config, secrets)?;
    let mut child = Command::new(&config.command)
        .args(&config.args)
        .env_clear()
        .envs(env)
        .current_dir(workspace)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| format!("{}: {error}", config.command))?;
    let stdin = child.stdin.take().ok_or("no stdin")?;
    let stdout = child.stdout.take().ok_or("no stdout")?;
    let stderr = child.stderr.take().ok_or("no stderr")?;
    let pending: Arc<Pending> = Arc::default();
    let dead = Arc::new(AtomicBool::new(false));
    let server = Arc::new(Server {
        name: name.to_owned(),
        stdin: tokio::sync::Mutex::new(Some(stdin)),
        pending: pending.clone(),
        next_id: AtomicU64::new(1),
        dead: dead.clone(),
        child: tokio::sync::Mutex::new(Some(child)),
    });

    // Server diagnostics go to the log, one line each, prefixed and cut.
    let log_name = name.to_owned();
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let line = crate::context_manager::cut(&line, STDERR_LINE_LIMIT);
            tracing::info!(target: "alyosha::mcp", "{log_name}: {line}");
        }
    });
    let reader = Arc::downgrade(&server);
    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let Some(server) = reader.upgrade() else {
                return;
            };
            if message.get("method").is_some() {
                if let Some(id) = message.get("id") {
                    // A request from the server: answer ping, refuse the rest.
                    let reply = if message["method"] == "ping" {
                        json!({"jsonrpc": "2.0", "id": id, "result": {}})
                    } else {
                        json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "method not found"}})
                    };
                    let _ = server.send(&reply).await;
                }
                continue;
            }
            let Some(id) = message["id"].as_u64() else {
                continue;
            };
            let waiter = pending
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&id);
            if let Some(waiter) = waiter {
                let result = match message.get("error") {
                    Some(error) => Err(error["message"].as_str().unwrap_or("error").to_owned()),
                    None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
                };
                let _ = waiter.send(result);
            }
        }
        dead.store(true, Ordering::SeqCst);
        pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    });

    server
        .request(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "alyosha", "version": env!("CARGO_PKG_VERSION")},
            }),
        )
        .await?
        .await
        .map_err(|_| "the server closed during the handshake".to_owned())??;
    server
        .send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
        .await?;
    let mut tools = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let params = match &cursor {
            Some(cursor) => json!({"cursor": cursor}),
            None => json!({}),
        };
        let page = server
            .request("tools/list", params)
            .await?
            .await
            .map_err(|_| "the server closed while listing tools".to_owned())??;
        tools.extend(page["tools"].as_array().cloned().unwrap_or_default());
        match page["nextCursor"].as_str() {
            Some(next) if !next.is_empty() => cursor = Some(next.to_owned()),
            _ => break,
        }
    }
    Ok((server, tools))
}

impl Server {
    async fn send(&self, message: &Value) -> Result<(), String> {
        let mut stdin = self.stdin.lock().await;
        let stdin = stdin.as_mut().ok_or("the server has been shut down")?;
        let mut line = message.to_string();
        line.push('\n');
        stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|error| format!("could not write to the server: {error}"))?;
        stdin.flush().await.map_err(|error| error.to_string())
    }

    /// Sends a request and returns its ID's waiter.
    async fn request(
        &self,
        method: &str,
        params: Value,
    ) -> Result<oneshot::Receiver<Result<Value, String>>, String> {
        let (id, receiver) = self.register();
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await?;
        Ok(receiver)
    }

    fn register(&self) -> (u64, oneshot::Receiver<Result<Value, String>>) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id, sender);
        (id, receiver)
    }

    /// Tells the server to stop working on a request and forgets it, so a
    /// late reply is dropped.
    async fn cancel(&self, id: u64, reason: &str) {
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&id);
        let _ = self
            .send(&json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": id, "reason": reason}}))
            .await;
    }
}

/// Forwards one call and converts the reply.
async fn call(
    server: &Server,
    original: &str,
    arguments: Value,
    context: &ToolContext,
) -> Result<ToolOutput, ToolError> {
    let gone = || {
        ToolError::failed(format!(
            "The MCP server {} has stopped. Its tools are unavailable until the next session.",
            server.name
        ))
    };
    if server.dead.load(Ordering::SeqCst) {
        return Err(gone());
    }
    let (id, receiver) = server.register();
    let request = json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": original, "arguments": arguments}});
    if server.send(&request).await.is_err() {
        return Err(gone());
    }
    let reply = tokio::select! {
        biased;
        () = context.cancel.cancelled() => {
            server.cancel(id, "the owner stopped the turn").await;
            return Err(ToolError::Cancelled);
        }
        () = tokio::time::sleep(CALL_LIMIT) => {
            server.cancel(id, "timed out").await;
            return Err(ToolError::failed(format!(
                "The MCP call {}.{original} timed out after {} seconds.",
                server.name,
                CALL_LIMIT.as_secs()
            )));
        }
        reply = receiver => reply,
    };
    let result = match reply {
        Ok(Ok(result)) => result,
        Ok(Err(message)) => {
            return Err(ToolError::failed(format!(
                "The MCP server {} returned an error: {message}",
                server.name
            )));
        }
        Err(_) => return Err(gone()),
    };
    let text = convert(&result);
    let images = images(&result);
    if result["isError"].as_bool() == Some(true) {
        return Err(ToolError::failed(text));
    }
    let capture = context
        .session
        .environment
        .as_ref()
        .map(|environment| LogCapture::new(environment.logs.clone(), text.len() as u64, "mcp"));
    Ok(ToolOutput {
        text,
        images,
        notice: None,
        kind: OutputKind::Logged {
            status: None,
            capture,
        },
    })
}

/// The images of a `tools/call` result, decoded from base64. One that does
/// not decode is left empty, so preparing it reports that it could not be
/// read. See `docs/Media.md#images-in-tool-results`.
pub fn images(result: &Value) -> Vec<crate::media::RawImage> {
    use base64::Engine;
    result["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|block| block["type"] == "image")
        .map(|block| crate::media::RawImage {
            mime: block["mimeType"].as_str().unwrap_or("unknown").to_owned(),
            bytes: base64::engine::general_purpose::STANDARD
                .decode(block["data"].as_str().unwrap_or_default())
                .unwrap_or_default(),
        })
        .collect()
}

/// A `tools/call` result as text. Images are returned apart, by `images`.
pub fn convert(result: &Value) -> String {
    let content = result["content"].as_array().cloned().unwrap_or_default();
    if content.is_empty()
        && let Some(structured) = result.get("structuredContent")
    {
        return structured.to_string();
    }
    content
        .iter()
        .filter(|block| block["type"] != "image")
        .map(|block| match block["type"].as_str().unwrap_or_default() {
            "text" => block["text"].as_str().unwrap_or_default().to_owned(),
            "audio" => format!(
                "[audio omitted: {}]",
                block["mimeType"].as_str().unwrap_or("unknown")
            ),
            "resource_link" => format!("[resource: {}]", block["uri"].as_str().unwrap_or_default()),
            "resource" => match block["resource"]["text"].as_str() {
                Some(text) => text.to_owned(),
                None => format!(
                    "[resource omitted: {}]",
                    block["resource"]["uri"].as_str().unwrap_or_default()
                ),
            },
            other => format!("[{other} omitted]"),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn images_of(result: &Value) -> Vec<u8> {
        images(result).remove(0).bytes
    }

    #[test]
    fn replies_convert_to_text() {
        let result = json!({"content": [
            {"type": "text", "text": "one"},
            {"type": "image", "mimeType": "image/png", "data": "x"},
            {"type": "resource_link", "uri": "file:///a"},
            {"type": "resource", "resource": {"uri": "file:///b", "text": "embedded"}},
            {"type": "resource", "resource": {"uri": "file:///c", "blob": "x"}},
        ]});
        assert_eq!(
            convert(&result),
            "one\n[resource: file:///a]\nembedded\n[resource omitted: file:///c]"
        );
        let images = images(&result);
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].mime, "image/png");
        assert!(
            images[0].bytes.is_empty(),
            "\"x\" is not base64 of anything"
        );
        let png =
            json!({"content": [{"type": "image", "mimeType": "image/png", "data": "iVBORw0K"}]});
        assert_eq!(images_of(&png), [0x89, b'P', b'N', b'G', 0x0d, 0x0a]);
        assert_eq!(
            convert(&json!({"content": [], "structuredContent": {"a": 1}})),
            "{\"a\":1}"
        );
    }
}
