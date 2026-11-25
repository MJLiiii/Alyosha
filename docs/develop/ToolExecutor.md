# Implement the Tool Executor

Use the same dispatcher for a direct model call and a read requested inside Code Mode. The Agent Loop owns the top-level call IDs, batch order, and history writes. [Component design](../design/ToolExecutor.md) · [Agent Loop](AgentLoop.md) · [Architecture overview](../README.md)

## Dispatch one call

| Input or output | Contract |
| --- | --- |
| Name and arguments | Exact registered name and a JSON value from the caller. |
| Context and scope | Session `ToolContext` plus a host-selected scope for a direct call or a Code Mode read. |
| Cancellation | The active turn's cancellation state, shared with inner reads. |
| Outcome | Handler output, tool error, or cancellation. Keep continuation references and truncation metadata separate from preview text. |

Use `registry.get(name)`, check the caller's scope, then validate the arguments against the returned definition. If the definition requires approval, ask the owner before admission. [The dispatch step](Approvals.md#the-dispatch-step) Coordinate admission with Stop so a call cannot slip between the cancellation check and the decision to start. Await the registered handler with validated arguments and a scoped `ToolContext` carrying the active cancellation token. Its file and shell operations go through the Environment Interface. Return the outcome to the caller. [Registry contract](ToolRegistry.md) · [Environment Interface](EnvironmentInterface.md) · [Tool permissions](../reference/ToolPermissions.md)

## Read-only Code Mode

The `execute_code` handler takes a `code` string containing a JavaScript async function expression. Use QuickJS-NG through `rquickjs` with its `futures` feature. Create an `AsyncRuntime` and `AsyncContext` per invocation. Evaluate the expression in that context, call it with a `tools` object, and drive the returned promise to a string. A rejected promise, invalid expression, or non-string return becomes an outer tool error. [rquickjs async context](https://docs.rs/rquickjs/latest/rquickjs/struct.AsyncContext.html)

Register one async Rust callback on `tools`. The callback submits `read_file` to the shared dispatcher with this invocation's workspace-only scope and cancellation token. Include its arguments and return fields in the description of `execute_code` so the model can write scripts such as this one. It looks for decisions in the first chunk of two meeting notes.

```javascript
async (tools) => {
  const decisions = [];
  for (const path of ["notes/monday.md", "notes/friday.md"]) {
    const { text } = await tools.read_file({ path, max_bytes: 4096 });
    for (const line of text.split("\n")) {
      if (line.startsWith("Decision:")) {
        decisions.push(path + "  " + line.trim());
      }
    }
  }
  return decisions.join("\n");
}
```

Leave the runtime's module loader unset and expose only the read callback. For `tools.read_file({ path, offset?, max_bytes? })`, the Environment Interface resolves relative paths from the session working directory, permits only regular workspace files at open time (including symlink resolution), and excludes the session log store. Resolve the promise with `text` and, when more data remains, `next_offset`. Permit at most one read in flight. Reject an overlapping attempt or a failed read through its promise. [Read contract](../reference/BuiltInTools.md#read_file) · [Environment Interface](EnvironmentInterface.md)

## Results and limits

V1 leaves script size, elapsed time, memory, and the total number of inner reads without additional Alyosha quotas. This also holds in bridge sessions and scheduled tasks, where a runaway script blocks that chat's queue until the owner sends `/cancel`. Per-call limits were considered and left out. An uncaught script exception or sandbox startup failure becomes a tool error for the outer call. [Telegram commands](TelegramBridge.md#incoming-messages)

The loop applies its usual UTF-8 preview and batch budgets to the returned string and records a truncation indication if it shortens the text. Inner results keep the normal per-read byte limits and continuation offsets. Only the outer call consumes a result slot in conversation history. [Tool result limits](ContextManager.md#v1-tool-result-limits)

For a direct file read, return the chunk and whether more text remains so the loop can compute `next_offset` from the bytes it records. For a shell command or an MCP call, base log retention on the final recorded preview. One implementation can keep a complete temporary capture until the loop has sized the preview, then ask the log store to issue a `log_path` or release the unused capture. If the full capture failed, keep the reason when the final preview omits output. This also covers a short command or MCP result whose text fits the per-result limit but exceeds the space left in its batch. An MCP handler already holds its whole text in memory, so it has no capture to fail and only the save can. [File and command results](../reference/BuiltInTools.md) · [Session logs](EnvironmentInterface.md#session-logs)

## Cancellation and teardown

Install an `AsyncRuntime` interrupt handler before evaluation. Have it check the active cancellation token so Stop can interrupt JavaScript computing between reads. While waiting for a promise, also listen for cancellation on the Rust side so Stop can wake a script whose promise never settles. Close the host interface to new calls on every exit and await any admitted read before dropping the runtime. Report Stop as cancellation. An admitted filesystem call returns its actual outcome even when Stop arrives during execution. An admitted shell call returns `Stopped` with its output so far. An admitted MCP call returns cancellation once the handler has sent the cancel notification. [MCP cancellation](MCP.md#cancellation-and-deadlines) [rquickjs interrupt handler](https://docs.rs/rquickjs/latest/rquickjs/struct.AsyncRuntime.html#method.set_interrupt_handler) · [Agent Loop](AgentLoop.md)

## Verification

- Give a fake handler valid arguments, then invalid arguments, an unknown name, and a call outside its scope. Check that only the valid call starts the handler.
- Run a script that reads two workspace files in sequence. Check that both use the registered handler, that a session log read is denied, and that the loop records one outer result. Try overlapping reads and check that the second promise rejects.
- Stop with an inner read in flight. Check that new reads are rejected and the outer cancellation is returned only after the admitted read finishes. Separately, stop a script that loops without calling a tool and one waiting on a promise that never settles. Check that each releases the runtime and returns a cancelled result.
- Give a command less space in the batch than its output needs, with the complete output small enough to retain. Check that the recorded result carries the shorter preview and a reference that opens the whole log.
