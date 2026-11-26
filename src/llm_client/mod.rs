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

use std::time::Duration;

use tokio_util::sync::CancellationToken;

pub use claude::DEFAULT_MAX_TOKENS as CLAUDE_DEFAULT_MAX_TOKENS;
pub(crate) use gateway::error_message;
pub use gateway::{GatewayClient, Timeouts};

use crate::{
    BoxFuture,
    agent_state::{AssistantEntry, HistoryEntry, ToolOutcome},
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
