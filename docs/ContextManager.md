# Context Manager

The Context Manager decides which parts of the session history to send to the model. It works with the model's limits, the tool definitions for this request, and any saved checkpoint. A checkpoint pairs a summary of older entries with the position where recent history begins. The Context Manager returns a conversation that fits the input budget or reports why it cannot build one. Agent State retains the full recorded history for the session. [Architecture overview](README.md) · [Agent State](AgentState.md)

## What the model sees

Before the first compaction, the Agent Loop sends every recorded entry if the request fits. After compaction the request carries a model-written summary of older entries followed by recent entries in their original order. The opening user message of the active turn always appears verbatim, once. A retained assistant message with tool calls travels with all its results so the model can match each result to its call.

For example, a single user request may take five tool batches. If the latest batch fits and the earlier four need summarizing, the next request can carry the summary, the original user message, and the fifth exchange verbatim. A later turn can still find the original exchanges in session history.

## When to compact

| Trigger | What happens |
| --- | --- |
| Before the first model request in a turn and after a completed tool batch | Estimate the whole request, including tool definitions. Reserve space for the reply and estimation error. Compact if the request exceeds the usable input budget. |
| The provider reports a context-window error | The Agent Loop can request forced compaction once in the turn. It retries on the same route only if the new request fits and is smaller than the one the provider rejected. |
| The user runs `/compact` | An idle session can compact older history before the next task, even below the usual threshold. |

The Context Manager prefers a boundary between user turns. Within a long turn it can cut before an assistant response, keeping recent exchanges intact. The target for recent history is one quarter of the usable input budget, capped at 20,000 tokens. The current user message and latest completed batch take priority over that target.

To summarize, the Context Manager calls the LLM Client on the pinned provider and model with an empty tool list. It gives the model the previous summary and the newly omitted entries. The summary preserves the goal, user corrections and constraints, decisions, observed outcomes, open work, and references such as paths and errors. The session driver commits a checkpoint only after a complete summary and a successful budget check. Stop or a failed summary leaves the previous checkpoint available. [LLM Client / Gateway](LLMClientGateway.md) · [Agent Loop](AgentLoop.md)

## Large tool results

Tool output is bounded when it enters history. The Context Manager sets the text budget, up to 8 KiB per result and 16 KiB per batch. The loop records each call ID, outcome, and continuation reference even if space for text runs out. A partial file read carries the next byte offset. For shortened command output or a shortened MCP result, the Environment Interface can keep the full text in a session file accessible through a bounded read. [MCP Client](MCP.md#one-call) If the required entries still cannot fit, the Context Manager reports a context-window error. [Built-in tools](reference/BuiltInTools.md) · [Environment Interface](EnvironmentInterface.md)

## `/compact` command

Run bare `/compact` between turns with an empty message queue. A busy session asks you to try again when idle. During automatic or manual summarization the status area shows **Compacting context…**. Stop cancels active work. Messages that arrive meanwhile wait in order. A manual command reports its result in the status area, including **No older context to compact** if everything is already recent. The next turn uses the latest committed checkpoint. [Implementation and verification](#implementation)

## Implementation

Given a snapshot of session history, the current checkpoint, and request settings, produce the conversation for the next model request. Request settings include system instructions, tool definitions, the pinned route, its model limits, and the reply allowance. The Agent Loop supplies those settings and sends the prepared request. When compaction is needed, the Context Manager calls `generate` for a summary and returns a candidate checkpoint to the session driver. [Architecture overview](README.md) · [LLM Client contract](LLMClientGateway.md#v1-caller-contract)

### V1 state and request contract

`AgentState.context_checkpoint` starts as `None`. A `ContextCheckpoint` holds `summary: String` and `first_kept_index: usize`, the index of the first retained entry in the append-only `history`. Only the session driver commits a replacement for active work. The summary is session metadata and adds zero entries to conversation history. [Agent State contract](AgentState.md#implementation)

With no checkpoint, select the full history. Otherwise supply the summary separately as conversation data and select `history[first_kept_index..]`. If the active turn's opening `User` entry precedes the slice, insert the original entry ahead of the slice. Send it once in its user role. Preserve the order, call IDs, and provider replay fields of retained entries. The LLM Client encodes the summary separately from system instructions.

### V1 token budgets

Use one text estimator across native routes. Round up model-visible UTF-8 bytes divided by three and include native message framing. Count each image part as 1,600 tokens, whatever its size, in user entries and in tool results alike. [Context and compaction](Media.md#context-and-compaction) Count system instructions, summary, user and assistant content, tool schemas, call arguments and results, and replay fields that use context. Calibrate the estimate against provider-reported input usage.

| Budget | V1 calculation |
| --- | --- |
| Estimation margin | 10% of the model's context window, rounded up, with a 1,024-token minimum. |
| Ordinary reply reservation | Reserve the route’s documented default output allowance. If it publishes only a maximum, reserve that maximum locally. |
| Usable input | Subtract the reply reservation from the context window, cap the result at the route’s input ceiling if it has one, then subtract the estimation margin. For a summary request use the summary reply allowance below. |
| Recent-history target | One quarter of usable input, up to 20,000 tokens. Keep fewer entries when the rest of the request needs space. |
| Summary reply | Up to 4,096 tokens within the model's limits. Estimate the summary request against its own usable input budget. |

Route limits come from the model ID's family until GPROXY or the configuration supplies them: `gpt-` 400,000 tokens with a 128,000 reply reservation, `claude-` 200,000 with 32,000, `gemini-` 1,048,576 with 65,536, `deepseek-` 128,000 with 8,192, and anything else 128,000 with 16,000. They live in `RouteLimits::for_model`.

### V1 tool result limits

Apply these limits as the Agent Loop records each top-level `ToolResult`. The text allowance is at most 8 KiB per result and 16 KiB across the batch. Reduce the allowance if less room remains after accounting for the active user message, request overhead, and the batch's required call and result metadata. Count UTF-8 bytes and cut on character boundaries. Keep the call ID, outcome, and any continuation reference even when the text allowance is zero. A retained recent batch uses these recorded results verbatim. [Agent Loop tool contract](AgentLoop.md#v1-tool-contract)

For a shortened file read, compute the next offset from the bytes actually recorded. For command output and MCP results, the host keeps the full text in a private session file when the preview omits text, returns a reference readable in bounded chunks, and cleans up on session exit. If saving fails, preserve the tool's actual outcome and record that the omitted output is unavailable. [Built-in tool contracts](reference/BuiltInTools.md) · [Environment Interface](EnvironmentInterface.md#implementation)

### Prepare the next model request

1. After the opening `User` entry or a complete tool batch has been recorded, build the conversation and estimate the full request, including the registry's definitions. If it fits, return it.
2. Otherwise reserve space for a summary. Walk backward to keep a recent slice within the remaining budget and the recent-history target. Prefer a cut at a `User` entry. Within a long turn a cut immediately before an `Assistant` entry is also valid. A tool-calling assistant entry and all its results form one unit. Keep the active user message and latest completed batch, even if they exceed the recent-history target. If required entries exceed the input budget or no valid cut advances the boundary, return a context-window error.
3. Send the pinned model the previous summary and the newly omitted range, from the previous kept index (or zero) to the new cut. Label the transcript as conversation data. If the cut passes the opening user message, include it in this transcript so later turns retain its requirements. Use a maximum 2 KiB excerpt for each older tool result and include its outcome and reference separately. Show each image in a user entry as `[image]`; the summary request carries no images. Use the active cancellation token, the summary reply limit, and an empty tool list. If the summary request exceeds its own budget, return a context-window error.
4. Require a complete reply with nonempty summary text and zero tool calls. Rebuild and estimate the original request using the candidate checkpoint. If it fits, return the candidate and the prepared request to the driver. The driver checks for Stop when committing the checkpoint and sends the prepared request if work continues. The commit also saves the checkpoint to the session store and rebuilds the system instruction with fresh memory and skills indexes. The prepared request goes as built, and later requests use the new instruction. [After each compaction](SessionStore.md#after-each-compaction) A failed, cancelled, or truncated compaction leaves the previous checkpoint in place. An oversized rebuilt request returns a context-window error.

Each preparation makes at most one summary request. During a turn, keep the phase at `Generating` while waiting for it. Expose **Compacting context…** only while processing a summary request, including a recovery or manual summary. Clear that status on every exit. On cancellation end the active turn as `Stopped`. Other preparation failures use the normal turn failure path. [State transitions](AgentState.md#state-transitions)

#### Summary prompt

Request one concise Markdown handoff with five sections.

1. **Goal and user constraints.** State the goal and requirements still in force. Quote short requirements when exact wording matters.
2. **Decisions.** Capture choices that affect the remaining work and their rationale when known.
3. **Completed actions and observed outcomes.** Ground each outcome in its recorded tool result. Keep unverified actions open.
4. **Open work.** Include next steps, questions, and blockers.
5. **References.** Keep the exact paths, identifiers, artifact references, and error text needed to continue.

On later compactions, carry forward facts that still apply and use newly omitted entries to update them. Apply explicit user corrections when they supersede an earlier requirement.

### Recover from provider context overflow

The loop owns the retry count. An explicit provider context-window error on a normal model request qualifies for one recovery attempt per user turn. Set the flag in `run_turn` before recovery. Local budget failures, summary errors, and output truncation follow their normal error paths. [LLM Client errors](LLMClientGateway.md#v1-caller-contract)

Force compaction even if the rejected request passed the local estimate. Use the same safe cuts and summary rules over committed history. Return a candidate for commit and retry only if its boundary advances and the rebuilt request fits *and* is smaller than the rejected request. If no candidate qualifies, return the context-window error. The loop resends the prepared conversation with the same provider, model, definitions, and cancellation token. Existing tool results stay in history. If the retry fails, report the failure. A valid checkpoint committed before a failed retry remains available.

### Run `/compact`

The session driver accepts bare `/compact` only when idle with an empty queue, reserves the work slot atomically, pins the route, and creates a fresh cancellation token. New user messages wait in the usual FIFO queue. [Session driver](AgentLoop.md#session-driver) · [Manual compaction state](AgentState.md#manual-compaction-state)

Use the normal compaction algorithm with the threshold check disabled. If the boundary cannot advance, return **No older context to compact** without calling the model. Otherwise compare the original and rebuilt context using the same settings and estimator. Commit a complete summary only when it produces a smaller request within budget. Report **Context compacted** on commit, **Context unchanged** if the request did not shrink, or the reason for failure.

The driver checks for Stop before committing. On cancellation keep the previous checkpoint and report **Compaction stopped**. Release the work slot on every exit. Return the command status outside history and turn outcomes. An accepted command makes at most one summary request.

### Verify the contracts

For a local budget fixture, give a synthetic route a 20,000-token context window, a 4,000-token default reply allowance, and a 12,000-token input ceiling. The ordinary input budget is 10,000 tokens after the 2,000-token margin, and its recent-history target is 2,500. Inject a scripted `generate` so the test can inspect the summary request and supply a fixed reply. [Model client contract](LLMClientGateway.md#v1-caller-contract)

| Scenario | Expected observation |
| --- | --- |
| Below budget, then over budget | The first request preserves entry order and makes zero summary calls. The second makes one text-only summary call on the pinned route and uses its summary with the verbatim recent slice. Full recorded history remains available. |
| Cross the threshold after a batch in a long turn | The active user entry appears once. Each retained call keeps all results and replay fields. Check the encoded summary and exchanges in each native route fixture. Earlier tools execute once. |
| Large tool outputs | Exercise the per-result and batch limits and a smaller remaining budget. Check call IDs, outcomes, corrected file offsets, and a usable temporary log reference for both a command and an MCP result. Check log cleanup on session end. In a summary request, verify the 2 KiB excerpts and separate outcomes and references. |
| Successive compactions | The second summary gets the previous summary plus only the newly omitted range. A new session starts without a checkpoint. A resumed session starts with its saved checkpoint. [Durable sessions](SessionStore.md#agent-state-changes) |
| Failure and Stop | Use an oversized required entry, an oversized rebuilt request, a truncated summary, and a failed summary. Check the error and previous checkpoint. Stop a summary with a message queued and verify a `Stopped` outcome and normal startup of the next turn. Check progress clears on every exit. |
| Provider overflow after a completed tool batch | Force compaction and retry once on the same route with a smaller request. Check the original user entry appears once and the batch executes once. Test a second overflow in the turn, cancellation, and no usable cut. Check each ends recovery, and output truncation uses the ordinary error path. |
| Manual compaction | Test below-threshold success, only recent history, and a busy session with a queue. During a manual summary, queue two messages and check FIFO order after success, failure, and Stop. Check the command creates zero history entries and turn outcomes. |

To check summary quality with a configured provider, run two real-model compactions, moving the facts under test outside the recent slice. In one scenario, confirm a write and then fail validation. The next action should follow the validation error and its log reference. In another, correct an earlier requirement while keeping a second constraint. The model's next action should honor both the correction and the continuing constraint.

For background on conversation compaction in another agent, see [Pi’s compaction notes](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/compaction.md).
