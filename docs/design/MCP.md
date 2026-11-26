# MCP Client

Alyosha can use tools from Model Context Protocol (MCP) servers that you configure, such as a GitHub or calendar server. Each server's tools join the Tool Registry at session start and work like built-in tools from the model's point of view. [Tool Registry](ToolRegistry.md) · [MCP specification](https://modelcontextprotocol.io/specification) · [Architecture overview](../README.md)

## Configuring a server

You list servers in the personal configuration file. Each entry gives the command that starts the server, its arguments, the environment variables it needs, and an optional allowlist of tools to expose.

```toml
[mcp.servers.github]
command = "github-mcp-server"
args = ["stdio"]
env = { GITHUB_PERSONAL_ACCESS_TOKEN = "..." }
tools = ["search_issues", "get_issue"]
```

The browser is built this way: one browser server and a skill with the rules for using it. [Browser](Browser.md)

V1 supports servers that talk over standard input and output. Remote servers over HTTP, and the OAuth sign-in they often need, are deferred. [Configuration](../reference/Configuration.md#mcpserversname)

## Session start

Alyosha starts every configured server when the session starts, completes the MCP handshake, and asks each server for its tools. Each tool is registered as `mcp__<server>__<tool>`, for example `mcp__github__search_issues`. If `tools` is set, only those tools are registered. After startup the catalog stays fixed. If a server's tool list changes, the change applies in the next session.

A server that fails to start, or a tool whose name or schema the registry rejects, does not stop the session. The chat shows a warning and the session continues without it.

## Where servers run

A server is a program you chose to configure, so Alyosha trusts it like the rest of your configuration. It runs as an ordinary child process outside the Bubblewrap sandbox, with the workspace as its working directory. It receives `PATH`, `HOME`, `XDG_CONFIG_HOME` and `XDG_DATA_HOME` when they are set, and the variables listed in its `env`, and nothing else from Alyosha's environment. The XDG variables let a server such as the schedule server find the same folders as Alyosha. Provider credentials and GPROXY's settings never reach it. The servers stop when the session ends. [Environment Interface](EnvironmentInterface.md)

This means an MCP server can reach the network and files outside the workspace, depending on what it does. The workspace permissions and the shell sandbox apply to built-in tools, not to MCP servers. [Tool permissions](../reference/ToolPermissions.md#mcp-tools)

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

[MCP implementation guide](../develop/MCP.md)
