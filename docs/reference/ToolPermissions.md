# Tool permissions

Selecting a workspace at session startup authorizes the following operations. The Tool Executor checks whether the caller may use the requested tool. The Environment Interface enforces workspace and process access when the handler opens a file or starts a shell. [Environment Interface](../EnvironmentInterface.md) · [Shared dispatch](../ToolExecutor.md#shared-dispatch)

| Tool | Allowed access |
| --- | --- |
| `read_file` | Read workspace files, session logs created for this session's shortened command and MCP output, and files under `/skills`. |
| `write_file` | Atomically create or replace a workspace file. |
| `edit_file` | Read and atomically replace an existing workspace file. |
| `run_command` | Run each command in a fresh command unit for the session's workspace, as `alyosha-run`, with `/skills` mounted read-only. |
| `job` | See and stop this session's background jobs, which run in command units like `run_command`. Direct model calls only. [Background jobs](../Jobs.md) |
| `memory` | Read and atomically replace files in the owner's memory store. Direct model calls only. [Memory store](#memory-store) |
| `skill` | Read the skills directory. Direct model calls only. [Skills directory](#skills-directory) |
| `ask_owner` | Ask the owner through the session's front end. No file, shell, or network access. Direct model calls only. [Approvals](../Approvals.md) |
| `mcp__<server>__<tool>` | Whatever the server itself can do. It runs as the agent's user, outside the command units. Direct model calls only. [MCP tools](#mcp-tools) |
| `execute_code` | Run JavaScript in an isolated sandbox. Its only host operation is `tools.read_file`, limited to regular files inside the workspace. [Read-only Code Mode](../ToolExecutor.md#read-only-code-mode) |

## V1 interface

Enforce file permissions in `ToolPermissions::open_file` and `ToolPermissions::replace_file`. Enforce process isolation in `ToolPermissions::run_shell`. Each operation uses the scoped context supplied to the handler. [Handler interface and access modes](../EnvironmentInterface.md#handler-interface)

## Workspace file access

Resolve relative paths from the session working directory. Treat `/workspace` as the model-facing absolute path to the selected workspace, so paths printed by the shell can be passed to file tools. Anchor the file operation to that workspace and enforce containment when resolving and opening or creating the target. Reject symlinks that resolve outside the workspace. Have the handler use the authorized handle for I/O. For any path-based replacement, enforce containment when performing that operation too.

Use `Read` for `read_file` and `ReadWorkspace` to inspect the existing target for `edit_file`. Use `replace_file` to stage and commit both write operations inside the authorized target's parent directory. [Built-in tool contracts](BuiltInTools.md) · [Atomic file replacement](../EnvironmentInterface.md#atomic-file-replacement)

For Code Mode, expose only regular files inside the workspace and omit access to the log store. The same `read_file` handler serves both callers. For example, when it receives a session log path, a direct call can open that session's log. A call from a script gets a rejected read promise for that path. [Read-only Code Mode](../ToolExecutor.md#read-only-code-mode)

## Shell sandbox

Run each command in a unit from the session's template, as `alyosha-run`. It gets a writable `/workspace`, a read-only `/skills`, the whole Nix store read-only, a fresh home directory, a writable shared shell cache, private scratch space, its own PID namespace, a memory limit, and internet access that cannot reach the container's loopback, the host, or the local network. Supply a minimal environment, plus the project's dev shell variables. Secrets, the database socket, session logs, other working folders, and the Nix daemon socket stay out of it. Capture output and enforce the deadline in the Environment Interface. [Command launch](../EnvironmentInterface.md#command-launch) · [Command units](../Deployment.md#command-units) · [Command contract](BuiltInTools.md#run_command)

## Session logs

The session log store holds the full text of shortened command output and MCP results. It accounts for retained logs and temporary captures against a single 64 MiB limit. Only a direct `read_file` call from the owning session can open an issued `log_path`. Open it read-only through the log store. [Session log budget and lifecycle](../EnvironmentInterface.md#session-logs)

## Memory store

The memory store is in the session database and is reached only through the `memory` tool. Accept only valid memory names. Command units cannot reach the database socket, and `alyosha-run` has no database role. Do not expose the tool to Code Mode. [Memory store contract](../Memory.md#v1-store-contract)

## Skills directory

The skills directory is outside the workspace and read-only everywhere. The `skill` tool opens files relative to the skills root without following symlinks out of it. A direct `read_file` resolves `/skills/...` paths against the root with `Read` access and permits regular files only. `ReadWorkspace` and `replace_file` never resolve `/skills`, so `edit_file` and `write_file` fail there. Command units bind the root read-only at `/skills`. Code Mode cannot read it. [Skills paths](../Skills.md#skills-paths)

## MCP tools

An MCP server is a program the owner configured, so it is trusted like the rest of the configuration. It runs as a child process of the agent, as `alyosha`, outside the command units, with the workspace as its working directory. Workspace containment and the command units do not limit what it does. Its environment holds only `PATH` and `HOME`, plus Alyosha's path variables, `XDG_CONFIG_HOME`, and `XDG_DATA_HOME` when they are set, its configured `env`, and its `secret_env` values. The gateway key and GPROXY settings stay out of it. [Folders](Configuration.md#folders) The executor still checks scope and validates arguments against the advertised schema before forwarding a call. Calls need no approval unless the configuration marks the tool `ask`, in which case the executor runs it only after the owner approves. The status area names each call as it starts. [Approvals](../Approvals.md) Code Mode cannot call MCP tools. [MCP Client](../MCP.md#where-servers-run)

## Container limits

Alyosha runs in a NixOS container. Everything above applies inside it. The container adds an outer limit for the agent, GPROXY, and MCP servers: nothing of the host is mounted except your skills, read-only, and the backup folder, and the container cannot reach the host or your local network. Every workspace is one of the container's working folders. [Where things live](../Deployment.md#where-things-live) · [Network](../Deployment.md#network)

Inside, each service runs as its own user. Commands run as `alyosha-run` and cannot read the secrets folder, GPROXY's files, the database, or other processes. The agent and its MCP servers run as `alyosha`. MCP servers, including the browser server, have internet access through the host. Web pages the browser reads are untrusted content. The secrets the agent's tools need, such as the browser's saved sign-ins and gogcli's keyring password, are readable by the `alyosha` group, so only the servers configured to use them do. The browser does not open `file://` addresses. Nothing stops another MCP server from reading them, because every server runs as `alyosha` and is trusted like your configuration. Running each server in its own sandbox is deferred. [Deferred](../Deployment.md#deferred) · [Email and calendar](../EmailCalendar.md#signing-in) · [Saved sign-ins](../Browser.md#saved-sign-ins) · [Browser trust](../Browser.md#trust) · [Container checks](../Deployment.md#verification)

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
