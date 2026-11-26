# Implement the Agent Loop

One session driver serializes work. Its asynchronous `run_turn` drives one user turn and returns its final assistant reply, a failure, or an interruption. The caller displays the returned reply. `run_turn` is the sole writer of conversation history. [Agent Loop design](../design/AgentLoop.md) · [Agent State fields and invariants](AgentState.md)

## Session driver

- **Session startup.** Create the in-memory state and its FIFO input queue, filled from the session store when resuming. [Durable sessions](SessionStore.md#resume) Start the configured MCP servers. Build the normal system instruction once, including the memory and skills indexes. [Memory index](Memory.md#build-the-index) · [Skills index](Skills.md#build-the-index) · [MCP startup](MCP.md#start-servers-and-register-tools) Create a `ToolContext` carrying the session’s workspace permissions. Give `run_turn` access to that context, the startup registry, and a Tool Executor backed by the same registry. [Environment Interface](EnvironmentInterface.md) · [Tool Registry](ToolRegistry.md)
- **User input.** Enqueue each message with its own ID. Use a single consumer to reserve the free work slot and take the oldest message in one state update. Capture the model and a fresh cancellation token for the turn. Keep accepting input during async work.
- **Manual compaction.** Recognize bare `/compact` before allocating a message ID. Reserve the slot only if the slot and queue are both empty, checking both in the same state update. Otherwise return a busy status immediately. Run an accepted command through the Context Manager and return its status separately. [Command contract](ContextManager.md#compact-command)
- **Job command.** Recognize `/job <command>` before allocating a message ID. Start the job through the session's `JobManager` without touching the slot or queue, and return its status separately. [`/job` command](Jobs.md#job-command)
- **Stop and completion.** Read the active slot and signal its token in the same short state update. On every exit, commit the turn outcome or finish the command, release the slot, and start the next queued turn with a fresh token. [State transitions](AgentState.md#state-transitions)

## Turn workflow

1. Append the opening `User` message once, with the clock line as its first line: `[`, the weekday as three letters, the date as `YYYY-MM-DD`, the 24-hour time as `HH:MM`, `, `, the configured timezone name, and `]`. Use the time the message was submitted, not when its turn started, in the configured timezone. Store the line in the history entry, so summaries and later requests see the same text. [Clock line](../design/AgentLoop.md#user-message-turn) Pin the session’s provider and this turn’s model for normal requests, summaries, and retries.
2. Get `registry.definitions()` and ask the Context Manager to prepare history with those definitions included in the token estimate. If compaction succeeds and the work is still active, commit the candidate checkpoint to Agent State. Send the prepared request and definitions through the LLM Client / Gateway. Summary requests use an empty tool list. [Context Manager contract](ContextManager.md) · [Generation contract](LLMClientGateway.md#v1-caller-contract)
3. If the gateway reports an explicit context-window error on a normal request, allow one recovery attempt per turn. Use the Context Manager’s forced compaction checks, then retry the smaller request with the same definitions, provider, model, and token. A partial reply from a failed request produces zero history entries and zero tool calls. Propagate unrecoverable errors and Stop to the session driver. [Overflow recovery](ContextManager.md#recover-from-provider-context-overflow)
4. Append the complete `Assistant` entry, preserving the order of its text and calls and any fields needed for provider replay. If it has no calls, return the recorded reply, including a clarifying question.
5. For a response with calls, process the batch below. After all results have been recorded, repeat step 2 unless Stop ended the turn.

## V1 tool contract

Process top-level calls in model order. Before each dispatch check Stop. Pass the call name, JSON arguments, `ToolContext`, direct-call scope, and active token to the Tool Executor. The executor resolves the name, checks scope, validates arguments, and runs the handler. For each call, use the Context Manager’s preview limits and append one `ToolResult` under the model’s original call ID. A tool error goes into the batch and the model can act on it. If an output carries a notice, pass it to the session driver to show in the status area. [Memory notices](Memory.md#memory-tool) [Shared dispatch](../design/ToolExecutor.md#shared-dispatch) · [Result limits](ContextManager.md#v1-tool-result-limits)

The `execute_code` host sends its inner reads through that executor with workspace-only access. The script receives those results. The loop records the outer call’s result. [Read-only Code Mode](../design/ToolExecutor.md#read-only-code-mode)

If Stop arrives during a batch, record the admitted call’s outcome, append a cancelled result for each unstarted call, then return an interruption. An admitted file call yields its actual outcome. An admitted shell call yields `Stopped` with the output captured so far. An active `execute_code` script yields cancellation after cleanup. An active MCP call yields cancellation as soon as the cancel notification is sent. [MCP cancellation](MCP.md#cancellation-and-deadlines) [Stop and cleanup](../design/ToolExecutor.md#stop-and-cleanup)

## Verification with a scripted model and fake tools

Use the session driver as the entry point for the first integration test. Start with empty history and inject a scripted `generate` that records each request. Register a fake `read_file` handler that returns `alpha` for `a.txt` and a missing-file error for `b.txt`. Give the test route enough context to fit the whole exchange. Once implemented, this fixture can run under `cargo test` using only local test data. [Model client contract](LLMClientGateway.md#v1-caller-contract) · [Startup registration](ToolRegistry.md#register-built-in-tools-at-startup)

Use a fixed clock at Wednesday 2026-11-25 10:02 in `Europe/Amsterdam`. Submit “Read a.txt and b.txt.” Have the first model reply request both reads in order. Assert that the second request advertises the same tool definitions and contains these history entries.

```text
User           [Wed 2026-11-25 10:02, Europe/Amsterdam]
               Read a.txt and b.txt.
Assistant      "I'll read both." + call A: read_file({"path": "a.txt"}) + call B: read_file({"path": "b.txt"})
Tool result A  "alpha"
Tool result B  Error: file not found
```

Have the second request return the text “a.txt contains alpha. b.txt was missing.” with zero calls. Check that the caller receives this reply, that history contains the four entries above followed by this reply once, and that the submitted message has a `Completed` outcome. Expect exactly two model requests and two tool executions, with the work slot empty at the end.

| Fixture | Expected behavior |
| --- | --- |
| Direct reply or clarifying question | One request, zero tool calls, one recorded reply. |
| Messages B and C arrive during A | A sees only its own inputs and the prior history. B and C start in arrival order after A. |
| `/compact` submitted during a turn or with a queued message | A busy status and the same queue. If accepted while idle, the command adds zero history entries and zero turn outcomes. Messages submitted during compaction start in arrival order after it ends. |
| Unknown tool name, invalid arguments, then a valid call | The normal request advertises the registered tool. Each rejected call has its own error result. Only the valid call runs and all three results keep their IDs and order. |
| `execute_code` makes two reads | Both inner reads use shared dispatch. The next model request includes one outer result under the script call ID. |
| Interrupted or truncated response containing a tool call | The gateway returns an error and the loop dispatches zero calls from that response. |
| Stop during file read A with B pending and a message queued | Record A’s actual result and B’s cancellation. The next model request belongs to the queued turn and includes both results. |

[Architecture overview](../README.md)
