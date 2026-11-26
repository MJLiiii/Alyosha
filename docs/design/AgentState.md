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

[Architecture overview](../README.md) · [Agent State implementation guide](../develop/AgentState.md)
