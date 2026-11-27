# MCP Client

Alyosha can use tools from Model Context Protocol (MCP) servers that you configure, such as a GitHub or calendar server. Each server's tools join the Tool Registry at session start and work like built-in tools from the model's point of view. [Tool Registry](ToolRegistry.md) · [MCP specification](https://modelcontextprotocol.io/specification) · [Architecture overview](README.md)

## Configuring a server

You list servers in the settings. Each entry gives the command that starts the server, its arguments, the environment variables it needs, and an optional allowlist of tools to expose. A variable whose value is a secret names the secret instead, under `secret_env`, because the settings are world-readable. [Secrets](reference/Configuration.md#secrets)

```toml
[mcp.servers.github]
command = "github-mcp-server"
args = ["stdio"]
secret_env = { GITHUB_PERSONAL_ACCESS_TOKEN = "github-token" }
tools = ["search_issues", "get_issue"]
```

The browser is built this way: one browser server and a skill with the rules for using it. [Browser](Browser.md)

V1 supports servers that talk over standard input and output. Remote servers over HTTP, and the OAuth sign-in they often need, are deferred. [Configuration](reference/Configuration.md#mcpservers)

## Session start

Alyosha starts every configured server when the session starts, completes the MCP handshake, and asks each server for its tools. Each tool is registered as `mcp__<server>__<tool>`, for example `mcp__github__search_issues`. If `tools` is set, only those tools are registered. After startup the catalog stays fixed. If a server's tool list changes, the change applies in the next session.

A server that fails to start, or a tool whose name or schema the registry rejects, does not stop the session. The chat shows a warning and the session continues without it.

## Where servers run

A server is a program you chose to configure, so Alyosha trusts it like the rest of your configuration. It runs as an ordinary child process of the agent, as the user `alyosha`, outside the command units, with the workspace as its working directory. It receives `PATH` and `HOME`, plus Alyosha's path variables, `XDG_CONFIG_HOME`, and `XDG_DATA_HOME` when they are set, the variables listed in its `env`, and the secrets named in its `secret_env`, and nothing else from Alyosha's environment. The path variables let a server such as the schedule server find the same folders as Alyosha. [Folders](reference/Configuration.md#folders) Provider credentials and GPROXY's settings never reach it. The servers stop when the session ends. [Environment Interface](EnvironmentInterface.md)

This means an MCP server can reach the network and files outside the workspace, depending on what it does. The workspace permissions and the command units apply to built-in tools, not to MCP servers. [Tool permissions](reference/ToolPermissions.md#mcp-tools)

When Alyosha runs in its NixOS container, the container is the outer limit. A server sees only the folders the container mounts and cannot reach services on the host's `localhost`. Its command must be installed in the container's configuration. [Deployment](Deployment.md)

## One call

The model requests an MCP tool like any other tool. The Tool Executor looks it up, checks the caller's scope, and validates the arguments against the schema the server advertised. The handler forwards the call to the server and turns its reply into a tool result. Calls run without approval unless your configuration marks the tool `ask`. Then the Tool Executor shows you the exact call and runs it only if you approve. [Approvals](Approvals.md) The chat's progress message names each call as it starts, for example **MCP: github.search_issues**, so you can see what leaves the machine. [Shared dispatch](ToolExecutor.md#shared-dispatch)

Text in the reply becomes the tool output. Images and other non-text content appear as a one-line placeholder. A reply that the server marks as an error becomes a tool error the model can act on. The usual result limits apply. When the text is too long for them, such as a large page snapshot or a long email thread, the result keeps the start of the text, and Alyosha saves the whole text in the session's logs, as it does for long shell output. The result then carries a `log_path`, and the model reads the rest in chunks with `read_file`. [Large tool results](ContextManager.md#large-tool-results)

Only direct model calls can use MCP tools. Code Mode scripts cannot.

## Stop, timeouts, and failures

Each call has a 120-second deadline, the same as a shell command's default. On Stop or on the deadline, Alyosha tells the server the call is cancelled and records the outcome at once. The MCP specification lets a server ignore the cancellation, so Alyosha does not wait for a reply and discards any that arrives later. This differs from file calls, which run to their actual outcome, and from shell calls, whose sandbox Alyosha kills itself. [Stop and cleanup](ToolExecutor.md#stop-and-cleanup)

If a server exits during the session, its tools return a tool error until the session ends. V1 does not restart it.

## Deferred

- Remote servers over Streamable HTTP, and OAuth.
- Resources, prompts, sampling, elicitation, and roots.
- Picking up tool list changes during a session, and restarting a crashed server.
- Choosing approval from a tool's own hints, for example asking for every tool the server does not mark read-only. In v1 only the tools listed in `ask` wait for approval. [Approvals](Approvals.md)
- Running each server in its own sandbox, inside the container.
- Loading tool definitions on demand when many servers are configured.
- Calling MCP tools from Code Mode.
- Letting Code Mode read the saved text of a shortened result, for example to search a long page snapshot.

## Implementation

Start the configured servers at session startup, register their tools, and forward calls. The client is a small JSON-RPC 2.0 client of Alyosha's own over the servers' newline-delimited stdio, in `src/mcp.rs`. It needs only the handshake, paginated `tools/list`, `tools/call`, `notifications/cancelled`, and answering `ping`. `rmcp` was the first choice; its typed API follows a newer protocol revision and made exact cancellation and the documented shutdown order harder to control. [rmcp](https://github.com/modelcontextprotocol/rust-sdk) · [Architecture overview](README.md)

### V1 configuration

```text
McpServerConfig {
    name: String               # the key under [mcp.servers]
    command: String
    args: Vec<String>          # default empty
    env: Map<String, String>   # default empty
    secret_env: Map<String, String> # variable → secret name; default empty
    tools: Option<Vec<String>> # default: every tool
    ask: Vec<String>           # default empty; tools that need the owner's approval
}
```

A server name matches `[a-z0-9][a-z0-9-]{0,31}`. An invalid entry is a configuration error, and Alyosha refuses to start, as for any mistake in the file. A server that is valid but fails to start is only a warning, and so is one whose `secret_env` names a secret that is not set: it is not started, and the warning names the secret and `alyosha secret set`. [Configuration](reference/Configuration.md#mcpservers)

### Start servers and register tools

```text
McpManager
    start(configs: &[McpServerConfig], workspace: &Path) -> (McpManager, Vec<McpWarning>)
    tools(&self) -> Vec<RegisteredTool>
    shutdown(self)
```

Start all servers concurrently, each with a 30-second limit covering launch, the `initialize` handshake, and `tools/list` (following `nextCursor` until done). Launch each with the workspace as its working directory and a cleared environment holding the host `PATH` and `HOME`, plus Alyosha's path variables, `XDG_CONFIG_HOME`, and `XDG_DATA_HOME` when they are set, plus its `env`, plus each `secret_env` variable with the secret's contents. Declare no client capabilities. Send stdin and stdout to the transport. Drain stderr into the program's log, one line per entry, prefixed with the server's name and cut to 1 KiB. [Logging](reference/Logging.md#what-is-logged) A server that misses the limit or fails the handshake is killed and reported as a warning. [MCP lifecycle](https://modelcontextprotocol.io/specification/2025-06-18/basic/lifecycle)

For each listed tool, skipping any not in `tools`:

- Build the model-facing name `mcp__<server>__<tool>`, replacing each character of the tool name outside `[A-Za-z0-9_-]` with `_`. Skip the tool with a warning if the result exceeds 64 bytes or collides with a name already built. Keep the original tool name for calls.
- Use the server's `description`, or its `title` if the description is missing, and its `inputSchema` unchanged.
- Register it through the normal `ToolRegistry::register`. A registration error becomes a warning for that tool. [Tool Registry](ToolRegistry.md#v1-interface)

Register MCP tools after the built-ins and before constructing the executor. Show warnings in the status area as **MCP: N problems at startup**, naming where the details are: `alyosha logs`. [Logging](reference/Logging.md) Call `shutdown` when the session ends. It closes each server's stdin, waits up to 5 seconds, then kills the process. [Startup registration](ToolRegistry.md#register-built-in-tools-at-startup)

### Handle one call

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

The Agent Loop applies the usual result limits to the output, including a tool error's text. The preview is the start of the text. When the preview omits text, the full converted text goes to the session log store and the result carries `log_path` in its metadata, as for `run_command`. If the store's budget is used up or saving fails, keep the preview and record that the rest is unavailable. [Tool result limits](ContextManager.md#v1-tool-result-limits) · [Session logs](EnvironmentInterface.md#session-logs) · [`run_command` contract](reference/BuiltInTools.md#run_command)

**Proposed interface addition.** `ToolContext` carries a way to emit a status notice when a call starts, in addition to the `notice` on `ToolOutput` that memory writes use. The session driver shows it outside history. [Memory notices](Memory.md#memory-tool) · [Agent Loop tool contract](AgentLoop.md#v1-tool-contract)

### Cancellation and deadlines

Race the reply against the call's cancellation token and a 120-second deadline. When either fires first, send `notifications/cancelled` with the request ID and return at once. Return a cancellation for Stop and a tool error saying the call timed out for the deadline. Drop any later reply to that request. [MCP cancellation](https://modelcontextprotocol.io/specification/2025-06-18/basic/utilities/cancellation) · [Stop and cleanup](ToolExecutor.md#stop-and-cleanup)

If a server's process exits, mark it dead. Its pending and later calls return a tool error until the session ends.

**Scope.** MCP tools are available to direct model calls only. The Code Mode host never exposes them.

### Verification

Write a small fake stdio server for tests with scriptable tools: one that echoes, one that returns an image, one that sets `isError`, one that returns 50 KB of text, one that sleeps, and one that exits. These checks run under `cargo test` without a model account.

- Configure the fake server. Check that its tools appear as `mcp__fake__<tool>` with the advertised schemas, and that `tools = ["echo"]` registers only `echo`.
- Advertise a tool named `get.item` and another whose full name exceeds 64 bytes. Check that the first registers as `mcp__fake__get_item` and is called as `get.item`, and the second is skipped with a warning.
- Configure a missing command and a server that never answers `initialize`. Check that the session starts with the other servers' tools and shows the startup warning within 30 seconds.
- Call echo, image, and `isError` tools. Check the converted text, the placeholder, and the tool error, and that each result keeps its call ID.
- Call the 50 KB tool. Check that the recorded result is the start of the text with a `log_path`, that `read_file` on that path with the next offset returns the following text, that a Code Mode script cannot open it, and that the file is gone after the session ends. Fill the session's log budget first and repeat. Check that the preview survives and the rest is reported unavailable.
- Call with arguments that violate the schema. Check that the executor rejects them and the server receives nothing.
- Stop during the sleeping tool with a second call pending. Check that the server receives `notifications/cancelled`, the loop records a cancellation at once and a cancellation for the pending call, and a late reply is ignored. Repeat with a short deadline and check the timeout error.
- Kill the server mid-session. Check that its next call returns a tool error and the session continues.
- Have the fake server report its environment. Check that it sees `PATH`, `HOME`, the path variables that were set, its configured `env`, and its `secret_env` values only, and that its working directory is the workspace. Name an unset secret in `secret_env`. Check that the server is skipped with a warning naming it.
- End the session. Check that every server process has exited.
- Check that `tools.mcp__fake__echo` is undefined in an `execute_code` script.

To try a real server, configure one you use and check a read-only call end to end with the configured model through GPROXY. [Gateway checks](LLMClientGateway.md#verify-with-the-upstream-binary-and-mock-providers)

[Architecture overview](README.md)
