# Tool permissions

Selecting a workspace at session startup authorizes the following operations. The Tool Executor checks whether the caller may use the requested tool. The Environment Interface enforces workspace and process access when the handler opens a file or starts a shell. [Environment Interface](../EnvironmentInterface.md) · [Shared dispatch](../ToolExecutor.md#shared-dispatch)

| Tool | Allowed access |
| --- | --- |
| `read_file` | Read workspace files, session logs created for this session's shortened command and MCP output, and files under `/skills`. |
| `write_file` | Atomically create or replace a workspace file. |
| `edit_file` | Read and atomically replace an existing workspace file. |
| `run_command` | Run each command in a fresh Bubblewrap sandbox configured for the session's workspace, with `/skills` mounted read-only. |
| `job` | See and stop this session's background jobs, which run in the same sandbox as `run_command`. Direct model calls only. [Background jobs](../Jobs.md) |
| `memory` | Read and atomically replace files in the owner's memory store. Direct model calls only. [Memory store](#memory-store) |
| `skill` | Read the skills directory. Direct model calls only. [Skills directory](#skills-directory) |
| `ask_owner` | Ask the owner through the session's front end. No file, shell, or network access. Direct model calls only. [Approvals](../Approvals.md) |
| `mcp__<server>__<tool>` | Whatever the server itself can do. It runs outside the sandbox. Direct model calls only. [MCP tools](#mcp-tools) |
| `execute_code` | Run JavaScript in an isolated sandbox. Its only host operation is `tools.read_file`, limited to regular files inside the workspace. [Read-only Code Mode](../ToolExecutor.md#read-only-code-mode) |

## V1 interface

Enforce file permissions in `ToolPermissions::open_file` and `ToolPermissions::replace_file`. Enforce process isolation in `ToolPermissions::run_shell`. Each operation uses the scoped context supplied to the handler. [Handler interface and access modes](../EnvironmentInterface.md#handler-interface)

## Workspace file access

Resolve relative paths from the session working directory. Treat `/workspace` as the model-facing absolute path to the selected workspace, so paths printed by the shell can be passed to file tools. Anchor the file operation to that workspace and enforce containment when resolving and opening or creating the target. Reject symlinks that resolve outside the workspace. Have the handler use the authorized handle for I/O. For any path-based replacement, enforce containment when performing that operation too.

Use `Read` for `read_file` and `ReadWorkspace` to inspect the existing target for `edit_file`. Use `replace_file` to stage and commit both write operations inside the authorized target's parent directory. [Built-in tool contracts](BuiltInTools.md) · [Atomic file replacement](../EnvironmentInterface.md#atomic-file-replacement)

For Code Mode, expose only regular files inside the workspace and omit access to the log store. The same `read_file` handler serves both callers. For example, when it receives a session log path, a direct call can open that session's log. A call from a script gets a rejected read promise for that path. [Read-only Code Mode](../ToolExecutor.md#read-only-code-mode)

## Shell sandbox

Set up the Bubblewrap sandbox before spawning the shell. Give it a writable `/workspace`, a read-only `/skills` when the skills directory exists, the whole Nix store read-only, a fresh home directory, a writable shared shell cache, private scratch space, and internet access through `pasta` that cannot reach the container's loopback. Supply a minimal environment, plus the project's dev shell variables, and keep provider credentials, private session logs, and the Nix daemon socket out of it. Capture output and enforce the host-set deadline in the Environment Interface. [Bubblewrap launch contract](../EnvironmentInterface.md#bubblewrap-launch-contract) · [Command contract](BuiltInTools.md#run_command)

## Session logs

The session log store holds the full text of shortened command output and MCP results. It accounts for retained logs and temporary captures against a single 64 MiB limit. Only a direct `read_file` call from the owning session can open an issued `log_path`. Open it read-only through the log store. [Session log budget and lifecycle](../EnvironmentInterface.md#session-logs)

## Memory store

The memory store is in the session database and is reached only through the `memory` tool. Accept only valid memory names. The shell sandbox never mounts the database socket. Do not expose the tool to Code Mode. [Memory store contract](../Memory.md#v1-store-contract)

## Skills directory

The skills directory is outside the workspace and read-only everywhere. The `skill` tool opens files relative to the skills root without following symlinks out of it. A direct `read_file` resolves `/skills/...` paths against the root with `Read` access and permits regular files only. `ReadWorkspace` and `replace_file` never resolve `/skills`, so `edit_file` and `write_file` fail there. The shell sandbox binds the root read-only at `/skills`. Code Mode cannot read it. [Skills paths](../Skills.md#skills-paths)

## MCP tools

An MCP server is a program the owner configured, so it is trusted like the rest of the configuration. It runs as a child process outside the Bubblewrap sandbox, with the workspace as its working directory. Workspace containment and the shell sandbox do not limit what it does. Its environment holds only `PATH` and `HOME`, plus `ALYOSHA_HOME`, `XDG_CONFIG_HOME`, and `XDG_DATA_HOME` when they are set, and its configured `env`. Provider credentials and GPROXY settings stay out of it. The executor still checks scope and validates arguments against the advertised schema before forwarding a call. Calls need no approval unless the configuration marks the tool `ask`, in which case the executor runs it only after the owner approves. The status area names each call as it starts. [Approvals](../Approvals.md) Code Mode cannot call MCP tools. [MCP Client](../MCP.md#where-servers-run)

## Container limits

Alyosha runs in a NixOS container. Everything above applies inside it. The container adds an outer limit for the agent, GPROXY, and MCP servers: only `~/alyosha/config`, read-only, and `~/alyosha/state` are mounted from the host, and the host's `localhost` services are unreachable. Every workspace is a folder under the container's own `~/work`. [What it can see](../Deployment.md#what-it-can-see) MCP servers, including the browser server, and the shell sandbox have internet access through the host. The shell sandbox cannot reach the container's own loopback. Web pages the browser reads are untrusted content. Saved browser sign-ins live in `~/alyosha/config/browser/`, which only the browser server is configured to read. File tools and the shell sandbox cannot reach it, and the browser does not open `file://` addresses. The same holds for gogcli's Google sign-in in `~/alyosha/state/gog`. Nothing stops another MCP server from reading either folder, because every server runs as your user and is trusted like your configuration. Running each server in its own sandbox is deferred. [Deferred](../Deployment.md#deferred) [Email and calendar](../EmailCalendar.md#signing-in) [Saved sign-ins](../Browser.md#saved-sign-ins) · [Browser trust](../Browser.md#trust) · [Deployment](../Deployment.md) · [Container checks](../Deployment.md#verification)

## Verification

- Read a workspace file, then try a workspace symlink whose target is outside the root. Check that the first read succeeds and the second returns a tool error.
- Fail a staged write before rename. Check that an existing destination still has its original contents, a new destination has yet to appear, and the temporary file is removed.
- Have a shell command print the absolute path to a workspace file. Pass that path to a direct `read_file` call and check that it resolves to the same file.
- Read a log through its owning session. Check that a different session and a Code Mode script cannot open it.
- Exceed the session log limit. Check that the command status and preview survive, the full log is reported unavailable, and previously issued paths are still readable.
- Make shell sandbox setup fail. Check that no command starts and that the Agent Loop records the resulting tool error under the original call ID. [Agent Loop tool contract](../AgentLoop.md#v1-tool-contract)
- Read a file under `/skills` with a direct `read_file`, then try to write it, edit it, and read it from a Code Mode script. Check that only the read succeeds. [Skills checks](../Skills.md#verification)
- Start an MCP server that reports its environment. Check that it sees only `PATH`, `HOME`, the XDG folder variables that were set, and its configured `env`. [MCP checks](../MCP.md#verification)
- Time out a command with a background child. Check that the child is terminated and the result contains `TimedOut` and output captured before the deadline.
- Stop during `sleep 600` with `timeout_seconds: 900`. Check that the sandbox and its children are gone at once and the result is `Stopped` with the output so far. Check that `timeout_seconds: 1801` is rejected by the schema.

[Architecture overview](../README.md)
