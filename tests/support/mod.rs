#![allow(dead_code)] // Each test binary uses a different part.

//! Test doubles for driving a session without a provider account: a scripted
//! model, a fake `read_file`, a fixed clock, and helpers to build history and
//! wait for events.

use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use alyosha::{
    BoxFuture,
    agent_loop::{SessionDriver, SessionEvent, SessionSetup},
    agent_state::{
        AssistantEntry, AssistantPart, HistoryEntry, MessageId, MessageOrigin, ToolCall,
        ToolOutcome, ToolResultEntry, UserEntry,
    },
    clock::FixedClock,
    llm_client::{ModelClient, ModelError, ModelReply, ModelRequest},
    tools::{
        RegisteredTool, ToolContext, ToolDefinition, ToolError, ToolOutput, ToolRegistry, handler,
    },
};
use chrono::{TimeZone, Utc};
use serde_json::{Value, json};
use tokio::sync::{Notify, mpsc::UnboundedReceiver};
use tokio_util::sync::CancellationToken;

pub const PROVIDER: &str = "test-provider";
pub const MODEL: &str = "test-model";

/// The clock line for the fixed clock: Wednesday 2026-11-25 10:02 in
/// Europe/Amsterdam.
pub const CLOCK_LINE: &str = "[Wed 2026-11-25 10:02, Europe/Amsterdam]";

/// Starts a session at the fixed time with the scripted model and the given
/// tools.
pub fn start(
    model: Arc<ScriptedModel>,
    registry: ToolRegistry,
) -> (SessionDriver, UnboundedReceiver<SessionEvent>) {
    SessionDriver::start(SessionSetup::minimal(
        PROVIDER,
        MODEL,
        chrono_tz::Europe::Amsterdam,
        model,
        registry,
        Arc::new(FixedClock(
            Utc.with_ymd_and_hms(2026, 11, 25, 9, 2, 0).unwrap(),
        )),
    ))
}

/// The events a front end shows for a turn, without status-line updates.
pub fn shown(events: &[SessionEvent]) -> Vec<SessionEvent> {
    events
        .iter()
        .filter(|event| !matches!(event, SessionEvent::Status(_)))
        .cloned()
        .collect()
}

/// A model whose replies are written in advance. It records every request it
/// receives.
#[derive(Default)]
pub struct ScriptedModel {
    steps: Mutex<VecDeque<Step>>,
    requests: Mutex<Vec<ModelRequest>>,
}

struct Step {
    reply: Result<ModelReply, ModelError>,
    gate: Option<Gate>,
}

impl ScriptedModel {
    pub fn new() -> Arc<Self> {
        Arc::default()
    }

    /// Adds a complete reply with these parts.
    pub fn reply(&self, parts: Vec<AssistantPart>) {
        self.push(Ok(complete(parts)), None);
    }

    /// Adds a reply that waits until the test releases it. The wait ignores
    /// Stop, like a client that never returns, so only the Agent Loop's own
    /// Stop handling can end it.
    pub fn reply_gated(&self, parts: Vec<AssistantPart>) -> Gate {
        let gate = Gate::default();
        self.push(Ok(complete(parts)), Some(gate.clone()));
        gate
    }

    /// Adds a failed request.
    pub fn fail(&self, error: ModelError) {
        self.push(Err(error), None);
    }

    /// Every request received so far, in order.
    pub fn requests(&self) -> Vec<ModelRequest> {
        self.requests.lock().unwrap().clone()
    }

    fn push(&self, reply: Result<ModelReply, ModelError>, gate: Option<Gate>) {
        self.steps.lock().unwrap().push_back(Step { reply, gate });
    }
}

impl ModelClient for ScriptedModel {
    fn generate(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
    ) -> BoxFuture<'_, Result<ModelReply, ModelError>> {
        // Recorded when the request is handed over, even if the reply is
        // never awaited.
        self.requests.lock().unwrap().push(request);
        Box::pin(async move {
            let Some(step) = self.steps.lock().unwrap().pop_front() else {
                return Err(ModelError::Unsupported(
                    "the script has no reply for this request".to_owned(),
                ));
            };
            if let Some(gate) = step.gate {
                gate.started.notify_one();
                gate.release.notified().await;
            }
            if cancel.is_cancelled() {
                return Err(ModelError::Cancelled);
            }
            step.reply
        })
    }
}

fn complete(parts: Vec<AssistantPart>) -> ModelReply {
    ModelReply {
        entry: AssistantEntry { parts },
        usage: None,
        request_id: None,
    }
}

/// Holds a scripted step until the test releases it.
#[derive(Clone, Default)]
pub struct Gate {
    started: Arc<Notify>,
    release: Arc<Notify>,
}

impl Gate {
    /// Waits until the step has started and is holding.
    pub async fn started(&self) {
        within_5s(self.started.notified()).await;
    }

    pub fn release(&self) {
        self.release.notify_one();
    }
}

/// The `read_file` definition from the built-in tool contracts.
pub fn read_file_definition() -> ToolDefinition {
    ToolDefinition {
        name: "read_file".to_owned(),
        description: "Read a bounded chunk of UTF-8 text from a file.".to_owned(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "offset": {"type": "integer", "minimum": 0},
                "max_bytes": {"type": "integer", "minimum": 1}
            },
            "required": ["path"]
        }),
        requires_approval: false,
    }
}

/// A fake `read_file`: `a.txt` holds `alpha` and every other file is missing.
/// It counts its runs in `runs`. A read of the gated path waits for the test,
/// like a real file operation it then finishes even after Stop.
pub fn fake_read_file(
    runs: Arc<AtomicUsize>,
    gated: Option<(&'static str, Gate)>,
) -> RegisteredTool {
    RegisteredTool {
        definition: read_file_definition(),
        handler: handler(move |arguments: Value, _context: ToolContext| {
            let runs = runs.clone();
            let gated = gated.clone();
            async move {
                runs.fetch_add(1, Ordering::SeqCst);
                let path = arguments["path"].as_str().unwrap_or_default();
                if let Some((gated_path, gate)) = gated
                    && path == gated_path
                {
                    gate.started.notify_one();
                    gate.release.notified().await;
                }
                match path {
                    "a.txt" => Ok(ToolOutput::text("alpha")),
                    _ => Err(ToolError::failed("file not found")),
                }
            }
        }),
    }
}

/// A registry holding only the fake `read_file`.
pub fn files_registry(runs: Arc<AtomicUsize>) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(fake_read_file(runs, None)).unwrap();
    registry
}

pub fn text(text: &str) -> AssistantPart {
    AssistantPart::Text(text.to_owned())
}

pub fn call(id: &str, name: &str, arguments: Value) -> AssistantPart {
    AssistantPart::ToolCall(ToolCall {
        id: id.into(),
        name: name.to_owned(),
        arguments,
    })
}

/// The history entry for an owner's message submitted at the fixed time.
pub fn user(id: MessageId, text: &str) -> HistoryEntry {
    HistoryEntry::User(UserEntry::text_only(
        id,
        MessageOrigin::Owner,
        format!("{CLOCK_LINE}\n{text}"),
    ))
}

pub fn assistant(parts: Vec<AssistantPart>) -> HistoryEntry {
    HistoryEntry::Assistant(AssistantEntry { parts })
}

pub fn tool_result(call_id: &str, outcome: ToolOutcome) -> HistoryEntry {
    HistoryEntry::ToolResult(ToolResultEntry {
        call_id: call_id.into(),
        outcome,
        images: Vec::new(),
    })
}

/// The events up to and including the end of the given turn.
pub async fn events_until_turn_ends(
    events: &mut UnboundedReceiver<SessionEvent>,
    turn_id: MessageId,
) -> Vec<SessionEvent> {
    let mut seen = Vec::new();
    loop {
        let event = within_5s(events.recv())
            .await
            .expect("the session closed its events");
        let ended = matches!(&event, SessionEvent::TurnEnded { turn_id: id, .. } if *id == turn_id);
        seen.push(event);
        if ended {
            return seen;
        }
    }
}

/// The IDs of the turns that started, in order.
pub fn started(events: &[SessionEvent]) -> Vec<MessageId> {
    events
        .iter()
        .filter_map(|event| match event {
            SessionEvent::TurnStarted(id) => Some(*id),
            _ => None,
        })
        .collect()
}

async fn within_5s<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(5), future)
        .await
        .expect("nothing happened within 5 seconds")
}
