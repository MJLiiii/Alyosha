# Agent State

Agent State is the in-memory record of one session. The session driver owns the queue, active work, and turn outcomes. Its turn driver, `run_turn`, appends conversation history. Each history entry and context checkpoint is also saved to the session store before it takes effect, so a restarted process can resume the session. The queue, active work, and turn outcomes are not saved. [Agent Loop](AgentLoop.md) · [Durable sessions](SessionStore.md)

| What the state holds | Why it matters |
| --- | --- |
| Provider account | The account stays fixed throughout the session. The model is selected at the start of each turn or manual compaction. In v1 it comes from the configuration when the session starts, so every turn uses the same one. Pinning it per turn keeps a turn on one model if switching is added later. |
| Pending messages | Each submission gets a unique message ID and waits in arrival order. Its ID becomes the turn ID when the turn starts, even if two submissions have identical text. |
| Active work | One slot identifies the current turn or manual compaction, the selected model, and the Stop signal for that work. |
| Conversation history | When a turn starts, its message moves into history. Each complete assistant response keeps its ordered text and calls together. Each tool result keeps its original call ID. |
| Context checkpoint | The latest summary and the position where verbatim history resumes. The Context Manager uses them to build a shorter request. The original history stays available. [Context Manager](ContextManager.md) |
| Turn outcomes | Each finished turn has one outcome, keyed by its opening message ID, until the session ends. |

## During a turn

The active turn records whether it is waiting for the model, processing a tool batch, or waiting for the owner to answer an approval or question. [Approvals](Approvals.md) During a call it also records the call ID. For example, if a turn requests file reads A and B and you press Stop during A, the state shows A as active and Stop as requested. When A finishes, history records its actual result and a cancelled result for B. The driver closes the turn as `Stopped`.

Manual compaction occupies the same slot and receives its own Stop signal. The driver can still queue user messages. On success it saves the new checkpoint. The command reports its status outside history and turn outcomes. [Manual compaction](ContextManager.md#compact-command)

## After a turn

| Outcome | Meaning |
| --- | --- |
| `Completed` | The assistant returned a final reply or clarifying question, recorded once in history. |
| `Stopped` | A Stop request ended the turn. |
| `Failed(reason)` | An unrecoverable error ended the turn. The reason identifies the failure. |

A successful file write followed by a model timeout leaves two facts available to the next turn. History records the successful write under its call ID. The outcome records the timeout under the turn ID.

[Architecture overview](README.md)

## Implementation

Create one in-memory `AgentState` per session. The Agent Loop’s session driver coordinates updates, and `run_turn` alone appends history. Keep state updates short so input and Stop remain available during model requests and tool execution. [Session driver](AgentLoop.md#session-driver)

### Session fields

| Field | Rust representation | Lifetime and updates |
| --- | --- | --- |
| `provider` | Provider account identifier | Fixed when the session opens. |
| `history` | `Vec<HistoryEntry>` | Append `User`, complete `Assistant`, and `ToolResult` entries in order. A `User` entry holds ordered parts, text and image references, with the clock line and the message's text first. Preserve text, calls, their order, and native replay fields within each assistant entry. Keep each result’s original call ID. [Model reply contract](LLMClientGateway.md#implementation) · [Images](Media.md#images) |
| `context_checkpoint` | `Option<ContextCheckpoint>` | Latest summary and index of the first retained history entry. The Context Manager supplies the candidate. The driver commits a validated replacement for the active turn or manual compaction after checking cancellation. [Checkpoint contract](ContextManager.md#implementation) |
| `pending_messages` | `VecDeque<UserMessage>` | Hold each message, its session-unique ID, its origin, always `Owner`, and its attachments as they came, until its turn starts. Attachments are prepared into parts when the turn starts. [Preparing a message](Media.md#preparing-a-message) A finished background job is not queued directly: the bridge sends its text back as a prompt. [Background jobs](Jobs.md#when-a-job-finishes) |
| `active_work` | `Option<ActiveWork>` | Either `Turn(ActiveTurn)`, `ManualCompaction { model_id, cancel }`, or an empty slot. |
| `outcomes` | `HashMap<MessageId, TurnOutcome>` | One final `Completed`, `Stopped`, or `Failed(reason)` per finished user turn. |

Initialize the collections empty and the two optional fields to `None`, or, when resuming, fill `history` and `context_checkpoint` from the session store. The store also adds `session_id` and `base_seq`, and every history append and checkpoint commit goes through it first. [Durable sessions: Agent State changes](SessionStore.md#agent-state-changes) Keep the original history append-only across compactions. Gateway configuration and credentials have a separate lifetime. [LLM Client / Gateway](LLMClientGateway.md)

### Active turn fields

| Field | Contract |
| --- | --- |
| `turn_id` | Copy the opening message ID. It also keys the eventual outcome. |
| `model_id` | Capture the selected model at turn start. |
| `phase` | `Generating` during normal or summary requests. `RunningTools { active_call_id: Option<CallId> }` while processing a batch. `WaitingForOwner { call_id: CallId }` while an approval or `ask_owner` question for that call is open. [Approvals](Approvals.md#the-front-end-operation) |
| `stop_requested` | Read from this turn’s cancellation token. `active_call_id` can still identify an admitted call after Stop has been requested. |

### State transitions

| Event | State update |
| --- | --- |
| Driver starts a message | Reserve the slot and take the oldest queued message together. Set `Turn` to `Generating` with a fresh token. `run_turn` prepares its attachments, then appends the opening message once. Stop during preparation finishes as `Stopped` with nothing appended. [Preparing a message](Media.md#preparing-a-message) |
| Complete response arrives | Append one `Assistant` entry. With calls, enter `RunningTools` with `active_call_id = None`. With zero calls, finish as `Completed`. |
| A top-level call runs | Set `active_call_id` at dispatch. Append its result and clear the ID together. Between calls keep the phase at `RunningTools`. After a completed batch, return to `Generating` if the turn continues. |
| An approval or `ask_owner` question opens | Enter `WaitingForOwner` with the call's ID. When `ask` returns, go back to `RunningTools` with the same `active_call_id`. The front end shows **Waiting for you** meanwhile. [The front-end operation](Approvals.md#the-front-end-operation) |
| User requests Stop | Signal the active token. While waiting for the owner, the open request ends as `Cancelled` and the call does not run. During generation discard any partial reply. During tools record the active call’s outcome and a cancellation for each unstarted call. Finish as `Stopped`. [Tool contract](AgentLoop.md#v1-tool-contract) |
| Model request fails beyond recovery | Finish as `Failed(reason)` and preserve all committed entries. Include the reported error kind and a short explanation in the reason. [Overflow recovery](ContextManager.md#recover-from-provider-context-overflow) |

On every turn exit, record the outcome under `turn_id` and clear `active_work` in the same update. The driver may then take the next pending message.

### Manual compaction state

Reserve `ManualCompaction` with the selected model and a fresh token only if the slot and queue are both empty. Messages arriving during compaction can enter `pending_messages`. At command exit, check cancellation, commit any eligible checkpoint, and clear the slot in one update. Failure or Stop preserves the prior checkpoint. Report the command status separately from `history` and `outcomes`. [Command contract](ContextManager.md#run-compact)

### Verification

Drive the session through its normal input and Stop operations, taking state snapshots at the transitions under test. To exercise an interruption deterministically, let a fake file handler signal when it starts and wait for the test to release it. The test can then submit Stop and inspect the active call before the handler finishes. [Scripted turn fixture](AgentLoop.md#verification-with-a-scripted-model-and-fake-tools)

- Queue the same text twice. Verify that each message keeps a distinct ID, starts once in order, and has one outcome.
- Stop during file read A with B pending. Observe A’s ID and the Stop flag together. Check A’s actual result, B’s cancellation, the `Stopped` outcome, and a fresh token on the next turn.
- Open an approval with a scripted `Approver` that waits for the test. Check that the phase is `WaitingForOwner` with the call's ID, that it returns to `RunningTools` after the answer, and that Stop during the wait ends the turn `Stopped`.
- Fail a model stream after a successful tool call. History keeps the tool result and excludes the partial reply. The outcome carries the failure reason.
- Complete a turn with a clarifying question. Verify one reply, a `Completed` outcome, and an empty slot when the queue is empty. Start a fresh session and check all fields have their initial values.
- Stop manual compaction with a message queued. Verify the checkpoint is preserved, only the queued turn records an outcome, and that turn starts with a fresh token.

[Architecture overview](README.md)
