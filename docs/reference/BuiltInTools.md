# Built-in tool contracts

The Tool Registry advertises nine built-in tools in v1, plus any tools from configured MCP servers. The Tool Executor passes validated arguments and a scoped `ToolContext` to each handler. File and shell handlers use the Environment Interface. Relative paths and shell commands start from the session working directory. Absolute paths under `/workspace` work in both the shell and the file tools. Paths under `/skills` are read-only in both. [MCP Client](../design/MCP.md) · [Tool Registry](../design/ToolRegistry.md) · [Environment Interface](../develop/EnvironmentInterface.md#handler-interface) · [Tool permissions](ToolPermissions.md)

## V1 inputs and outputs

`offset`, `max_bytes`, and `timeout_seconds` are integers, and `background` is a boolean. All other inputs are strings. A question mark marks an optional field.

| Tool | Purpose | Inputs | Successful output |
| --- | --- | --- | --- |
| `read_file` | Read a bounded chunk of UTF-8 text | `path`, `offset?`, `max_bytes?` | `text` and `next_offset?` |
| `write_file` | Create or overwrite a workspace file | `path`, `content` | `bytes_written` |
| `edit_file` | Replace one exact match in a workspace file | `path`, `old_text`, `new_text` | Success acknowledgment |
| `run_command` | Run a shell command in the workspace sandbox, or start it as a background job | `command`, `timeout_seconds?`, `background?` | `status`, `output`, and `log_path?`, or a job's `job_id`, `log_path`, and `Started` |
| `job` | List, check, or stop this session's background jobs | `command`, `id?` | Depends on `command` |
| `execute_code` | Read and filter files in a JavaScript sandbox | `code` | Text returned by the script |
| `memory` | List, read, write, or delete long-term memories | `command`, `name?`, `description?`, `content?` | Depends on `command` |
| `skill` | List skills or load one | `name?` | The skill list, or one skill's files and instructions |
| `ask_owner` | Ask the owner a question and wait for the answer | `question`, `choices?` | The owner's answer as text |

### `read_file`

Open the workspace file, current session log, or skill file under `/skills` with `open_file(path, Read)`. A session log is a `log_path` from `run_command` or from a shortened MCP result. [MCP results](../develop/MCP.md#handle-one-call) Code Mode may read regular workspace files only. Use a zero-based byte `offset`, defaulting to zero, on a UTF-8 character boundary. Accept a positive `max_bytes` and cap it at the per-result byte limit. Use that limit when `max_bytes` is omitted. Return only whole characters.

Include `next_offset` whenever more text remains. Add the requested offset to the UTF-8 byte length of the text actually returned, accounting for any shortening by the batch budget. Keep this offset in result metadata so it survives even when only metadata fits. [Tool result limits](../develop/ContextManager.md#v1-tool-result-limits)

For example, the text `café!` occupies six UTF-8 bytes. A read at offset 0 with `max_bytes: 4` returns `caf` and offset 3, where `é` begins. If the batch has room to record only `ca`, the model receives `next_offset: 2`. A script that makes the same inner read receives the full `caf` chunk and offset 3.

### `write_file`

Use `replace_file` with `CreateOrReplace` to atomically create a workspace file or replace its entire contents with `content`. The folder must already exist; the error says to create it with `mkdir -p`. Return the number of UTF-8 bytes written after commit. A staging or commit failure returns a tool error. [Atomic file replacement](../develop/EnvironmentInterface.md#atomic-file-replacement)

### `edit_file`

Read the existing workspace file with `open_file(path, ReadWorkspace)`. Require a nonempty `old_text` and exactly one occurrence in the file. Return a tool error for zero or multiple matches. An empty `new_text` deletes the match. Stage and commit the new contents using `replace_file` with `ReplaceExisting` and the original file snapshot. A detected change to the original returns a tool error before replacement. [Atomic file replacement](../develop/EnvironmentInterface.md#atomic-file-replacement)

### `run_command`

Directory listings, search, Git, and test runners all use this tool. Call `run_shell` to start a fresh Bubblewrap sandbox and run the command with a deadline that includes sandbox setup. `timeout_seconds` sets it, from 1 to 1800, and it defaults to 120. The tool's description tells the model to raise it for long builds and test suites. Return `ShellStatus(code)` in `status` on completion, including for a nonzero code. The code follows Bash's convention of `128 + signal` for signal termination. On deadline expiry, return `TimedOut` and the output captured so far. On Stop, kill the sandbox the same way and return `Stopped` with the output so far. A sandbox setup or launch failure returns a tool error. [Bubblewrap launch contract](../develop/EnvironmentInterface.md#bubblewrap-launch-contract) · [Tool permissions](ToolPermissions.md)

Keep `output` within the preview budget. When the recorded preview omits text, publish `log_path` only if the complete log was saved within the 64 MiB session budget. Keep `status` and any `log_path` in result metadata. The model can pass the path to a direct `read_file` call until the session ends, Alyosha restarts, or a compaction replaces the result. [Durable sessions](../design/SessionStore.md#without-the-nightly-fresh-start) If the log would exceed the budget or saving fails, return the command status and preview and record why the omitted output is unavailable in the result metadata. [Session log budget](../develop/EnvironmentInterface.md#session-logs) · [Tool result limits](../develop/ContextManager.md#v1-tool-result-limits)

With `background: true`, the command starts as a background job and the call returns its `job_id` and `log_path` at once. `timeout_seconds` then defaults to 14400 and may go up to 86400. [Background jobs](../develop/Jobs.md#run_command-with-background)

### `job`

`command` is `list`, `status`, or `stop`, and `status` and `stop` take the job's `id`. Only jobs of the current session are visible. When a job ends on its own, a message about it joins the session's queue and starts a turn. Direct model calls only. [Job tool contract](../develop/Jobs.md#job-tool)

### `execute_code`

The `code` string contains an async JavaScript function that takes `tools` and resolves to a string. It can call `tools.read_file` to read bounded chunks, then filter them in memory. Inner reads return to the script. The Agent Loop records the outer result under the model's original call ID using its usual preview budget. The Tool Executor guide specifies isolation and cancellation. [Code Mode contract and example](../develop/ToolExecutor.md#read-only-code-mode)

### `memory`

`command` is `list`, `read`, `write`, or `delete`. `write` creates or replaces a whole memory from `name`, `description`, and `content`, and Alyosha sets its date. Successful writes and deletes return a notice that the TUI shows in the status area. The handler uses the session's memory store, not the Environment Interface. [Memory tool contract](../develop/Memory.md#memory-tool)

### `user_memory`

Only in guest sessions, which answer group chats. `notes` replaces the complete notes of the person whose message the turn answers, at most 2 KiB. The tool has no input naming the person: Alyosha takes it from the message's sender. Successful writes return a notice. [People's notes](../develop/Memory.md#peoples-notes)

### `skill`

With no `name`, list every valid skill with its description. With a `name`, return a header with the path of its `SKILL.md` and its other files under `/skills/<name>/`, followed by the instructions without the front matter. An invalid or missing skill returns a tool error. The handler uses the session's skill store, not the Environment Interface. [Skill tool contract](../develop/Skills.md#skill-tool)

### `ask_owner`

The model asks a yes-or-no question, or offers up to six choices, before an action only it can recognize as real-world, or when a decision is the owner's. The owner answers through the session's front end: the TUI, or a chat bridge. The result says what the owner chose, or that they did not answer in time. Direct model calls only. [Approvals](../design/Approvals.md) · [Tool contract](../develop/Approvals.md#the-ask_owner-tool)

## Startup registration

The filesystem module supplies the three file handlers, the shell module supplies `run_command` and `job`, the Code Mode module supplies `execute_code`, the memory module supplies `memory`, the skills module supplies `skill`, and the approvals module supplies `ask_owner`. The Tool Registry pairs each handler with its model-facing definition at startup. [Registration example](../develop/ToolRegistry.md#register-built-in-tools-at-startup) · [Architecture overview](../README.md)
