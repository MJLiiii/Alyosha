//! The OpenAI Responses route, used by the `codex` and `openai` channels.
//!
//! Requests stream, set `store: false`, and ask for encrypted reasoning, whose
//! items go back in place with the tool results.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::{
    ModelError, ModelRequest, Piece, TokenUsage, data_url,
    gateway::{Assembler, Encoded, arguments_text, parse_arguments, stream_error},
    result_images, result_text,
    sse::SseEvent,
    summary_text, user_pieces,
};
use crate::agent_state::{AssistantEntry, AssistantPart, HistoryEntry, Replay, ToolCall};

pub(crate) const ROUTE: &str = "responses";

/// `with_output_limit` is false for the `codex` channel, whose backend
/// accepts no output limit.
pub(crate) fn encode(
    request: &ModelRequest,
    with_output_limit: bool,
    effort: Option<alyosha_config::Effort>,
) -> Encoded {
    let mut input = Vec::new();
    if let Some(summary) = &request.summary {
        input.push(user_message(&summary_text(summary)));
    }
    for entry in &request.history {
        match entry {
            HistoryEntry::User(user) => {
                let content: Vec<Value> = user_pieces(user, request)
                    .into_iter()
                    .map(|piece| match piece {
                        Piece::Text(text) => json!({"type": "input_text", "text": text}),
                        Piece::Image(data) => json!({"type": "input_image", "image_url": data_url(data), "detail": "auto"}),
                    })
                    .collect();
                input.push(json!({"type": "message", "role": "user", "content": content}));
            }
            HistoryEntry::Assistant(assistant) => {
                for part in &assistant.parts {
                    match part {
                        AssistantPart::Text(text) => input.push(json!({
                            "type": "message",
                            "role": "assistant",
                            "content": [{"type": "output_text", "text": text}],
                        })),
                        AssistantPart::ToolCall(call) => input.push(json!({
                            "type": "function_call",
                            "call_id": call.id.0,
                            "name": call.name,
                            "arguments": arguments_text(&call.arguments),
                        })),
                        AssistantPart::Replay(replay) if replay.route == ROUTE => {
                            input.push(replay.data.clone());
                        }
                        AssistantPart::Replay(_) => {}
                    }
                }
            }
            HistoryEntry::ToolResult(result) => {
                let text = result_text(&result.outcome);
                let output = if result.images.is_empty() {
                    json!(text)
                } else {
                    let mut parts = vec![json!({"type": "input_text", "text": text})];
                    for piece in result_images(result, request) {
                        parts.push(match piece {
                            Piece::Text(text) => json!({"type": "input_text", "text": text}),
                            Piece::Image(data) => json!({"type": "input_image", "image_url": data_url(data), "detail": "auto"}),
                        });
                    }
                    Value::Array(parts)
                };
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": result.call_id.0,
                    "output": output,
                }));
            }
        }
    }
    let mut body = json!({
        "model": request.model,
        "instructions": request.system_instruction,
        "input": input,
        "stream": true,
        "store": false,
        "include": ["reasoning.encrypted_content"],
    });
    if let Some(effort) = effort {
        body["reasoning"] = json!({"effort": effort.as_str()});
    }
    if !request.tools.is_empty() {
        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "type": "function",
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.input_schema,
                    "strict": false,
                })
            })
            .collect();
        body["tools"] = Value::Array(tools);
        body["tool_choice"] = json!("auto");
    }
    if with_output_limit && let Some(limit) = request.max_output_tokens {
        body["max_output_tokens"] = json!(limit);
    }
    Encoded {
        path: format!("/{}/v1/responses", request.provider),
        body,
        headers: Vec::new(),
    }
}

fn user_message(text: &str) -> Value {
    json!({
        "type": "message",
        "role": "user",
        "content": [{"type": "input_text", "text": text}],
    })
}

/// Collects finished output items until `response.completed`.
#[derive(Default)]
pub(crate) struct Stream {
    items: BTreeMap<u64, Value>,
    completed: bool,
    usage: Option<TokenUsage>,
}

impl Assembler for Stream {
    fn event(&mut self, event: &SseEvent) -> Result<(), ModelError> {
        let Ok(data) = serde_json::from_str::<Value>(&event.data) else {
            return Ok(());
        };
        let kind = data["type"]
            .as_str()
            .or(event.name.as_deref())
            .unwrap_or_default();
        match kind {
            "response.output_item.done" => {
                let index = data["output_index"]
                    .as_u64()
                    .unwrap_or(self.items.len() as u64);
                self.items.insert(index, data["item"].clone());
            }
            "response.completed" => {
                self.completed = true;
                let usage = &data["response"]["usage"];
                if let (Some(input), Some(output)) = (
                    usage["input_tokens"].as_u64(),
                    usage["output_tokens"].as_u64(),
                ) {
                    self.usage = Some(TokenUsage {
                        input_tokens: input,
                        output_tokens: output,
                    });
                }
            }
            "response.incomplete" => {
                let reason = data["response"]["incomplete_details"]["reason"]
                    .as_str()
                    .unwrap_or("unknown");
                return Err(ModelError::Incomplete(format!(
                    "the response stopped early ({reason})"
                )));
            }
            "response.failed" => {
                let error = &data["response"]["error"];
                return Err(stream_error(
                    error["code"].as_str().unwrap_or_default(),
                    error["message"].as_str().unwrap_or("the response failed"),
                ));
            }
            "error" => {
                let error = data.get("error").unwrap_or(&data);
                return Err(stream_error(
                    error["code"].as_str().unwrap_or_default(),
                    error["message"]
                        .as_str()
                        .unwrap_or("the stream reported an error"),
                ));
            }
            _ => {}
        }
        Ok(())
    }

    fn finish(self: Box<Self>) -> Result<(AssistantEntry, Option<TokenUsage>), ModelError> {
        if !self.completed {
            return Err(ModelError::Incomplete(
                "the stream ended before the response completed".to_owned(),
            ));
        }
        let mut parts = Vec::new();
        for item in self.items.into_values() {
            match item["type"].as_str().unwrap_or_default() {
                "message" => {
                    for content in item["content"].as_array().into_iter().flatten() {
                        let text = content["text"].as_str().or(content["refusal"].as_str());
                        if let Some(text) = text {
                            parts.push(AssistantPart::Text(text.to_owned()));
                        }
                    }
                }
                "function_call" => parts.push(AssistantPart::ToolCall(ToolCall {
                    id: item["call_id"].as_str().unwrap_or_default().into(),
                    name: item["name"].as_str().unwrap_or_default().to_owned(),
                    arguments: parse_arguments(item["arguments"].as_str().unwrap_or_default()),
                })),
                _ => parts.push(AssistantPart::Replay(Replay {
                    route: ROUTE.to_owned(),
                    data: item,
                })),
            }
        }
        Ok((AssistantEntry { parts }, self.usage))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> ModelRequest {
        ModelRequest {
            provider: "chatgpt".to_owned(),
            model: "test-model".to_owned(),
            system_instruction: "Be brief.".to_owned(),
            summary: None,
            history: Vec::new(),
            tools: Vec::new(),
            max_output_tokens: None,
            media: Default::default(),
        }
    }

    #[test]
    fn effort_is_sent_only_when_configured() {
        let none = encode(&request(), false, None);
        assert!(none.body.get("reasoning").is_none());
        let high = encode(&request(), false, Some(alyosha_config::Effort::High));
        assert_eq!(high.body["reasoning"], json!({"effort": "high"}));
        assert_eq!(high.body["include"], json!(["reasoning.encrypted_content"]));
    }
}
