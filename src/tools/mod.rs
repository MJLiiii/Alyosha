//! Tools: the types shared by the Tool Registry, the Tool Executor, and the
//! tool handlers.
//!
//! Designs: `docs/ToolRegistry.md` and `docs/ToolExecutor.md`.
//! Built-in tool contracts: `docs/reference/BuiltInTools.md`.

pub mod builtin;
pub mod code_mode;
pub mod executor;
pub mod registry;

use std::{future::Future, sync::Arc};

use serde_json::Value;
use tokio_util::sync::CancellationToken;

pub use executor::ToolExecutor;
pub use registry::{RegisteredTool, RegistrationError, ToolRegistry};

use crate::BoxFuture;

/// What the model sees of a tool.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    /// A JSON Schema for the call's arguments, whose root is an object.
    pub input_schema: Value,
    /// Set from an MCP server's `ask` list. False for built-in tools.
    pub requires_approval: bool,
}

/// Which tools a caller may use. The Environment Interface enforces the file
/// and process limits that go with each scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// A call the model made: any registered tool, with the session's tool
    /// permissions.
    Direct,
    /// A read requested by an `execute_code` script: `read_file` only,
    /// limited to regular workspace files.
    CodeMode,
}

impl Scope {
    /// Whether a caller with this scope may use the named tool.
    pub fn allows(self, tool_name: &str) -> bool {
        match self {
            Scope::Direct => true,
            Scope::CodeMode => tool_name == "read_file",
        }
    }
}

/// What a handler receives with an admitted call.
#[derive(Clone)]
pub struct ToolContext {
    /// The caller's scope.
    pub scope: Scope,
    /// The active work's Stop signal.
    pub cancel: CancellationToken,
    /// The session's environment and stores.
    pub session: Arc<SessionServices>,
    /// The dispatcher that admitted this call, for Code Mode's inner reads.
    pub executor: ToolExecutor,
    /// The model's ID for a direct call.
    pub call_id: Option<crate::agent_state::CallId>,
}

impl std::fmt::Debug for ToolContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolContext")
            .field("scope", &self.scope)
            .field("call_id", &self.call_id)
            .finish_non_exhaustive()
    }
}

impl ToolContext {
    /// The session's environment, which every file and shell tool needs.
    pub fn environment(&self) -> Result<&crate::environment::Environment, ToolError> {
        self.session
            .environment
            .as_ref()
            .ok_or_else(|| ToolError::failed("This session has no workspace."))
    }

    /// Shows a line in the status area, outside the conversation.
    pub fn notice(&self, text: impl Into<String>) {
        if let Some(notify) = &self.session.notify {
            notify(text.into());
        }
    }
}

/// Sends a notice to the session's front end.
pub type Notify = Arc<dyn Fn(String) + Send + Sync>;

/// What a session gives its tools. Each part is optional so that tests can
/// run a session with only fake tools.
#[derive(Default)]
pub struct SessionServices {
    pub environment: Option<crate::environment::Environment>,
    pub memory: Option<crate::memory::MemoryStore>,
    pub skills: Option<crate::skills::SkillStore>,
    pub jobs: Option<crate::jobs::JobManager>,
    pub approver: Option<Arc<dyn crate::approvals::Approver>>,
    /// How long an approval or question waits; 60 minutes when unset.
    pub approval_timeout: Option<std::time::Duration>,
    pub mcp: Option<crate::mcp::McpManager>,
    pub notify: Option<Notify>,
    /// In a guest session, the notes kept on each person.
    pub users: Option<crate::user_notes::UserNotes>,
    /// In a guest session, the sender of the message the active turn answers.
    pub speaker: std::sync::Mutex<Option<crate::agent_state::Sender>>,
    /// Prepares attachments; without it they become a line saying so.
    pub media: Option<Arc<crate::media::MediaServices>>,
    /// Where `send_file` puts files for the bridge. The driver sets how it
    /// tells the bridge.
    pub outbox: Option<crate::media::send_file::Outbox>,
    /// Searches saved sessions; only in sessions with a store.
    pub recall: Option<crate::recall::Recall>,
}

impl SessionServices {
    /// The sender of the message the active turn answers.
    pub fn speaker(&self) -> Option<crate::agent_state::Sender> {
        self.speaker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// A handler's successful result.
#[derive(Debug, Default)]
pub struct ToolOutput {
    /// What the model receives, before the Agent Loop sizes its preview.
    pub text: String,
    /// A line for the status area, such as **Remembered: name**. The model
    /// does not see it.
    pub notice: Option<String>,
    pub kind: OutputKind,
    /// Images for the model, which the Agent Loop prepares and stores.
    /// See `docs/Media.md#images-in-tool-results`.
    pub images: Vec<crate::media::RawImage>,
}

/// What the Agent Loop needs to record an output within its limits.
#[derive(Debug, Default)]
pub enum OutputKind {
    /// Shortened text is simply cut.
    #[default]
    Plain,
    /// A chunk of a file read from `offset`. The loop computes `next_offset`
    /// from the bytes it records.
    FileChunk { offset: u64, more: bool },
    /// Command or MCP output. When the preview omits text, the capture saves
    /// the whole text as a session log.
    Logged {
        status: Option<String>,
        capture: Option<crate::environment::LogCapture>,
    },
}

impl PartialEq for ToolOutput {
    fn eq(&self, other: &Self) -> bool {
        self.text == other.text
            && self.notice == other.notice
            && self.kind == other.kind
            && self.images == other.images
    }
}

impl PartialEq for OutputKind {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Plain, Self::Plain) => true,
            (
                Self::FileChunk { offset, more },
                Self::FileChunk {
                    offset: other_offset,
                    more: other_more,
                },
            ) => offset == other_offset && more == other_more,
            (
                Self::Logged { status, capture },
                Self::Logged {
                    status: other_status,
                    capture: other_capture,
                },
            ) => status == other_status && capture.is_some() == other_capture.is_some(),
            _ => false,
        }
    }
}

impl ToolOutput {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            ..Self::default()
        }
    }

    pub fn with_notice(mut self, notice: impl Into<String>) -> Self {
        self.notice = Some(notice.into());
        self
    }
}

/// Why a call produced no output.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ToolError {
    /// The call failed or was rejected. The model sees the message.
    #[error("{0}")]
    Failed(String),
    /// Stop cancelled the call.
    #[error("cancelled")]
    Cancelled,
}

impl ToolError {
    pub fn failed(message: impl Into<String>) -> Self {
        Self::Failed(message.into())
    }
}

/// Runs one validated call.
pub type ToolHandler = Arc<
    dyn Fn(Value, ToolContext) -> BoxFuture<'static, Result<ToolOutput, ToolError>> + Send + Sync,
>;

/// Makes a [`ToolHandler`] from an async function or closure.
pub fn handler<F, Fut>(f: F) -> ToolHandler
where
    F: Fn(Value, ToolContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<ToolOutput, ToolError>> + Send + 'static,
{
    Arc::new(move |arguments, context| Box::pin(f(arguments, context)))
}
