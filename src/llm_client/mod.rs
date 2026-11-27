//! LLM Client / Gateway: the `generate` contract and the client that sends
//! requests through the local GPROXY service.
//!
//! The client encodes each request for the provider's native API, streams the
//! response, and returns it only once the provider signals that the whole
//! response completed. See `docs/LLMClientGateway.md#implementation`.

mod chat;
mod claude;
mod gateway;
mod gemini;
mod responses;
mod sse;

use std::{collections::HashMap, time::Duration};

use tokio_util::sync::CancellationToken;

pub use claude::DEFAULT_MAX_TOKENS as CLAUDE_DEFAULT_MAX_TOKENS;
pub(crate) use gateway::error_message;
pub use gateway::{GatewayClient, Timeouts};

use crate::{
    BoxFuture,
    agent_state::{AssistantEntry, HistoryEntry, ToolOutcome, UserEntry, UserPart},
    tools::ToolDefinition,
};

/// What every Alyosha program says when the provider is not signed in.
pub const NOT_SIGNED_IN: &str =
    "The model provider is not signed in. Run `alyosha gateway login` in a terminal.";

/// Sends one prepared request and returns a complete reply or an error.
pub trait ModelClient: Send + Sync {
    /// Waits for the whole response. A broken stream or an output limit is an
    /// error, and the partial reply is discarded. `cancel` stops the request.
    fn generate(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
    ) -> BoxFuture<'_, Result<ModelReply, ModelError>>;
}

/// One model request.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelRequest {
    /// The GPROXY provider name, pinned for the session.
    pub provider: String,
    /// The model ID, pinned for the turn.
    pub model: String,
    /// The normal system instruction, or a summary request's instructions.
    pub system_instruction: String,
    /// A checkpoint summary, sent before the history as conversation context.
    pub summary: Option<String>,
    /// The history entries selected for this request, in order.
    pub history: Vec<HistoryEntry>,
    /// The registry's definitions for a normal request; empty for a summary.
    pub tools: Vec<ToolDefinition>,
    /// `None` uses the route's default output allowance.
    pub max_output_tokens: Option<u32>,
    /// The bytes of the images in `history`, by hash, loaded by the caller
    /// from the media table (`docs/Media.md#context-and-compaction`).
    pub media: HashMap<String, MediaData>,
}

/// One image's bytes, ready to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaData {
    pub mime: String,
    /// The bytes in standard base64.
    pub base64: String,
}

/// One piece of a user message as a route encodes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Piece<'a> {
    Text(String),
    Image(&'a MediaData),
}

/// A user entry's pieces in order. An entry without images is one text
/// piece, so it encodes exactly as before images existed. Adjacent text
/// parts are joined by a line break.
pub(crate) fn user_pieces<'a>(user: &UserEntry, request: &'a ModelRequest) -> Vec<Piece<'a>> {
    let mut pieces = Vec::new();
    let mut text: Option<String> = None;
    for part in &user.parts {
        match part {
            UserPart::Text(part) => match &mut text {
                Some(text) => {
                    text.push('\n');
                    text.push_str(part);
                }
                None => text = Some(part.clone()),
            },
            UserPart::Image(image) => {
                if let Some(text) = text.take() {
                    pieces.push(Piece::Text(text));
                }
                match request.media.get(&image.sha256) {
                    Some(data) => pieces.push(Piece::Image(data)),
                    // The caller checks every image before sending.
                    None => pieces.push(Piece::Text(crate::agent_state::IMAGE_PLACEHOLDER.into())),
                }
            }
        }
    }
    if let Some(text) = text {
        pieces.push(Piece::Text(text));
    }
    if pieces.is_empty() {
        pieces.push(Piece::Text(String::new()));
    }
    pieces
}

/// `data:<mime>;base64,<data>`.
pub(crate) fn data_url(data: &MediaData) -> String {
    format!("data:{};base64,{}", data.mime, data.base64)
}

/// A complete reply.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelReply {
    /// The assistant's text and tool calls in their original order.
    pub entry: AssistantEntry,
    pub usage: Option<TokenUsage>,
    /// The upstream request ID, when the gateway supplies one.
    pub request_id: Option<String>,
}

/// Token usage reported by the provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// Why a model request failed. The display text names the kind first, so it
/// can serve as a turn's failure reason.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ModelError {
    #[error("cancelled")]
    Cancelled,
    #[error("timeout: {0}")]
    Timeout(String),
    /// A disconnection, a broken stream, or truncated output.
    #[error("incomplete response: {0}")]
    Incomplete(String),
    /// The provider explicitly reported that the request exceeds the context
    /// window.
    #[error("context window exceeded: {0}")]
    ContextOverflow(String),
    #[error("unsupported request: {0}")]
    Unsupported(String),
    /// A missing provider, an expired login, or an invalid key.
    #[error("not signed in: {NOT_SIGNED_IN}")]
    NotSignedIn,
    #[error("rate limited: {message}")]
    RateLimited {
        message: String,
        retry_after: Option<Duration>,
    },
    /// Nothing answered at the gateway's address.
    #[error("gateway unreachable: {0}")]
    Unreachable(String),
    /// Any other error status from the gateway or the provider.
    #[error("provider error: {0}")]
    Provider(String),
}

/// The text a tool result carries to the model.
pub fn result_text(outcome: &ToolOutcome) -> String {
    match outcome {
        ToolOutcome::Success(text) => text.clone(),
        ToolOutcome::Error(message) => format!("Error: {message}"),
        ToolOutcome::Cancelled => {
            "Cancelled: the owner stopped the turn, so this call did not finish.".to_owned()
        }
    }
}

/// How the conversation summary is introduced to the model.
pub(crate) fn summary_text(summary: &str) -> String {
    format!("Summary of the earlier conversation, written for you to continue from:\n\n{summary}")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::agent_state::{MediaRef, MessageId, MessageOrigin};

    /// A user entry of text, an image, and more text, with its bytes.
    fn request() -> ModelRequest {
        let image = MediaRef {
            sha256: "abc".into(),
            mime: "image/png".into(),
            width: 1,
            height: 1,
        };
        ModelRequest {
            provider: "p".into(),
            model: "m".into(),
            system_instruction: "s".into(),
            summary: None,
            history: vec![HistoryEntry::User(UserEntry {
                message_id: MessageId(1),
                origin: MessageOrigin::Owner,
                parts: vec![
                    UserPart::Text("[clock]\nlook".into()),
                    UserPart::Text("[photo: inbox/a.png]".into()),
                    UserPart::Image(image),
                    UserPart::Text("after".into()),
                ],
            })],
            tools: Vec::new(),
            max_output_tokens: None,
            media: HashMap::from([(
                "abc".to_owned(),
                MediaData {
                    mime: "image/png".into(),
                    base64: "QUJD".into(),
                },
            )]),
        }
    }

    #[test]
    fn each_route_puts_the_image_between_its_texts() {
        let request = request();
        let responses = responses::encode(&request, true, None);
        assert_eq!(
            responses.body["input"][0]["content"],
            json!([
                {"type": "input_text", "text": "[clock]\nlook\n[photo: inbox/a.png]"},
                {"type": "input_image", "image_url": "data:image/png;base64,QUJD", "detail": "auto"},
                {"type": "input_text", "text": "after"},
            ])
        );
        let claude = claude::encode(&request);
        assert_eq!(
            claude.body["messages"][0]["content"][1],
            json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "QUJD"}})
        );
        let chat = chat::encode(&request);
        assert_eq!(
            chat.body["messages"][1]["content"][1],
            json!({"type": "image_url", "image_url": {"url": "data:image/png;base64,QUJD"}})
        );
        let gemini = gemini::encode(&request);
        assert_eq!(
            gemini.body["contents"][0]["parts"][1],
            json!({"inline_data": {"mime_type": "image/png", "data": "QUJD"}})
        );
    }

    #[test]
    fn text_only_entries_encode_as_before() {
        let mut request = request();
        request.history = vec![HistoryEntry::User(UserEntry::text_only(
            MessageId(1),
            MessageOrigin::Owner,
            "hi",
        ))];
        assert_eq!(chat::encode(&request).body["messages"][1]["content"], json!("hi"));
        assert_eq!(
            responses::encode(&request, true, None).body["input"][0]["content"],
            json!([{"type": "input_text", "text": "hi"}])
        );
    }
}
