//! Agent Loop: the session driver and its turn driver, `run_turn`.
//!
//! The session driver serializes work. Messages wait in arrival order and a
//! single consumer runs one turn or manual compaction at a time. `run_turn`
//! takes one message through as many model requests and tool batches as it
//! needs and is the sole writer of conversation history.
//!
//! Design: `docs/design/AgentLoop.md`. Implementation guide:
//! `docs/develop/AgentLoop.md`.

use std::sync::{
    Arc, Mutex, MutexGuard, PoisonError, Weak,
    atomic::{AtomicU64, Ordering},
};

use chrono_tz::Tz;
use serde_json::Value;
use tokio::sync::{Notify, mpsc};
use tokio_util::sync::CancellationToken;

use crate::{
    agent_state::{
        ActiveTurn, ActiveWork, AgentState, CallId, ContextCheckpoint, HistoryEntry, MessageId,
        MessageOrigin, Sender, ToolCall, ToolOutcome, ToolResultEntry, TurnOutcome, TurnPhase,
        UserEntry, UserMessage,
    },
    approvals::{Approver, ObservedApprover, OwnerRequest},
    clock::{Clock, clock_line},
    context_manager::{self, Mode, Preparation, PrepareError, RequestSettings, RouteLimits},
    jobs::{self, JobEnvironment, JobFinished, JobManager, JobOrigin},
    llm_client::{ModelClient, ModelError, ModelRequest},
    session_store::{SessionId, SessionStore, StoredSession},
    tools::{Scope, SessionServices, ToolExecutor, ToolRegistry},
};

/// The normal system instruction before the session adds to it. See
/// `docs/design/LLMClientGateway.md#default-instruction-for-normal-requests`.
pub const DEFAULT_INSTRUCTION: &str = "You are Alyosha, a personal assistant.";

/// The status a busy session gives `/compact`.
pub const COMPACT_BUSY: &str = "Busy: try /compact again when the session is idle.";
/// The usage a bare `/job` shows.
pub const JOB_USAGE: &str = "/job <command> starts a background job.";

/// What a session needs at startup.
pub struct SessionSetup {
    /// The GPROXY provider name, fixed for the session.
    pub provider: String,
    /// The model ID. Each turn pins the model it starts with.
    pub model: String,
    /// The owner's timezone, for the clock line on each message.
    pub timezone: Tz,
    /// The client every model request goes through.
    pub client: Arc<dyn ModelClient>,
    /// The catalog built at startup. It stays fixed while turns run.
    pub registry: ToolRegistry,
    /// Where submission times come from.
    pub clock: Arc<dyn Clock>,
    /// The normal system instruction, built once at startup.
    pub system_instruction: String,
    /// The route's token limits.
    pub limits: RouteLimits,
    /// The environment and stores. The driver adds the notice channel and
    /// watches the approver.
    pub services: SessionServices,
    /// Background jobs' sandbox, when the session can run commands.
    pub jobs: Option<JobEnvironment>,
    /// Where history is saved, with the session it was created or loaded
    /// as. Without it, history lives only in memory.
    pub persistence: Option<Persistence>,
    /// Builds the normal system instruction again, with fresh memory and
    /// skills indexes, after each compaction.
    pub rebuild_instruction: Option<Arc<dyn Fn() -> String + Send + Sync>>,
}

/// A session's store and its saved state.
pub struct Persistence {
    pub store: Arc<dyn SessionStore>,
    pub stored: StoredSession,
    /// Whether the session was loaded rather than just created. Startup then
    /// closes a turn a restart cut off and says the session resumed.
    pub resumed: bool,
}

impl SessionSetup {
    /// A session with only the given tools: no workspace, memory, skills, or
    /// jobs. For tests and tool-less use.
    pub fn minimal(
        provider: &str,
        model: &str,
        timezone: Tz,
        client: Arc<dyn ModelClient>,
        registry: ToolRegistry,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            provider: provider.to_owned(),
            model: model.to_owned(),
            timezone,
            client,
            registry,
            clock,
            system_instruction: DEFAULT_INSTRUCTION.to_owned(),
            limits: RouteLimits::for_model(model),
            services: SessionServices::default(),
            jobs: None,
            persistence: None,
            rebuild_instruction: None,
        }
    }
}

/// What the status line shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Idle,
    Thinking,
    Running(String),
    WaitingForOwner,
    Compacting,
}

/// What the session driver tells its front end.
#[derive(Debug, Clone, PartialEq)]
pub enum SessionEvent {
    /// A submitted message joined the queue.
    Queued(MessageId),
    /// A message's turn started.
    TurnStarted(MessageId),
    Status(Status),
    /// A tool call started; the title reads like the status line.
    ToolStarted {
        call_id: CallId,
        tool: String,
        title: String,
    },
    /// A tool call finished; `ok` is false for a tool error or cancellation.
    ToolFinished {
        call_id: CallId,
        ok: bool,
    },
    /// Assistant text that came with tool calls, in response order.
    AssistantText(String),
    /// A line for the status area, such as a memory write.
    Notice(String),
    /// A turn's final reply, to display.
    Reply {
        turn_id: MessageId,
        text: String,
    },
    /// A turn closed. Its outcome is already recorded in the state.
    TurnEnded {
        turn_id: MessageId,
        outcome: TurnOutcome,
    },
    /// A queued message that will never run, because the session ended.
    Discarded(MessageId),
    /// The result of `/compact`, `/job`, or the busy message.
    CommandStatus(String),
    /// The number of running background jobs changed.
    Jobs(usize),
    /// In ACP mode, a background job ended on its own.
    JobFinished {
        job_id: String,
        text: String,
    },
}

/// A running session. Clones drive the same session.
#[derive(Clone)]
pub struct SessionDriver {
    session: Arc<Session>,
}

struct Session {
    state: Mutex<AgentState>,
    /// The last message ID issued. It changes only under the state lock, so
    /// IDs follow queue order.
    last_message_id: AtomicU64,
    /// Wakes the consumer when work may be ready.
    wake: Notify,
    events: mpsc::UnboundedSender<SessionEvent>,
    model: String,
    timezone: Tz,
    client: Arc<dyn ModelClient>,
    /// The request settings; the system instruction changes at compaction.
    settings: Mutex<RequestSettings>,
    rebuild_instruction: Option<Arc<dyn Fn() -> String + Send + Sync>>,
    /// Where each entry and checkpoint is saved before it takes effect.
    store: Option<(Arc<dyn SessionStore>, SessionId)>,
    /// The sequence number of `history[0]`.
    base_seq: u64,
    executor: ToolExecutor,
    services: Arc<SessionServices>,
    clock: Arc<dyn Clock>,
    /// Ends the consumer when the session ends.
    ended: CancellationToken,
}

impl SessionDriver {
    /// Starts a session and its consumer task. Call it within a Tokio
    /// runtime. The receiver carries the session's events for the front end.
    pub fn start(setup: SessionSetup) -> (Self, mpsc::UnboundedReceiver<SessionEvent>) {
        let (events, receiver) = mpsc::unbounded_channel();
        let (job_messages, mut job_inbox) = mpsc::unbounded_channel::<JobFinished>();
        let registry = Arc::new(setup.registry);
        let session = Arc::new_cyclic(|weak: &Weak<Session>| {
            let mut services = setup.services;
            let notices = events.clone();
            services.notify = Some(Arc::new(move |text| {
                let _ = notices.send(SessionEvent::Notice(text));
            }));
            if let Some(approver) = services.approver.take() {
                services.approver = Some(watch_approver(approver, weak.clone()));
            }
            if let Some(environment) = setup.jobs {
                let counts = events.clone();
                services.jobs = Some(
                    JobManager::new(
                        environment,
                        Arc::new(move |finished| {
                            let _ = job_messages.send(finished);
                        }),
                    )
                    .on_change(Arc::new(move |count| {
                        let _ = counts.send(SessionEvent::Jobs(count));
                    })),
                );
            }
            let services = Arc::new(services);
            let mut state = AgentState::new(setup.provider.clone());
            let mut last_message_id = 0;
            let mut base_seq = 0;
            let store = setup.persistence.map(|persistence| {
                let stored = persistence.stored;
                base_seq = stored.base_seq();
                last_message_id = stored.next_message_id.saturating_sub(1);
                state.history = stored.entries;
                state.context_checkpoint =
                    stored.checkpoint.map(|(_, summary)| ContextCheckpoint {
                        summary,
                        first_kept_index: 0,
                    });
                (persistence.store, stored.id)
            });
            Session {
                state: Mutex::new(state),
                last_message_id: AtomicU64::new(last_message_id),
                wake: Notify::new(),
                events,
                model: setup.model.clone(),
                timezone: setup.timezone,
                client: setup.client,
                settings: Mutex::new(RequestSettings {
                    provider: setup.provider,
                    model: setup.model,
                    system_instruction: setup.system_instruction,
                    tools: registry.definitions().cloned().collect(),
                    limits: setup.limits,
                }),
                rebuild_instruction: setup.rebuild_instruction,
                store,
                base_seq,
                executor: ToolExecutor::new(registry, services.clone()),
                services,
                clock: setup.clock,
                ended: CancellationToken::new(),
            }
        });
        tokio::spawn(consume(session.clone()));
        let weak = Arc::downgrade(&session);
        // A finished job is reported, not queued: the bridge sends its text
        // back as a prompt (docs/develop/Jobs.md#when-a-job-finishes).
        tokio::spawn(async move {
            while let Some(finished) = job_inbox.recv().await {
                let Some(session) = weak.upgrade() else {
                    return;
                };
                session.emit(SessionEvent::JobFinished {
                    job_id: finished.id,
                    text: finished.text,
                });
            }
        });
        (Self { session }, receiver)
    }

    /// Queues one of the owner's messages behind any earlier ones and returns
    /// its ID.
    pub fn submit(&self, text: impl Into<String>) -> MessageId {
        self.session
            .enqueue(text.into(), None)
    }

    /// Queues a group member's message in a guest session. When its turn
    /// starts, the sender becomes the speaker whose notes `user_memory`
    /// writes, and the opening entry carries their notes.
    pub fn submit_from(&self, text: impl Into<String>, sender: Sender) -> MessageId {
        self.session
            .enqueue(text.into(), Some(sender))
    }

    /// Signals Stop to the active turn or command. Queued messages stay
    /// queued and start in order afterwards.
    pub fn stop(&self) {
        let state = self.session.lock_state();
        match &state.active_work {
            Some(ActiveWork::Turn(turn)) => turn.cancel.cancel(),
            Some(ActiveWork::ManualCompaction { cancel, .. }) => cancel.cancel(),
            None => {}
        }
    }

    /// Runs `/compact` if the session is idle with an empty queue, and
    /// reports its status through `CommandStatus`.
    pub fn compact(&self) {
        let session = self.session.clone();
        let cancel = CancellationToken::new();
        {
            let mut state = session.lock_state();
            if state.active_work.is_some() || !state.pending_messages.is_empty() {
                drop(state);
                session.emit(SessionEvent::CommandStatus(COMPACT_BUSY.into()));
                return;
            }
            state.active_work = Some(ActiveWork::ManualCompaction {
                model_id: session.model.clone(),
                cancel: cancel.clone(),
            });
        }
        tokio::spawn(async move {
            let status = session.manual_compaction(&cancel).await;
            session.emit(SessionEvent::Status(Status::Idle));
            session.emit(SessionEvent::CommandStatus(status));
            session.wake.notify_one();
        });
    }

    /// Runs `/job <command>` at once, without the work slot or the queue, and
    /// returns its status, which is also sent as `CommandStatus`.
    pub fn start_job(&self, command: &str) -> String {
        let command = command.trim();
        let status = if command.is_empty() {
            JOB_USAGE.to_owned()
        } else {
            match &self.session.services.jobs {
                None => "This session cannot run background jobs.".to_owned(),
                Some(jobs) => match jobs.start(command, jobs::DEFAULT_TIMEOUT, JobOrigin::Owner) {
                    Ok(info) => {
                        format!("Job {} started: {}", info.id, jobs::short_command(command))
                    }
                    Err(error) => error.to_string(),
                },
            }
        };
        self.session
            .emit(SessionEvent::CommandStatus(status.clone()));
        status
    }

    /// A copy of the session's state.
    pub fn snapshot(&self) -> AgentState {
        self.session.lock_state().clone()
    }

    /// The session's services, such as its job manager.
    pub fn services(&self) -> &Arc<SessionServices> {
        &self.session.services
    }

    /// Whether nothing is running, queued, or waiting, and no job runs.
    pub fn is_idle(&self) -> bool {
        let state = self.session.lock_state();
        state.active_work.is_none()
            && state.pending_messages.is_empty()
            && self
                .session
                .services
                .jobs
                .as_ref()
                .is_none_or(|jobs| jobs.running_count() == 0)
    }

    /// Ends the session: stops active work, discards queued messages, kills
    /// background jobs, and removes the session's logs.
    pub async fn end(&self) {
        let session = &self.session;
        session.ended.cancel();
        let discarded: Vec<MessageId> = {
            let mut state = session.lock_state();
            match &state.active_work {
                Some(ActiveWork::Turn(turn)) => turn.cancel.cancel(),
                Some(ActiveWork::ManualCompaction { cancel, .. }) => cancel.cancel(),
                None => {}
            }
            state
                .pending_messages
                .drain(..)
                .map(|message| message.id)
                .collect()
        };
        for id in discarded {
            session.emit(SessionEvent::Discarded(id));
        }
        session.wake.notify_one();
        // Wait for the active turn to close.
        loop {
            if session.lock_state().active_work.is_none() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        if let Some(jobs) = &session.services.jobs {
            jobs.shutdown().await;
        }
        if let Some(mcp) = &session.services.mcp {
            mcp.shutdown().await;
        }
        if let Some(environment) = &session.services.environment {
            environment.logs.remove_all();
        }
        if let Some((store, id)) = &session.store
            && let Err(error) = store.close(*id).await
        {
            tracing::warn!(error = %error, "could not release the session's lock");
        }
    }

    /// The system instruction requests use now.
    pub fn system_instruction(&self) -> String {
        self.session.settings().system_instruction
    }
}

/// Tracks the approval phase around the front end's approver.
fn watch_approver(inner: Arc<dyn Approver>, session: Weak<Session>) -> Arc<dyn Approver> {
    Arc::new(ObservedApprover {
        inner,
        on_ask: Arc::new(move |request: &OwnerRequest, asking: bool| {
            let Some(session) = session.upgrade() else {
                return;
            };
            let call_id = CallId(request.call_id.clone());
            {
                let mut state = session.lock_state();
                let phase = if asking {
                    TurnPhase::WaitingForOwner {
                        call_id: call_id.clone(),
                    }
                } else {
                    TurnPhase::RunningTools {
                        active_call_id: Some(call_id),
                    }
                };
                set_phase(&mut state, phase);
            }
            if asking {
                session.emit(SessionEvent::Status(Status::WaitingForOwner));
            }
        }),
    })
}

/// How `run_turn` ended.
enum TurnEnd {
    Completed(String),
    Stopped,
    Failed(String),
}

/// The single consumer. It reserves the free slot and takes the oldest message
/// in one state update, runs the turn, then records the outcome and releases
/// the slot in one update.
async fn consume(session: Arc<Session>) {
    loop {
        if session.ended.is_cancelled() {
            return;
        }
        let Some((message, model, cancel)) = session.start_next_turn() else {
            tokio::select! {
                () = session.wake.notified() => {}
                () = session.ended.cancelled() => return,
            }
            continue;
        };
        let turn_id = message.id;
        session.emit(SessionEvent::TurnStarted(turn_id));
        let (outcome, reply) = match session.run_turn(message, &model, &cancel).await {
            TurnEnd::Completed(reply) => (TurnOutcome::Completed, Some(reply)),
            TurnEnd::Stopped => (TurnOutcome::Stopped, None),
            TurnEnd::Failed(reason) => (TurnOutcome::Failed(reason), None),
        };
        let idle = {
            let mut state = session.lock_state();
            state.outcomes.insert(turn_id, outcome.clone());
            state.active_work = None;
            state.pending_messages.is_empty()
        };
        if let Some(text) = reply {
            session.emit(SessionEvent::Reply { turn_id, text });
        }
        session.emit(SessionEvent::TurnEnded { turn_id, outcome });
        if idle {
            session.emit(SessionEvent::Status(Status::Idle));
        }
    }
}

impl Session {
    fn enqueue(&self, text: String, sender: Option<Sender>) -> MessageId {
        let id = {
            let mut state = self.lock_state();
            let id = MessageId(self.last_message_id.fetch_add(1, Ordering::Relaxed) + 1);
            state.pending_messages.push_back(UserMessage {
                id,
                origin: MessageOrigin::Owner,
                sender,
                text,
                submitted_at: self.clock.now(),
            });
            // Sent under the lock, so the front end hears of the message
            // before its turn can start.
            self.emit(SessionEvent::Queued(id));
            id
        };
        self.wake.notify_one();
        id
    }

    /// Reserves the free slot for the oldest message, with the session's
    /// model and a fresh Stop signal.
    fn start_next_turn(&self) -> Option<(UserMessage, String, CancellationToken)> {
        let mut state = self.lock_state();
        if state.active_work.is_some() {
            return None;
        }
        let message = state.pending_messages.pop_front()?;
        let cancel = CancellationToken::new();
        state.active_work = Some(ActiveWork::Turn(ActiveTurn {
            turn_id: message.id,
            model_id: self.model.clone(),
            phase: TurnPhase::Generating,
            cancel: cancel.clone(),
        }));
        Some((message, self.model.clone(), cancel))
    }

    fn settings(&self) -> RequestSettings {
        self.settings
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn settings_for(&self, model: &str) -> RequestSettings {
        RequestSettings {
            model: model.to_owned(),
            ..self.settings()
        }
    }

    /// Saves an entry as the next in history, then appends it. `run_turn`
    /// is the only writer, so the position cannot change in between.
    async fn append(&self, entry: HistoryEntry) -> Result<usize, String> {
        let index = self.lock_state().history.len();
        if let Some((store, id)) = &self.store {
            store
                .append(*id, self.base_seq + index as u64, &entry)
                .await
                .map_err(|error| {
                    tracing::error!(error = %error, "could not save a history entry");
                    format!("Could not save history: {error}")
                })?;
        }
        self.lock_state().history.push(entry);
        Ok(index)
    }

    /// Prepares the next request through the Context Manager and commits any
    /// new checkpoint while the work is still active.
    async fn prepare(
        &self,
        settings: &RequestSettings,
        opening: Option<usize>,
        cancel: &CancellationToken,
        mode: Mode,
    ) -> Result<ModelRequest, PrepareError> {
        let (history, checkpoint) = {
            let state = self.lock_state();
            (state.history.clone(), state.context_checkpoint.clone())
        };
        let compacting = |active: bool| {
            if active {
                self.emit(SessionEvent::Status(Status::Compacting));
                self.emit(SessionEvent::Notice("Compacting context…".into()));
            } else {
                self.emit(SessionEvent::Status(Status::Thinking));
            }
        };
        let prepared = Preparation {
            settings,
            history: &history,
            checkpoint: checkpoint.as_ref(),
            opening,
            client: self.client.as_ref(),
            cancel,
            compacting: &compacting,
        }
        .run(mode)
        .await?;
        if let Some(candidate) = prepared.candidate {
            self.commit(candidate, cancel).await?;
        }
        Ok(prepared.request)
    }

    /// Saves and commits a checkpoint, unless Stop came first. Then the
    /// system instruction is rebuilt and the logs of replaced results go.
    async fn commit(
        &self,
        checkpoint: ContextCheckpoint,
        cancel: &CancellationToken,
    ) -> Result<(), PrepareError> {
        if cancel.is_cancelled() {
            return Err(PrepareError::Cancelled);
        }
        if let Some((store, id)) = &self.store {
            let first_kept_seq = self.base_seq + checkpoint.first_kept_index as u64;
            store
                .save_checkpoint(*id, first_kept_seq, &checkpoint.summary)
                .await
                .map_err(|error| PrepareError::Store(error.to_string()))?;
        }
        let replaced: Vec<String> = {
            let mut state = self.lock_state();
            let from = state
                .context_checkpoint
                .as_ref()
                .map_or(0, |old| old.first_kept_index);
            let to = checkpoint.first_kept_index.min(state.history.len());
            let replaced = state.history[from.min(to)..to]
                .iter()
                .filter_map(|entry| match entry {
                    HistoryEntry::ToolResult(result) => match &result.outcome {
                        ToolOutcome::Success(text) | ToolOutcome::Error(text) => {
                            context_manager::log_path(text).map(str::to_owned)
                        }
                        ToolOutcome::Cancelled => None,
                    },
                    _ => None,
                })
                .collect();
            state.context_checkpoint = Some(checkpoint);
            replaced
        };
        if let Some(environment) = &self.services.environment {
            for path in replaced {
                environment.logs.remove(&path);
            }
        }
        if let Some(rebuild) = &self.rebuild_instruction {
            let instruction = rebuild();
            self.settings
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .system_instruction = instruction;
        }
        Ok(())
    }

    /// Takes one message through model requests and tool batches until the
    /// model replies without calls, Stop ends the turn, or a request fails.
    async fn run_turn(
        &self,
        message: UserMessage,
        model: &str,
        cancel: &CancellationToken,
    ) -> TurnEnd {
        // In a guest session the sender becomes the speaker for this turn,
        // and their notes go in front of their message.
        *self
            .services
            .speaker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = message.sender.clone();
        let body = match (&message.sender, &self.services.users) {
            (Some(sender), Some(users)) => users.opening(sender, &message.text),
            _ => message.text.clone(),
        };
        let opening = UserEntry {
            message_id: message.id,
            origin: message.origin,
            text: format!(
                "{}\n{}",
                clock_line(message.submitted_at, self.timezone),
                body
            ),
        };
        let opening_index = match self.append(HistoryEntry::User(opening)).await {
            Ok(index) => index,
            Err(reason) => return TurnEnd::Failed(reason),
        };
        let mut recovered = false;

        loop {
            // Read each round, so a compaction's new instruction applies.
            let settings = self.settings_for(model);
            {
                let mut state = self.lock_state();
                if cancel.is_cancelled() {
                    return TurnEnd::Stopped;
                }
                set_phase(&mut state, TurnPhase::Generating);
            }
            self.emit(SessionEvent::Status(Status::Thinking));
            let mut request = match self
                .prepare(&settings, Some(opening_index), cancel, Mode::Normal)
                .await
            {
                Ok(request) => request,
                Err(error) => return prepare_failure(error),
            };
            let reply = loop {
                let size = context_manager::estimate(&request);
                let result = tokio::select! {
                    biased;
                    () = cancel.cancelled() => return TurnEnd::Stopped,
                    result = self.client.generate(request, cancel.clone()) => result,
                };
                match result {
                    Ok(reply) => break reply,
                    Err(ModelError::Cancelled) => return TurnEnd::Stopped,
                    // One recovery attempt per turn for an explicit overflow.
                    Err(ModelError::ContextOverflow(reason)) if !recovered => {
                        recovered = true;
                        match self
                            .prepare(
                                &settings,
                                Some(opening_index),
                                cancel,
                                Mode::Forced { rejected: size },
                            )
                            .await
                        {
                            Ok(smaller) => request = smaller,
                            Err(PrepareError::Cancelled) => return TurnEnd::Stopped,
                            Err(_) => {
                                return TurnEnd::Failed(
                                    ModelError::ContextOverflow(reason).to_string(),
                                );
                            }
                        }
                    }
                    Err(error) => return TurnEnd::Failed(error.to_string()),
                }
            };

            let calls: Vec<ToolCall> = reply.entry.tool_calls().cloned().collect();
            let text = reply.entry.text();
            // Stop during generation discards the reply.
            if cancel.is_cancelled() {
                return TurnEnd::Stopped;
            }
            if let Err(reason) = self.append(HistoryEntry::Assistant(reply.entry)).await {
                return TurnEnd::Failed(reason);
            }
            if !calls.is_empty() {
                set_phase(
                    &mut self.lock_state(),
                    TurnPhase::RunningTools {
                        active_call_id: None,
                    },
                );
            }
            if calls.is_empty() {
                return TurnEnd::Completed(text);
            }
            if !text.trim().is_empty() {
                self.emit(SessionEvent::AssistantText(text));
            }
            if let Err(reason) = self
                .run_batch(&calls, cancel, &settings, opening_index)
                .await
            {
                return TurnEnd::Failed(reason);
            }
        }
    }

    /// Bytes of tool-result text the request still has room for.
    fn result_room(&self, settings: &RequestSettings, opening: usize) -> usize {
        let (history, checkpoint) = {
            let state = self.lock_state();
            (state.history.clone(), state.context_checkpoint.clone())
        };
        let request = settings.build(&history, checkpoint.as_ref(), Some(opening));
        let budget = settings.limits.usable_input(settings.limits.default_output);
        (budget.saturating_sub(context_manager::estimate(&request)) * 3) as usize
    }

    /// Runs the calls in model order and records one result for each under its
    /// original call ID, within the result limits. After Stop, the admitted
    /// call keeps its actual outcome and each unstarted call gets a cancelled
    /// result. A result that cannot be saved fails the turn.
    async fn run_batch(
        &self,
        calls: &[ToolCall],
        cancel: &CancellationToken,
        settings: &RequestSettings,
        opening: usize,
    ) -> Result<(), String> {
        let mut batch_left = context_manager::BATCH_LIMIT;
        let mut room = self.result_room(settings, opening);
        for (index, call) in calls.iter().enumerate() {
            if cancel.is_cancelled() {
                for unstarted in &calls[index..] {
                    self.append(tool_result(&unstarted.id, ToolOutcome::Cancelled))
                        .await?;
                    self.emit(SessionEvent::ToolFinished {
                        call_id: unstarted.id.clone(),
                        ok: false,
                    });
                }
                return Ok(());
            }
            {
                let mut state = self.lock_state();
                set_phase(
                    &mut state,
                    TurnPhase::RunningTools {
                        active_call_id: Some(call.id.clone()),
                    },
                );
            }
            let title = call_title(&call.name, &call.arguments);
            self.emit(SessionEvent::ToolStarted {
                call_id: call.id.clone(),
                tool: call.name.clone(),
                title: title.clone(),
            });
            self.emit(SessionEvent::Status(Status::Running(title)));
            // Name and ID only: arguments and output stay out of the log.
            tracing::info!(tool = %call.name, call_id = %call.id.0, "tool call started");
            let started = std::time::Instant::now();
            let result = self
                .executor
                .dispatch(
                    &call.name,
                    call.arguments.clone(),
                    Scope::Direct,
                    cancel,
                    Some(call.id.clone()),
                )
                .await;
            // Keep a little room for each later result's metadata.
            let reserve = 120 * (calls.len() - index - 1);
            let allowance = context_manager::RESULT_LIMIT
                .min(batch_left)
                .min(room.saturating_sub(reserve));
            let (outcome, used, notice) = context_manager::record(result, allowance);
            tracing::info!(
                tool = %call.name,
                call_id = %call.id.0,
                duration_ms = started.elapsed().as_millis() as u64,
                outcome = match &outcome {
                    ToolOutcome::Success(_) => "success",
                    ToolOutcome::Error(_) => "error",
                    ToolOutcome::Cancelled => "cancelled",
                },
                "tool call finished"
            );
            batch_left = batch_left.saturating_sub(used);
            room = room.saturating_sub(used + 120);
            let ok = matches!(outcome, ToolOutcome::Success(_));
            let saved = self.append(tool_result(&call.id, outcome)).await;
            set_phase(
                &mut self.lock_state(),
                TurnPhase::RunningTools {
                    active_call_id: None,
                },
            );
            saved?;
            self.emit(SessionEvent::ToolFinished {
                call_id: call.id.clone(),
                ok,
            });
            if let Some(notice) = notice {
                self.emit(SessionEvent::Notice(notice));
            }
        }
        Ok(())
    }

    /// `/compact` with the slot already reserved: summarize older history
    /// even below the threshold, and release the slot on every exit.
    async fn manual_compaction(&self, cancel: &CancellationToken) -> String {
        let settings = self.settings();
        let result = self.prepare_manual(&settings, cancel).await;
        let mut state = self.lock_state();
        state.active_work = None;
        drop(state);
        result
    }

    async fn prepare_manual(
        &self,
        settings: &RequestSettings,
        cancel: &CancellationToken,
    ) -> String {
        let (history, checkpoint) = {
            let state = self.lock_state();
            (state.history.clone(), state.context_checkpoint.clone())
        };
        let compacting = |active: bool| {
            if active {
                self.emit(SessionEvent::Status(Status::Compacting));
            }
        };
        let prepared = Preparation {
            settings,
            history: &history,
            checkpoint: checkpoint.as_ref(),
            opening: None,
            client: self.client.as_ref(),
            cancel,
            compacting: &compacting,
        }
        .run(Mode::Manual)
        .await;
        match prepared {
            Ok(prepared) => match prepared.candidate {
                Some(candidate) => match self.commit(candidate, cancel).await {
                    Ok(()) => "Context compacted".into(),
                    Err(PrepareError::Cancelled) => "Compaction stopped".into(),
                    Err(error) => format!("Compaction failed: {error}"),
                },
                None => "Context unchanged".into(),
            },
            Err(PrepareError::Cancelled) => "Compaction stopped".into(),
            Err(PrepareError::ContextWindow(reason))
                if reason == context_manager::NO_OLDER_CONTEXT =>
            {
                reason
            }
            Err(error) => format!("Compaction failed: {error}"),
        }
    }

    fn lock_state(&self) -> MutexGuard<'_, AgentState> {
        // Every update is a short sequence that cannot panic halfway, so the
        // state behind a poisoned lock is still consistent.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn emit(&self, event: SessionEvent) {
        // A front end that has gone away does not stop the session.
        let _ = self.events.send(event);
    }
}

fn prepare_failure(error: PrepareError) -> TurnEnd {
    match error {
        PrepareError::Cancelled => TurnEnd::Stopped,
        other => TurnEnd::Failed(other.to_string()),
    }
}

/// How a call reads in the status line and the scrollback.
pub fn call_title(name: &str, arguments: &Value) -> String {
    let text = |key: &str| arguments.get(key).and_then(Value::as_str);
    match name {
        "read_file" | "write_file" | "edit_file" => match text("path") {
            Some(path) => format!("{name} {path}"),
            None => name.to_owned(),
        },
        "run_command" => {
            let command = jobs::short_command(text("command").unwrap_or_default());
            if arguments.get("background").and_then(Value::as_bool) == Some(true) {
                format!("run_command (background): {command}")
            } else {
                format!("run_command: {command}")
            }
        }
        "memory" | "job" => {
            let parts: Vec<&str> = [text("command"), text("name"), text("id")]
                .into_iter()
                .flatten()
                .collect();
            format!("{name} {}", parts.join(" ")).trim_end().to_owned()
        }
        "skill" => match text("name") {
            Some(skill) => format!("skill {skill}"),
            None => "skill".to_owned(),
        },
        _ if name.starts_with("mcp__") => format!("MCP: {}", crate::approvals::display_name(name)),
        _ => name.to_owned(),
    }
}

fn set_phase(state: &mut AgentState, phase: TurnPhase) {
    if let Some(turn) = state.active_turn_mut() {
        turn.phase = phase;
    }
}

fn tool_result(call_id: &CallId, outcome: ToolOutcome) -> HistoryEntry {
    HistoryEntry::ToolResult(ToolResultEntry {
        call_id: call_id.clone(),
        outcome,
    })
}
