//! The Gemini route, used by the `aistudio` channel.
//!
//! Gemini omits call IDs, so Alyosha assigns them and finds each result's
//! function name through its call. A thought signature is kept as a replay
//! part right before the part it came with and sent back on that part.

use std::{
    collections::HashMap,
    sync::atomic::{AtomicU64, Ordering},
};

use serde_json::{Map, Value, json};

use super::{
    ModelError, ModelRequest, TokenUsage,
    gateway::{Assembler, Encoded, stream_error},
    result_text,
    sse::SseEvent,
    summary_text,
};
use crate::agent_state::{
    AssistantEntry, AssistantPart, HistoryEntry, Replay, ToolCall, ToolOutcome,
};

pub(crate) const ROUTE: &str = "gemini";

pub(crate) fn encode(request: &ModelRequest) -> Encoded {
    let mut names = HashMap::new();
    for entry in &request.history {
        if let HistoryEntry::Assistant(assistant) = entry {
            for call in assistant.tool_calls() {
                names.insert(call.id.0.clone(), call.name.clone());
            }
        }
    }
    let mut contents: Vec<(&str, Vec<Value>)> = Vec::new();
    let mut push = |role: &'static str, part: Value| match contents.last_mut() {
        Some((last, parts)) if *last == role => parts.push(part),
        _ => contents.push((role, vec![part])),
    };
    if let Some(summary) = &request.summary {
        push("user", json!({"text": summary_text(summary)}));
    }
    for entry in &request.history {
        match entry {
            HistoryEntry::User(user) => {
                push("user", json!({"text": user.text}));
                for image in &user.images {
                    push(
                        "user",
                        json!({"inlineData": {
                            "mimeType": image.mime_type(), "data": image.data(),
                        }}),
                    );
                }
            }
            HistoryEntry::Assistant(assistant) => {
                let mut signature = None;
                for part in &assistant.parts {
                    let mut encoded = match part {
                        AssistantPart::Replay(replay) => {
                            if replay.route == ROUTE {
                                signature = replay.data.get("thoughtSignature").cloned();
                            }
                            continue;
                        }
                        AssistantPart::Text(text) => json!({"text": text}),
                        AssistantPart::ToolCall(call) => {
                            let args = match &call.arguments {
                                Value::Object(_) => call.arguments.clone(),
                                _ => Value::Object(Map::new()),
                            };
                            json!({"functionCall": {"name": call.name, "args": args}})
                        }
                    };
                    if let Some(signature) = signature.take() {
                        encoded["thoughtSignature"] = signature;
                    }
                    push("model", encoded);
                }
            }
            HistoryEntry::ToolResult(result) => {
                let name = names.get(&result.call_id.0).cloned().unwrap_or_default();
                let response = match &result.outcome {
                    ToolOutcome::Success(text) => json!({"output": text}),
                    other => json!({"error": result_text(other)}),
                };
                push(
                    "user",
                    json!({"functionResponse": {"name": name, "response": response}}),
                );
            }
        }
    }
    let contents: Vec<Value> = contents
        .into_iter()
        .map(|(role, parts)| json!({"role": role, "parts": parts}))
        .collect();
    let mut body = json!({
        "systemInstruction": {"parts": [{"text": request.system_instruction}]},
        "contents": contents,
    });
    if !request.tools.is_empty() {
        let declarations: Vec<Value> = request
            .tools
            .iter()
            .map(|tool| {
                json!({"name": tool.name, "description": tool.description, "parametersJsonSchema": tool.input_schema})
            })
            .collect();
        body["tools"] = json!([{"functionDeclarations": declarations}]);
    }
    if let Some(limit) = request.max_output_tokens {
        body["generationConfig"] = json!({"maxOutputTokens": limit});
    }
    Encoded {
        path: format!(
            "/{}/v1beta/models/{}:streamGenerateContent?alt=sse",
            request.provider, request.model
        ),
        body,
        headers: Vec::new(),
    }
}

/// A session-unique ID for a call Gemini left without one.
fn assign_call_id() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let started = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|time| time.as_millis())
        .unwrap_or_default();
    format!(
        "gemini-{started:x}-{}",
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

#[derive(Default)]
pub(crate) struct Stream {
    parts: Vec<AssistantPart>,
    finish_reason: Option<String>,
    usage: Option<TokenUsage>,
}

impl Assembler for Stream {
    fn event(&mut self, event: &SseEvent) -> Result<(), ModelError> {
        let Ok(data) = serde_json::from_str::<Value>(&event.data) else {
            return Ok(());
        };
        if let Some(error) = data.get("error") {
            return Err(stream_error(
                error["status"].as_str().unwrap_or_default(),
                error["message"]
                    .as_str()
                    .unwrap_or("the stream reported an error"),
            ));
        }
        let usage = &data["usageMetadata"];
        if let Some(input) = usage["promptTokenCount"].as_u64() {
            self.usage = Some(TokenUsage {
                input_tokens: input,
                output_tokens: usage["candidatesTokenCount"].as_u64().unwrap_or_default(),
            });
        }
        let candidate = &data["candidates"][0];
        for part in candidate["content"]["parts"]
            .as_array()
            .into_iter()
            .flatten()
        {
            if let Some(signature) = part.get("thoughtSignature") {
                self.parts.push(AssistantPart::Replay(Replay {
                    route: ROUTE.to_owned(),
                    data: json!({"thoughtSignature": signature}),
                }));
            }
            if part["thought"].as_bool() == Some(true) {
                continue;
            }
            if let Some(text) = part["text"].as_str() {
                match self.parts.last_mut() {
                    Some(AssistantPart::Text(previous)) => previous.push_str(text),
                    _ => self.parts.push(AssistantPart::Text(text.to_owned())),
                }
            } else if let Some(call) = part.get("functionCall") {
                let id = call["id"]
                    .as_str()
                    .map_or_else(assign_call_id, str::to_owned);
                self.parts.push(AssistantPart::ToolCall(ToolCall {
                    id: id.as_str().into(),
                    name: call["name"].as_str().unwrap_or_default().to_owned(),
                    arguments: call.get("args").cloned().unwrap_or(json!({})),
                }));
            }
        }
        if let Some(reason) = candidate["finishReason"].as_str() {
            self.finish_reason = Some(reason.to_owned());
        }
        Ok(())
    }

    fn finish(self: Box<Self>) -> Result<(AssistantEntry, Option<TokenUsage>), ModelError> {
        match self.finish_reason.as_deref() {
            None => Err(ModelError::Incomplete(
                "the stream ended before the response completed".to_owned(),
            )),
            Some("MAX_TOKENS") => Err(ModelError::Incomplete(
                "the output limit was reached".to_owned(),
            )),
            Some("STOP") => Ok((AssistantEntry { parts: self.parts }, self.usage)),
            Some(other) => Err(ModelError::Provider(format!(
                "the response ended with {other}"
            ))),
        }
    }
}
