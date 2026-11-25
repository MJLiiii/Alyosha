# Context Manager

The Context Manager decides which parts of the session history to send to the model. It works with the model's limits, the tool definitions for this request, and any saved checkpoint. A checkpoint pairs a summary of older entries with the position where recent history begins. The Context Manager returns a conversation that fits the input budget or reports why it cannot build one. Agent State retains the full recorded history for the session. [Architecture overview](../README.md) · [Agent State](AgentState.md)

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

Tool output is bounded when it enters history. The Context Manager sets the text budget, up to 8 KiB per result and 16 KiB per batch. The loop records each call ID, outcome, and continuation reference even if space for text runs out. A partial file read carries the next byte offset. For shortened command output or a shortened MCP result, the Environment Interface can keep the full text in a session file accessible through a bounded read. [MCP Client](MCP.md#one-call) If the required entries still cannot fit, the Context Manager reports a context-window error. [Built-in tools](../reference/BuiltInTools.md) · [Environment Interface](EnvironmentInterface.md)

## `/compact` command

Run bare `/compact` between turns with an empty message queue. A busy session asks you to try again when idle. During automatic or manual summarization the status area shows **Compacting context…**. Stop cancels active work. Messages that arrive meanwhile wait in order. A manual command reports its result in the status area, including **No older context to compact** if everything is already recent. The next turn uses the latest committed checkpoint. [Implementation guide and verification](../develop/ContextManager.md)
