//! The HTTP client for the local GPROXY service.

use std::time::Duration;

use alyosha_config::{Api, Effort};
use reqwest::{StatusCode, header::HeaderMap};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::{
    ModelClient, ModelError, ModelReply, ModelRequest, TokenUsage, chat, claude, gemini, responses,
    sse::{SseEvent, SseParser},
};
use crate::{BoxFuture, agent_state::AssistantEntry};

/// How long the client waits.
#[derive(Debug, Clone, Copy)]
pub struct Timeouts {
    /// To connect to GPROXY.
    pub connect: Duration,
    /// Without receiving any response bytes. A reply that keeps arriving is
    /// never cut off.
    pub idle: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(10),
            idle: Duration::from_secs(5 * 60),
        }
    }
}

/// Assembles one route's stream into a complete response.
pub(crate) trait Assembler: Send {
    /// Takes one event. Fails at once on a provider error event.
    fn event(&mut self, event: &SseEvent) -> Result<(), ModelError>;
    /// The complete response, or an error if the provider never signalled
    /// that the whole response completed.
    fn finish(self: Box<Self>) -> Result<(AssistantEntry, Option<TokenUsage>), ModelError>;
}

/// An encoded native request.
pub(crate) struct Encoded {
    pub path: String,
    pub body: Value,
    pub headers: Vec<(&'static str, &'static str)>,
}

/// Sends model requests through GPROXY on one native route.
#[derive(Debug, Clone)]
pub struct GatewayClient {
    http: reqwest::Client,
    base_url: String,
    gateway_key: String,
    api: Api,
    timeouts: Timeouts,
    effort: Option<Effort>,
}

impl GatewayClient {
    pub fn new(base_url: &str, gateway_key: &str, api: Api, timeouts: Timeouts) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(timeouts.connect)
            .build()
            .expect("the HTTP client settings are valid");
        Self {
            http,
            base_url: base_url.trim_end_matches('/').to_owned(),
            gateway_key: gateway_key.to_owned(),
            api,
            timeouts,
            effort: None,
        }
    }

    /// Sends `effort` with every request, where the API takes one.
    #[must_use]
    pub fn with_effort(mut self, effort: Option<Effort>) -> Self {
        self.effort = effort;
        self
    }

    fn encode(&self, request: &ModelRequest) -> Encoded {
        match self.api {
            Api::Codex => responses::encode(request, false, self.effort),
            Api::Openai => responses::encode(request, true, self.effort),
            Api::Claudeapi => claude::encode(request),
            Api::Deepseek => chat::encode(request),
            Api::Aistudio => gemini::encode(request),
        }
    }

    fn assembler(&self) -> Box<dyn Assembler> {
        match self.api {
            Api::Codex | Api::Openai => Box::<responses::Stream>::default(),
            Api::Claudeapi => Box::<claude::Stream>::default(),
            Api::Deepseek => Box::<chat::Stream>::default(),
            Api::Aistudio => Box::<gemini::Stream>::default(),
        }
    }

    async fn send(&self, request: ModelRequest) -> Result<ModelReply, ModelError> {
        let encoded = self.encode(&request);
        let mut builder = self
            .http
            .post(format!("{}{}", self.base_url, encoded.path))
            .bearer_auth(&self.gateway_key)
            .json(&encoded.body);
        for (name, value) in encoded.headers {
            builder = builder.header(name, value);
        }
        let response = tokio::time::timeout(self.timeouts.idle, builder.send())
            .await
            .map_err(|_| self.idle_timeout())?
            .map_err(|error| self.send_error(&error))?;
        let request_id = request_id(response.headers());
        let status = response.status();
        if !status.is_success() {
            let retry_after = retry_after(response.headers());
            let body = tokio::time::timeout(self.timeouts.idle, response.text())
                .await
                .map_err(|_| self.idle_timeout())?
                .unwrap_or_default();
            return Err(status_error(status, &body, retry_after));
        }

        let mut response = response;
        let mut parser = SseParser::default();
        let mut assembler = self.assembler();
        loop {
            let chunk = tokio::time::timeout(self.timeouts.idle, response.chunk())
                .await
                .map_err(|_| self.idle_timeout())?
                .map_err(|error| {
                    ModelError::Incomplete(format!("the stream broke: {}", error_chain(&error)))
                })?;
            let Some(chunk) = chunk else { break };
            for event in parser.feed(&chunk) {
                assembler.event(&event)?;
            }
        }
        // A final event without its blank line still counts.
        for event in parser.feed(b"\n\n") {
            assembler.event(&event)?;
        }
        let (entry, usage) = assembler.finish()?;
        Ok(ModelReply {
            entry,
            usage,
            request_id,
        })
    }

    fn idle_timeout(&self) -> ModelError {
        ModelError::Timeout(format!(
            "no reply from the model for {} seconds",
            self.timeouts.idle.as_secs()
        ))
    }

    fn send_error(&self, error: &reqwest::Error) -> ModelError {
        if error.is_connect() && error.is_timeout() {
            ModelError::Timeout(format!(
                "could not connect to GPROXY at {} within {} seconds. Check `systemctl status gproxy`.",
                self.base_url,
                self.timeouts.connect.as_secs()
            ))
        } else if error.is_connect() {
            ModelError::Unreachable(format!(
                "nothing answered at {}. Check `systemctl status gproxy`.",
                self.base_url
            ))
        } else {
            ModelError::Incomplete(format!("the request failed: {}", error_chain(error)))
        }
    }
}

impl ModelClient for GatewayClient {
    fn generate(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
    ) -> BoxFuture<'_, Result<ModelReply, ModelError>> {
        Box::pin(async move {
            let model = request.model.clone();
            let started = std::time::Instant::now();
            let result = tokio::select! {
                biased;
                () = cancel.cancelled() => Err(ModelError::Cancelled),
                result = self.send(request) => result,
            };
            let duration_ms = started.elapsed().as_millis() as u64;
            match &result {
                Ok(reply) => tracing::info!(
                    api = ?self.api,
                    model = %model,
                    duration_ms,
                    input_tokens = reply.usage.map(|usage| usage.input_tokens),
                    output_tokens = reply.usage.map(|usage| usage.output_tokens),
                    tool_calls = reply.entry.tool_calls().count(),
                    request_id = reply.request_id.as_deref().unwrap_or("-"),
                    "model request"
                ),
                Err(ModelError::Cancelled) => {
                    tracing::info!(api = ?self.api, model = %model, duration_ms, "model request cancelled")
                }
                Err(error) => tracing::warn!(
                    api = ?self.api,
                    model = %model,
                    duration_ms,
                    error = %error,
                    "model request failed"
                ),
            }
            result
        })
    }
}

fn error_chain(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

fn request_id(headers: &HeaderMap) -> Option<String> {
    ["x-request-id", "request-id", "x-gproxy-request-id"]
        .iter()
        .find_map(|name| headers.get(*name)?.to_str().ok().map(str::to_owned))
}

fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let value = headers.get("retry-after")?.to_str().ok()?;
    value.trim().parse::<u64>().ok().map(Duration::from_secs)
}

/// Maps an error status from GPROXY or the provider to a model error.
pub(crate) fn status_error(
    status: StatusCode,
    body: &str,
    retry_after: Option<Duration>,
) -> ModelError {
    let message = error_message(body).unwrap_or_else(|| format!("HTTP {}", status.as_u16()));
    match status.as_u16() {
        // GPROXY answers a path naming no provider with 404, and a missing,
        // expired, or invalid credential with 401 or 403.
        401 | 403 | 404 => ModelError::NotSignedIn,
        429 => ModelError::RateLimited {
            message,
            retry_after,
        },
        413 => ModelError::ContextOverflow(message),
        400..=499 if is_overflow(body) => ModelError::ContextOverflow(message),
        400..=499 => ModelError::Unsupported(message),
        _ => ModelError::Provider(format!("HTTP {}: {message}", status.as_u16())),
    }
}

/// The message of a JSON error body, or the start of a plain one.
pub(crate) fn error_message(body: &str) -> Option<String> {
    if let Ok(json) = serde_json::from_str::<Value>(body) {
        let error = json.get("error").unwrap_or(&json);
        if let Some(message) = error.get("message").and_then(Value::as_str) {
            return Some(message.to_owned());
        }
        if let Some(message) = error.as_str() {
            return Some(message.to_owned());
        }
    }
    let trimmed = body.trim();
    (!trimmed.is_empty()).then(|| trimmed.chars().take(300).collect())
}

/// Whether an error body reports an exceeded context window.
pub(crate) fn is_overflow(body: &str) -> bool {
    let body = body.to_lowercase();
    [
        "context_length_exceeded",
        "context length",
        "context window",
        "prompt is too long",
        "maximum context",
        "too many tokens",
        "exceeds the maximum number of tokens",
        "input token count",
    ]
    .iter()
    .any(|marker| body.contains(marker))
}

/// Maps an error reported inside a stream.
pub(crate) fn stream_error(code: &str, message: &str) -> ModelError {
    let text = if message.is_empty() { code } else { message };
    if is_overflow(code) || is_overflow(message) {
        ModelError::ContextOverflow(text.to_owned())
    } else if code.contains("rate_limit") {
        ModelError::RateLimited {
            message: text.to_owned(),
            retry_after: None,
        }
    } else {
        ModelError::Provider(text.to_owned())
    }
}

/// Arguments as the model wrote them: a JSON value, or the raw text if it is
/// not valid JSON, so the executor rejects it and replay sends it unchanged.
pub(crate) fn parse_arguments(raw: &str) -> Value {
    if raw.trim().is_empty() {
        return Value::Object(Default::default());
    }
    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_owned()))
}

/// The arguments as text for routes that carry them as a string.
pub(crate) fn arguments_text(arguments: &Value) -> String {
    match arguments {
        Value::String(raw) => raw.clone(),
        other => other.to_string(),
    }
}
