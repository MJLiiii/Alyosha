# Tool Executor

The Tool Executor runs **one tool call** and returns its outcome to the caller. The Agent Loop calls it for each tool requested by the model. A Code Mode script can also call it through a restricted host interface. Both paths use the same Tool Registry and dispatch checks. [Tool Registry](ToolRegistry.md) · [Agent Loop](AgentLoop.md)

## Shared dispatch

The caller supplies the tool name and JSON arguments. Alyosha supplies the session's tool context, the caller's scope, and the active cancellation state.

1. Look up the exact name in the Tool Registry and check whether the scope allows that tool.
2. Validate the arguments against the registered input schema.
3. If the configuration marks the tool `ask`, show the owner the exact call and wait for an answer. A decline, no answer in time, or a request that cannot be delivered returns a tool error, and the tool does not run. [Approvals](Approvals.md)
4. Check cancellation and admit the call in one step. Pass the handler a context limited to the caller's scope. The Environment Interface enforces that scope at the file open or shell launch.
5. Return the handler's output or an error. An unknown name, forbidden tool, or invalid argument returns a tool error before a handler runs. [Environment Interface](EnvironmentInterface.md) · [Tool permissions](reference/ToolPermissions.md)

| Caller | Scope | Where the outcome goes |
| --- | --- | --- |
| Agent Loop | Session tool permissions | The loop applies the Context Manager's result limits and records one result under the original model call ID. The loop owns the order of the top-level batch. |
| Code Mode host, on behalf of a script | Workspace file reads only. No `/skills` paths, `memory`, `skill`, or MCP tools. | The script receives each read result or error. Its outer `execute_code` outcome goes back to the loop. |

## Read-only Code Mode

`execute_code` lets the model read and filter workspace files in one call. For example, a script can read two sets of meeting notes and return only the lines marked “Decision”. The model sees one `execute_code` result on its next request.

In v1, `execute_code` runs JavaScript with QuickJS-NG through the `rquickjs` Rust bindings. Each invocation starts a fresh sandbox. Its only host operation is `tools.read_file`, restricted to regular files in the selected workspace. The sandbox can compute over the returned text in memory. File writes, shell commands, direct filesystem access, session logs, networking, environment variables, credentials, module loading, and processes are outside its scope. [rquickjs](https://github.com/DelSkayn/rquickjs) · [Built-in tools](reference/BuiltInTools.md) · [Code Mode contract and example](#run-read-only-code-mode)

## Stop and cleanup

If Stop arrives during an admitted filesystem call, the loop waits for its actual outcome. During a shell call, the Environment Interface kills the sandbox and returns the output so far with the status `Stopped`, because a command may run for up to 30 minutes. During `execute_code`, the host interrupts the script even while it is computing, waits for any admitted read, then returns a cancelled outer result. During an MCP call, the handler sends the server a cancel notification and returns a cancelled result without waiting for a reply. [MCP Client](MCP.md#stop-timeouts-and-failures) The loop records that outcome and cancellations for the remaining unstarted model calls, then ends the turn as `Stopped`. [Agent State](AgentState.md) · [Implementation](#implementation) · [Architecture overview](README.md)

## Implementation

Use the same dispatcher for a direct model call and a read requested inside Code Mode. The Agent Loop owns the top-level call IDs, batch order, and history writes. [Agent Loop](AgentLoop.md#implementation) · [Architecture overview](README.md)

### Dispatch one call

| Input or output | Contract |
| --- | --- |
| Name and arguments | Exact registered name and a JSON value from the caller. |
| Context and scope | Session `ToolContext` plus a host-selected scope for a direct call or a Code Mode read. |
| Cancellation | The active turn's cancellation state, shared with inner reads. |
| Outcome | Handler output, tool error, or cancellation. Keep continuation references and truncation metadata separate from preview text. |

Use `registry.get(name)`, check the caller's scope, then validate the arguments against the returned definition. If the definition requires approval, ask the owner before admission. [The dispatch step](Approvals.md#the-dispatch-step) Coordinate admission with Stop so a call cannot slip between the cancellation check and the decision to start. Await the registered handler with validated arguments and a scoped `ToolContext` carrying the active cancellation token. Its file and shell operations go through the Environment Interface. Return the outcome to the caller. [Registry contract](ToolRegistry.md#implementation) · [Environment Interface](EnvironmentInterface.md#implementation) · [Tool permissions](reference/ToolPermissions.md)

### Run read-only Code Mode

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

Leave the runtime's module loader unset and expose only the read callback. For `tools.read_file({ path, offset?, max_bytes? })`, the Environment Interface resolves relative paths from the session working directory, permits only regular workspace files at open time (including symlink resolution), and excludes the session log store. Resolve the promise with `text` and, when more data remains, `next_offset`. Permit at most one read in flight. Reject an overlapping attempt or a failed read through its promise. [Read contract](reference/BuiltInTools.md#read_file) · [Environment Interface](EnvironmentInterface.md#implementation)

### Results and limits

V1 leaves script size, elapsed time, memory, and the total number of inner reads without additional Alyosha quotas. This also holds in bridge sessions and scheduled tasks, where a runaway script blocks that chat's queue until the owner sends `/cancel`. Per-call limits were considered and left out. An uncaught script exception or sandbox startup failure becomes a tool error for the outer call. [Telegram commands](TelegramBridge.md#incoming-messages)

The loop applies its usual UTF-8 preview and batch budgets to the returned string and records a truncation indication if it shortens the text. Inner results keep the normal per-read byte limits and continuation offsets. Only the outer call consumes a result slot in conversation history. [Tool result limits](ContextManager.md#v1-tool-result-limits)

For a direct file read, return the chunk and whether more text remains so the loop can compute `next_offset` from the bytes it records. For a shell command or an MCP call, base log retention on the final recorded preview. One implementation can keep a complete temporary capture until the loop has sized the preview, then ask the log store to issue a `log_path` or release the unused capture. If the full capture failed, keep the reason when the final preview omits output. This also covers a short command or MCP result whose text fits the per-result limit but exceeds the space left in its batch. An MCP handler already holds its whole text in memory, so it has no capture to fail and only the save can. [File and command results](reference/BuiltInTools.md) · [Session logs](EnvironmentInterface.md#session-logs)

### Cancellation and teardown

Install an `AsyncRuntime` interrupt handler before evaluation. Have it check the active cancellation token so Stop can interrupt JavaScript computing between reads. While waiting for a promise, also listen for cancellation on the Rust side so Stop can wake a script whose promise never settles. Close the host interface to new calls on every exit and await any admitted read before dropping the runtime. Report Stop as cancellation. An admitted filesystem call returns its actual outcome even when Stop arrives during execution. An admitted shell call returns `Stopped` with its output so far. An admitted MCP call returns cancellation once the handler has sent the cancel notification. [MCP cancellation](MCP.md#cancellation-and-deadlines) [rquickjs interrupt handler](https://docs.rs/rquickjs/latest/rquickjs/struct.AsyncRuntime.html#method.set_interrupt_handler) · [Agent Loop](AgentLoop.md#implementation)

### Verification

- Give a fake handler valid arguments, then invalid arguments, an unknown name, and a call outside its scope. Check that only the valid call starts the handler.
- Run a script that reads two workspace files in sequence. Check that both use the registered handler, that a session log read is denied, and that the loop records one outer result. Try overlapping reads and check that the second promise rejects.
- Stop with an inner read in flight. Check that new reads are rejected and the outer cancellation is returned only after the admitted read finishes. Separately, stop a script that loops without calling a tool and one waiting on a promise that never settles. Check that each releases the runtime and returns a cancelled result.
- Give a command less space in the batch than its output needs, with the complete output small enough to retain. Check that the recorded result carries the shorter preview and a reference that opens the whole log.
