# Tool Executor

The Tool Executor runs **one tool call** and returns its outcome to the caller. The Agent Loop calls it for each tool requested by the model. A Code Mode script can also call it through a restricted host interface. Both paths use the same Tool Registry and dispatch checks. [Tool Registry](ToolRegistry.md) · [Agent Loop](AgentLoop.md)

## Shared dispatch

The caller supplies the tool name and JSON arguments. Alyosha supplies the session's tool context, the caller's scope, and the active cancellation state.

1. Look up the exact name in the Tool Registry and check whether the scope allows that tool.
2. Validate the arguments against the registered input schema.
3. If the configuration marks the tool `ask`, show the owner the exact call and wait for an answer. A decline, no answer in time, or a request that cannot be delivered returns a tool error, and the tool does not run. [Approvals](Approvals.md)
4. Check cancellation and admit the call in one step. Pass the handler a context limited to the caller's scope. The Environment Interface enforces that scope at the file open or shell launch.
5. Return the handler's output or an error. An unknown name, forbidden tool, or invalid argument returns a tool error before a handler runs. [Environment Interface](EnvironmentInterface.md) · [Tool permissions](../reference/ToolPermissions.md)

| Caller | Scope | Where the outcome goes |
| --- | --- | --- |
| Agent Loop | Session tool permissions | The loop applies the Context Manager's result limits and records one result under the original model call ID. The loop owns the order of the top-level batch. |
| Code Mode host, on behalf of a script | Workspace file reads only. No `/skills` paths, `memory`, `skill`, or MCP tools. | The script receives each read result or error. Its outer `execute_code` outcome goes back to the loop. |

## Read-only Code Mode

`execute_code` lets the model read and filter workspace files in one call. For example, a script can read two sets of meeting notes and return only the lines marked “Decision”. The model sees one `execute_code` result on its next request.

In v1, `execute_code` runs JavaScript with QuickJS-NG through the `rquickjs` Rust bindings. Each invocation starts a fresh sandbox. Its only host operation is `tools.read_file`, restricted to regular files in the selected workspace. The sandbox can compute over the returned text in memory. File writes, shell commands, direct filesystem access, session logs, networking, environment variables, credentials, module loading, and processes are outside its scope. [rquickjs](https://github.com/DelSkayn/rquickjs) · [Built-in tools](../reference/BuiltInTools.md) · [Code Mode contract and example](../develop/ToolExecutor.md#read-only-code-mode)

## Stop and cleanup

If Stop arrives during an admitted filesystem call, the loop waits for its actual outcome. During a shell call, the Environment Interface kills the sandbox and returns the output so far with the status `Stopped`, because a command may run for up to 30 minutes. During `execute_code`, the host interrupts the script even while it is computing, waits for any admitted read, then returns a cancelled outer result. During an MCP call, the handler sends the server a cancel notification and returns a cancelled result without waiting for a reply. [MCP Client](MCP.md#stop-timeouts-and-failures) The loop records that outcome and cancellations for the remaining unstarted model calls, then ends the turn as `Stopped`. [Agent State](AgentState.md) · [Rust implementation guide](../develop/ToolExecutor.md) · [Architecture overview](../README.md)
