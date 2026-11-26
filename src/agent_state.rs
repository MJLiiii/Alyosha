//! Agent State: the in-memory record of one session.
//!
//! The session driver owns the queue, the active work, and the turn outcomes.
//! `run_turn` alone appends conversation history. Each history entry and
//! checkpoint is saved to the session store first, so a restarted process
//! can resume the session (`docs/SessionStore.md`).
//!
//! Design: `docs/AgentState.md`. Fields and transitions:
//! `docs/AgentState.md#implementation`.

use std::collections::{HashMap, VecDeque};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

/// A submitted message's session-unique ID. It becomes the turn ID when the
/// message's turn starts, even if two submissions have identical text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MessageId(pub u64);

/// A tool call's ID as the model gave it. Its result is recorded under the same
/// ID.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CallId(pub String);

impl From<&str> for CallId {
    fn from(id: &str) -> Self {
        Self(id.to_owned())
    }
}

/// Who submitted a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageOrigin {
    /// The owner, through a front end. A finished job's text also arrives
    /// this way, sent back by the bridge as a prompt. The field stays so
    /// that saved history keeps its shape.
    Owner,
}

/// Who wrote a message in a group chat: a platform user ID, such as
/// `telegram-123`, and their display name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sender {
    pub id: String,
    pub name: String,
}

/// A message waiting in the queue for its turn.
#[derive(Debug, Clone, PartialEq)]
pub struct UserMessage {
    pub id: MessageId,
    pub origin: MessageOrigin,
    /// In a guest session, the person who wrote it.
    pub sender: Option<Sender>,
    /// The text as submitted, without the clock line.
    pub text: String,
    /// When it was submitted. Its clock line shows this time, not the time
    /// its turn starts.
    pub submitted_at: DateTime<Utc>,
}

/// One entry of conversation history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HistoryEntry {
    User(UserEntry),
    Assistant(AssistantEntry),
    ToolResult(ToolResultEntry),
}

/// The message that opened a turn, as the model sees it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserEntry {
    pub message_id: MessageId,
    pub origin: MessageOrigin,
    /// The clock line, a line break, and the submitted text.
    pub text: String,
}

/// One complete assistant response.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct AssistantEntry {
    /// The response's text and tool calls in their original order.
    pub parts: Vec<AssistantPart>,
}

/// One part of an assistant response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum AssistantPart {
    Text(String),
    ToolCall(ToolCall),
    /// Provider data the model needs back in place on later requests, such as
    /// an OpenAI reasoning item or a Gemini thought signature. Only the route
    /// that produced it sends it again.
    Replay(Replay),
}

/// Native data kept for replay on its originating route.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Replay {
    /// The route that produced it, such as `responses` or `gemini`.
    pub route: String,
    pub data: Value,
}

/// A tool call requested by the model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: CallId,
    pub name: String,
    pub arguments: Value,
}

impl AssistantEntry {
    /// The tool calls, in the order the model gave them.
    pub fn tool_calls(&self) -> impl Iterator<Item = &ToolCall> {
        self.parts.iter().filter_map(|part| match part {
            AssistantPart::ToolCall(call) => Some(call),
            _ => None,
        })
    }

    /// The text parts, concatenated in order.
    pub fn text(&self) -> String {
        self.parts
            .iter()
            .filter_map(|part| match part {
                AssistantPart::Text(text) => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }
}

/// The recorded result of one tool call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResultEntry {
    /// The model's original call ID.
    pub call_id: CallId,
    pub outcome: ToolOutcome,
}

/// How a tool call ended, as recorded in history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum ToolOutcome {
    /// The tool's output.
    Success(String),
    /// A tool error. The model sees it and can act on it.
    Error(String),
    /// Stop ended the turn before the call ran, or cancelled it while it ran.
    Cancelled,
}

/// The latest summary and where verbatim history resumes.
#[derive(Debug, Clone, PartialEq)]
pub struct ContextCheckpoint {
    pub summary: String,
    /// The index of the first retained entry in the append-only history.
    pub first_kept_index: usize,
}

/// The work occupying the session's one slot.
#[derive(Debug, Clone)]
pub enum ActiveWork {
    Turn(ActiveTurn),
    ManualCompaction {
        model_id: String,
        cancel: CancellationToken,
    },
}

/// The turn that holds the slot.
#[derive(Debug, Clone)]
pub struct ActiveTurn {
    /// The opening message's ID. It also keys the eventual outcome.
    pub turn_id: MessageId,
    /// The model selected when the turn started.
    pub model_id: String,
    pub phase: TurnPhase,
    /// This turn's Stop signal.
    pub cancel: CancellationToken,
}

impl ActiveTurn {
    /// Whether Stop has been requested for this turn.
    pub fn stop_requested(&self) -> bool {
        self.cancel.is_cancelled()
    }
}

/// What the active turn is doing.
#[derive(Debug, Clone, PartialEq)]
pub enum TurnPhase {
    /// Waiting for a model reply.
    Generating,
    /// Processing a tool batch. `active_call_id` names the call being run.
    RunningTools { active_call_id: Option<CallId> },
    /// An approval or `ask_owner` question for this call is open.
    WaitingForOwner { call_id: CallId },
}

/// How a finished turn ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnOutcome {
    /// The assistant returned a final reply or a clarifying question.
    Completed,
    /// A Stop request ended the turn.
    Stopped,
    /// An unrecoverable error ended the turn. The reason names the error kind.
    Failed(String),
}

/// Everything one session remembers.
#[derive(Debug, Clone)]
pub struct AgentState {
    /// The provider account, fixed when the session opens.
    pub provider: String,
    pub history: Vec<HistoryEntry>,
    pub context_checkpoint: Option<ContextCheckpoint>,
    pub pending_messages: VecDeque<UserMessage>,
    pub active_work: Option<ActiveWork>,
    /// One outcome per finished turn, keyed by its opening message ID.
    pub outcomes: HashMap<MessageId, TurnOutcome>,
}

impl AgentState {
    /// A fresh session's state: empty collections and an empty slot.
    pub fn new(provider: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            history: Vec::new(),
            context_checkpoint: None,
            pending_messages: VecDeque::new(),
            active_work: None,
            outcomes: HashMap::new(),
        }
    }

    /// The active turn, if a turn holds the slot.
    pub fn active_turn(&self) -> Option<&ActiveTurn> {
        match &self.active_work {
            Some(ActiveWork::Turn(turn)) => Some(turn),
            _ => None,
        }
    }

    pub(crate) fn active_turn_mut(&mut self) -> Option<&mut ActiveTurn> {
        match &mut self.active_work {
            Some(ActiveWork::Turn(turn)) => Some(turn),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn assistant_entry_keeps_parts_in_order() {
        let entry = AssistantEntry {
            parts: vec![
                AssistantPart::Text("I'll read ".into()),
                AssistantPart::ToolCall(ToolCall {
                    id: "a".into(),
                    name: "read_file".into(),
                    arguments: json!({"path": "a.txt"}),
                }),
                AssistantPart::Text("both.".into()),
                AssistantPart::ToolCall(ToolCall {
                    id: "b".into(),
                    name: "read_file".into(),
                    arguments: json!({"path": "b.txt"}),
                }),
            ],
        };
        assert_eq!(entry.text(), "I'll read both.");
        let ids: Vec<_> = entry.tool_calls().map(|call| call.id.0.as_str()).collect();
        assert_eq!(ids, ["a", "b"]);
    }
}
