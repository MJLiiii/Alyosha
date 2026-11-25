# Implement the MCP Client

Start the configured servers at session startup, register their tools, and forward calls. Use the official Rust SDK, `rmcp`, with its client role and child-process transport. [MCP design](../design/MCP.md) · [rmcp](https://github.com/modelcontextprotocol/rust-sdk) · [Architecture overview](../README.md)

## V1 configuration

```text
McpServerConfig {
    name: String               # the key under [mcp.servers]
    command: String
    args: Vec<String>          # default empty
    env: Map<String, String>   # default empty
    tools: Option<Vec<String>> # default: every tool
    ask: Vec<String>           # default empty; tools that need the owner's approval
}
```

A server name matches `[a-z0-9][a-z0-9-]{0,31}`. An invalid entry is a configuration error, and Alyosha refuses to start, as for any mistake in the file. A server that is valid but fails to start is only a warning. The configuration file holds tokens in `env`, so it must be readable only by you. [Configuration](../reference/Configuration.md#mcpserversname)

## Start servers and register tools

```text
McpManager
    start(configs: &[McpServerConfig], workspace: &Path) -> (McpManager, Vec<McpWarning>)
    tools(&self) -> Vec<RegisteredTool>
    shutdown(self)
```

Start all servers concurrently, each with a 30-second limit covering launch, the `initialize` handshake, and `tools/list` (following `nextCursor` until done). Launch each with the workspace as its working directory and a cleared environment holding the host `PATH` and `HOME`, `XDG_CONFIG_HOME` and `XDG_DATA_HOME` when they are set, plus its `env`. Declare no client capabilities. Send stdin and stdout to the transport. Drain stderr into the program's log file, one line per entry, prefixed with the server's name and cut to 1 KiB. [Logging](../reference/Logging.md#what-is-logged) A server that misses the limit or fails the handshake is killed and reported as a warning. [MCP lifecycle](https://modelcontextprotocol.io/specification/2025-06-18/basic/lifecycle)

For each listed tool, skipping any not in `tools`:

- Build the model-facing name `mcp__<server>__<tool>`, replacing each character of the tool name outside `[A-Za-z0-9_-]` with `_`. Skip the tool with a warning if the result exceeds 64 bytes or collides with a name already built. Keep the original tool name for calls.
- Use the server's `description`, or its `title` if the description is missing, and its `inputSchema` unchanged.
- Register it through the normal `ToolRegistry::register`. A registration error becomes a warning for that tool. [Tool Registry](ToolRegistry.md#v1-interface)

Register MCP tools after the built-ins and before constructing the executor. Show warnings in the status area as **MCP: N problems at startup**, naming the log file that holds the details. [Logging](../reference/Logging.md) Call `shutdown` when the session ends. It closes each server's stdin, waits up to 5 seconds, then kills the process. [Startup registration](ToolRegistry.md#register-built-in-tools-at-startup)

## Handle one call

The handler sends `tools/call` with the original tool name and the validated arguments. Before sending, emit a start notice, **MCP: server.tool**, for the status area. Convert the reply as follows. [MCP tools](https://modelcontextprotocol.io/specification/2025-06-18/server/tools)

| Reply | Tool outcome |
| --- | --- |
| `text` content | The text, with blocks joined by newlines |
| `image` or `audio` content | `[image omitted: <mimeType>]` or `[audio omitted: <mimeType>]` |
| `resource_link` | `[resource: <uri>]` |
| Embedded `resource` | Its `text` if present, otherwise `[resource omitted: <uri>]` |
| No content, only `structuredContent` | The structured content as compact JSON |
| `isError: true` | A tool error carrying the converted text |
| JSON-RPC error or a closed transport | A tool error naming the server |

The Agent Loop applies the usual result limits to the output, including a tool error's text. The preview is the start of the text. When the preview omits text, the full converted text goes to the session log store and the result carries `log_path` in its metadata, as for `run_command`. If the store's budget is used up or saving fails, keep the preview and record that the rest is unavailable. [Tool result limits](ContextManager.md#v1-tool-result-limits) · [Session logs](EnvironmentInterface.md#session-logs) · [`run_command` contract](../reference/BuiltInTools.md#run_command)

**Proposed interface addition.** `ToolContext` carries a way to emit a status notice when a call starts, in addition to the `notice` on `ToolOutput` that memory writes use. The session driver shows it outside history. [Memory notices](Memory.md#memory-tool) · [Agent Loop tool contract](AgentLoop.md#v1-tool-contract)

## Cancellation and deadlines

Race the reply against the call's cancellation token and a 120-second deadline. When either fires first, send `notifications/cancelled` with the request ID and return at once. Return a cancellation for Stop and a tool error saying the call timed out for the deadline. Drop any later reply to that request. [MCP cancellation](https://modelcontextprotocol.io/specification/2025-06-18/basic/utilities/cancellation) · [Stop and cleanup](../design/ToolExecutor.md#stop-and-cleanup)

If a server's process exits, mark it dead. Its pending and later calls return a tool error until the session ends.

**Scope.** MCP tools are available to direct model calls only. The Code Mode host never exposes them.

## Verification

Write a small fake stdio server for tests with scriptable tools: one that echoes, one that returns an image, one that sets `isError`, one that returns 50 KB of text, one that sleeps, and one that exits. These checks run under `cargo test` without a model account.

- Configure the fake server. Check that its tools appear as `mcp__fake__<tool>` with the advertised schemas, and that `tools = ["echo"]` registers only `echo`.
- Advertise a tool named `get.item` and another whose full name exceeds 64 bytes. Check that the first registers as `mcp__fake__get_item` and is called as `get.item`, and the second is skipped with a warning.
- Configure a missing command and a server that never answers `initialize`. Check that the session starts with the other servers' tools and shows the startup warning within 30 seconds.
- Call echo, image, and `isError` tools. Check the converted text, the placeholder, and the tool error, and that each result keeps its call ID.
- Call the 50 KB tool. Check that the recorded result is the start of the text with a `log_path`, that `read_file` on that path with the next offset returns the following text, that a Code Mode script cannot open it, and that the file is gone after the session ends. Fill the session's log budget first and repeat. Check that the preview survives and the rest is reported unavailable.
- Call with arguments that violate the schema. Check that the executor rejects them and the server receives nothing.
- Stop during the sleeping tool with a second call pending. Check that the server receives `notifications/cancelled`, the loop records a cancellation at once and a cancellation for the pending call, and a late reply is ignored. Repeat with a short deadline and check the timeout error.
- Kill the server mid-session. Check that its next call returns a tool error and the session continues.
- Have the fake server report its environment. Check that it sees `PATH`, `HOME`, the XDG folder variables that were set, and its configured `env` only, and that its working directory is the workspace.
- End the session. Check that every server process has exited.
- Check that `tools.mcp__fake__echo` is undefined in an `execute_code` script.

To try a real server, configure one you use and check a read-only call end to end with the configured model through GPROXY. [Gateway checks](LLMClientGateway.md#verify-with-the-upstream-binary-and-mock-providers)

[Architecture overview](../README.md)
