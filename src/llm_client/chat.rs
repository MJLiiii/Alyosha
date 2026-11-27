//! The Chat Completions route, used by the `deepseek` channel.
//!
//! DeepSeek's thinking mode with tools needs each assistant message's
//! `reasoning_content` on later requests, so it is kept as a replay part.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::{
    ModelError, ModelRequest, Piece, TokenUsage, data_url,
    gateway::{Assembler, Encoded, arguments_text, parse_arguments, stream_error},
    result_text,
    sse::SseEvent,
    summary_text, user_pieces,
};
use crate::agent_state::{AssistantEntry, AssistantPart, HistoryEntry, Replay, ToolCall};

pub(crate) const ROUTE: &str = "chat";

pub(crate) fn encode(request: &ModelRequest) -> Encoded {
    let mut messages = vec![json!({"role": "system", "content": request.system_instruction})];
    if let Some(summary) = &request.summary {
        messages.push(json!({"role": "user", "content": summary_text(summary)}));
    }
    for entry in &request.history {
        match entry {
            HistoryEntry::User(user) => {
                let pieces = user_pieces(user, request);
                let content = match pieces.as_slice() {
                    [Piece::Text(text)] => json!(text),
                    _ => Value::Array(
                        pieces
                            .iter()
                            .map(|piece| match piece {
                                Piece::Text(text) => json!({"type": "text", "text": text}),
                                Piece::Image(data) => {
                                    json!({"type": "image_url", "image_url": {"url": data_url(data)}})
                                }
                            })
                            .collect(),
                    ),
                };
                messages.push(json!({"role": "user", "content": content}));
            }
            HistoryEntry::Assistant(assistant) => {
                let mut message = json!({"role": "assistant", "content": assistant.text()});
                let calls: Vec<Value> = assistant
                    .tool_calls()
                    .map(|call| {
                        json!({
                            "id": call.id.0,
                            "type": "function",
                            "function": {"name": call.name, "arguments": arguments_text(&call.arguments)},
                        })
                    })
                    .collect();
                if !calls.is_empty() {
                    message["tool_calls"] = Value::Array(calls);
                }
                for part in &assistant.parts {
                    if let AssistantPart::Replay(replay) = part
                        && replay.route == ROUTE
                        && let Some(reasoning) = replay.data.get("reasoning_content")
                    {
                        message["reasoning_content"] = reasoning.clone();
                    }
                }
                messages.push(message);
            }
            HistoryEntry::ToolResult(result) => messages.push(json!({
                "role": "tool",
                "tool_call_id": result.call_id.0,
                "content": result_text(&result.outcome),
            })),
        }
    }
    let mut body = json!({
        "model": request.model,
        "messages": messages,
        "stream": true,
        "stream_options": {"include_usage": true},
    });
    if !request.tools.is_empty() {
        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "type": "function",
                    "function": {"name": tool.name, "description": tool.description, "parameters": tool.input_schema},
                })
            })
            .collect();
        body["tools"] = Value::Array(tools);
    }
    if let Some(limit) = request.max_output_tokens {
        body["max_tokens"] = json!(limit);
    }
    Encoded {
        path: format!("/{}/v1/chat/completions", request.provider),
        body,
        headers: Vec::new(),
    }
}

#[derive(Default)]
struct PendingCall {
    id: String,
    name: String,
    arguments: String,
}

#[derive(Default)]
pub(crate) struct Stream {
    content: String,
    reasoning: String,
    calls: BTreeMap<u64, PendingCall>,
    finish_reason: Option<String>,
    usage: Option<TokenUsage>,
}

impl Assembler for Stream {
    fn event(&mut self, event: &SseEvent) -> Result<(), ModelError> {
        if event.data.trim() == "[DONE]" {
            return Ok(());
        }
        let Ok(data) = serde_json::from_str::<Value>(&event.data) else {
            return Ok(());
        };
        if let Some(error) = data.get("error") {
            return Err(stream_error(
                error["code"]
                    .as_str()
                    .or(error["type"].as_str())
                    .unwrap_or_default(),
                error["message"]
                    .as_str()
                    .unwrap_or("the stream reported an error"),
            ));
        }
        if let (Some(input), Some(output)) = (
            data["usage"]["prompt_tokens"].as_u64(),
            data["usage"]["completion_tokens"].as_u64(),
        ) {
            self.usage = Some(TokenUsage {
                input_tokens: input,
                output_tokens: output,
            });
        }
        for choice in data["choices"].as_array().into_iter().flatten() {
            let delta = &choice["delta"];
            if let Some(text) = delta["content"].as_str() {
                self.content.push_str(text);
            }
            if let Some(text) = delta["reasoning_content"].as_str() {
                self.reasoning.push_str(text);
            }
            for call in delta["tool_calls"].as_array().into_iter().flatten() {
                let index = call["index"].as_u64().unwrap_or(self.calls.len() as u64);
                let pending = self.calls.entry(index).or_default();
                if let Some(id) = call["id"].as_str() {
                    pending.id = id.to_owned();
                }
                if let Some(name) = call["function"]["name"].as_str() {
                    pending.name.push_str(name);
                }
                if let Some(arguments) = call["function"]["arguments"].as_str() {
                    pending.arguments.push_str(arguments);
                }
            }
            if let Some(reason) = choice["finish_reason"].as_str() {
                self.finish_reason = Some(reason.to_owned());
            }
        }
        Ok(())
    }

    fn finish(self: Box<Self>) -> Result<(AssistantEntry, Option<TokenUsage>), ModelError> {
        match self.finish_reason.as_deref() {
            None => {
                return Err(ModelError::Incomplete(
                    "the stream ended before the response completed".to_owned(),
                ));
            }
            Some("length") => {
                return Err(ModelError::Incomplete(
                    "the output limit was reached".to_owned(),
                ));
            }
            Some("stop" | "tool_calls" | "function_call") => {}
            Some(other) => {
                return Err(ModelError::Provider(format!(
                    "the response ended with {other}"
                )));
            }
        }
        let mut parts = Vec::new();
        if !self.reasoning.is_empty() {
            parts.push(AssistantPart::Replay(Replay {
                route: ROUTE.to_owned(),
                data: json!({"reasoning_content": self.reasoning}),
            }));
        }
        if !self.content.is_empty() {
            parts.push(AssistantPart::Text(self.content));
        }
        for call in self.calls.into_values() {
            parts.push(AssistantPart::ToolCall(ToolCall {
                id: call.id.as_str().into(),
                name: call.name,
                arguments: parse_arguments(&call.arguments),
            }));
        }
        Ok((AssistantEntry { parts }, self.usage))
    }
}
