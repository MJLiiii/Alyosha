//! The Claude Messages route, used by the `claudeapi` channel.

use serde_json::{Map, Value, json};

use super::{
    ModelError, ModelRequest, TokenUsage,
    gateway::{Assembler, Encoded, parse_arguments, stream_error},
    result_text,
    sse::SseEvent,
    summary_text,
};
use crate::agent_state::{
    AssistantEntry, AssistantPart, HistoryEntry, Replay, ToolCall, ToolOutcome,
};

pub(crate) const ROUTE: &str = "claude";

/// The Messages API requires an output limit and publishes no default, so
/// ordinary requests reserve this much.
pub const DEFAULT_MAX_TOKENS: u32 = 32_000;

pub(crate) fn encode(request: &ModelRequest) -> Encoded {
    let mut messages: Vec<(String, Vec<Value>)> = Vec::new();
    let mut push = |role: &str, block: Value| match messages.last_mut() {
        // The API wants alternating roles, so adjacent blocks of one role
        // share a message.
        Some((last, blocks)) if last == role => blocks.push(block),
        _ => messages.push((role.to_owned(), vec![block])),
    };
    if let Some(summary) = &request.summary {
        push(
            "user",
            json!({"type": "text", "text": summary_text(summary)}),
        );
    }
    for entry in &request.history {
        match entry {
            HistoryEntry::User(user) => push("user", json!({"type": "text", "text": user.text})),
            HistoryEntry::Assistant(assistant) => {
                for part in &assistant.parts {
                    match part {
                        AssistantPart::Text(text) if !text.is_empty() => {
                            push("assistant", json!({"type": "text", "text": text}));
                        }
                        AssistantPart::Text(_) => {}
                        AssistantPart::ToolCall(call) => {
                            let input = match &call.arguments {
                                Value::Object(_) => call.arguments.clone(),
                                _ => Value::Object(Map::new()),
                            };
                            push(
                                "assistant",
                                json!({"type": "tool_use", "id": call.id.0, "name": call.name, "input": input}),
                            );
                        }
                        AssistantPart::Replay(replay) if replay.route == ROUTE => {
                            push("assistant", replay.data.clone());
                        }
                        AssistantPart::Replay(_) => {}
                    }
                }
            }
            HistoryEntry::ToolResult(result) => push(
                "user",
                json!({
                    "type": "tool_result",
                    "tool_use_id": result.call_id.0,
                    "content": result_text(&result.outcome),
                    "is_error": !matches!(result.outcome, ToolOutcome::Success(_)),
                }),
            ),
        }
    }
    let messages: Vec<Value> = messages
        .into_iter()
        .map(|(role, content)| json!({"role": role, "content": content}))
        .collect();
    let mut body = json!({
        "model": request.model,
        "max_tokens": request.max_output_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
        "system": request.system_instruction,
        "messages": messages,
        "stream": true,
    });
    if !request.tools.is_empty() {
        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(|tool| {
                json!({"name": tool.name, "description": tool.description, "input_schema": tool.input_schema})
            })
            .collect();
        body["tools"] = Value::Array(tools);
    }
    Encoded {
        path: format!("/{}/v1/messages", request.provider),
        body,
        headers: vec![("anthropic-version", "2023-06-01")],
    }
}

enum Block {
    Text(String),
    ToolUse {
        id: String,
        name: String,
        json: String,
    },
    /// Thinking and other blocks, replayed as they arrived.
    Other(Value),
}

#[derive(Default)]
pub(crate) struct Stream {
    blocks: Vec<Block>,
    stop_reason: Option<String>,
    stopped: bool,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
}

impl Assembler for Stream {
    fn event(&mut self, event: &SseEvent) -> Result<(), ModelError> {
        let Ok(data) = serde_json::from_str::<Value>(&event.data) else {
            return Ok(());
        };
        match data["type"].as_str().unwrap_or_default() {
            "message_start" => {
                let usage = &data["message"]["usage"];
                self.input_tokens = usage["input_tokens"].as_u64();
                self.output_tokens = usage["output_tokens"].as_u64();
            }
            "content_block_start" => {
                let block = &data["content_block"];
                self.blocks
                    .push(match block["type"].as_str().unwrap_or_default() {
                        "text" => {
                            Block::Text(block["text"].as_str().unwrap_or_default().to_owned())
                        }
                        "tool_use" => Block::ToolUse {
                            id: block["id"].as_str().unwrap_or_default().to_owned(),
                            name: block["name"].as_str().unwrap_or_default().to_owned(),
                            json: String::new(),
                        },
                        _ => Block::Other(block.clone()),
                    });
            }
            "content_block_delta" => {
                let delta = &data["delta"];
                match (
                    self.blocks.last_mut(),
                    delta["type"].as_str().unwrap_or_default(),
                ) {
                    (Some(Block::Text(text)), "text_delta") => {
                        text.push_str(delta["text"].as_str().unwrap_or_default());
                    }
                    (Some(Block::ToolUse { json, .. }), "input_json_delta") => {
                        json.push_str(delta["partial_json"].as_str().unwrap_or_default());
                    }
                    (Some(Block::Other(block)), kind) => {
                        let (field, value) = match kind {
                            "thinking_delta" => ("thinking", &delta["thinking"]),
                            "signature_delta" => ("signature", &delta["signature"]),
                            _ => return Ok(()),
                        };
                        let current = block[field].as_str().unwrap_or_default().to_owned();
                        block[field] = json!(current + value.as_str().unwrap_or_default());
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                if let Some(reason) = data["delta"]["stop_reason"].as_str() {
                    self.stop_reason = Some(reason.to_owned());
                }
                if let Some(output) = data["usage"]["output_tokens"].as_u64() {
                    self.output_tokens = Some(output);
                }
            }
            "message_stop" => self.stopped = true,
            "error" => {
                let error = &data["error"];
                let kind = error["type"].as_str().unwrap_or_default();
                let message = error["message"]
                    .as_str()
                    .unwrap_or("the stream reported an error");
                if kind == "overloaded_error" || kind == "api_error" {
                    return Err(ModelError::Provider(message.to_owned()));
                }
                return Err(stream_error(kind, message));
            }
            _ => {}
        }
        Ok(())
    }

    fn finish(self: Box<Self>) -> Result<(AssistantEntry, Option<TokenUsage>), ModelError> {
        if !self.stopped {
            return Err(ModelError::Incomplete(
                "the stream ended before the response completed".to_owned(),
            ));
        }
        if self.stop_reason.as_deref() == Some("max_tokens") {
            return Err(ModelError::Incomplete(
                "the output limit was reached".to_owned(),
            ));
        }
        let parts = self
            .blocks
            .into_iter()
            .map(|block| match block {
                Block::Text(text) => AssistantPart::Text(text),
                Block::ToolUse { id, name, json } => AssistantPart::ToolCall(ToolCall {
                    id: id.as_str().into(),
                    name,
                    arguments: parse_arguments(&json),
                }),
                Block::Other(data) => AssistantPart::Replay(Replay {
                    route: ROUTE.to_owned(),
                    data,
                }),
            })
            .collect();
        let usage = match (self.input_tokens, self.output_tokens) {
            (Some(input_tokens), Some(output_tokens)) => Some(TokenUsage {
                input_tokens,
                output_tokens,
            }),
            _ => None,
        };
        Ok((AssistantEntry { parts }, usage))
    }
}
