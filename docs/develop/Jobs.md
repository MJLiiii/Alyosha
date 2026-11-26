# Implement background jobs

Add a per-session `JobManager`, a `background` input on `run_command`, the `job` tool, and a way for a finished job to start a turn through the bridge. Build the job manager with the shell at step 3 and the ACP side with the bridges at step 7. [Background jobs design](../design/Jobs.md) · [Architecture overview](../README.md)

## Job manager

```text
JobManager                      # one per session, carried in ToolContext
    start(command: &str, timeout: Duration) -> Result<JobInfo, ToolError>
    list(&self) -> Vec<JobInfo>
    status(&self, id: &JobId) -> Result<JobInfo, ToolError>
    stop(&self, id: &JobId) -> Result<JobInfo, ToolError>
    shutdown(self)              # kill every running job; called at session end

JobInfo {
    id: JobId                   # "j1", "j2", … in start order, unique per session
    command: String
    started: DateTime
    state: Running | Finished(ShellStatus(code) | TimedOut | Stopped)
    runtime: Duration
    tail: String                # last 2 KiB of output, on a character boundary
    log_path: String
    output_dropped: bool        # earlier output was dropped when the log budget ran out
}
```

`start` refuses with a tool error when two jobs are running. It launches the sandbox with the normal Bubblewrap launch contract, but under the job's own cancellation token, which lives for the session rather than the turn, so Stop does not reach it. The deadline works as for a command: on expiry, kill the whole sandbox and finish as `TimedOut`. `stop` kills it the same way and finishes as `Stopped`. `shutdown` kills every running job and waits up to 5 seconds for each launcher to exit. [Bubblewrap launch contract](EnvironmentInterface.md#bubblewrap-launch-contract) · [Deadline and result](EnvironmentInterface.md#deadline-and-result)

Stream the job's output to a file in the session log store from the start, and issue its `log_path` at once. Reserve log budget before each write, as for a command capture. When a reservation fails, stop writing to the file, keep the last 64 KiB in memory, and set `output_dropped`. `read_file` on the `log_path` then returns the saved start of the output, and `status` returns the latest tail. [Session logs](EnvironmentInterface.md#session-logs)

## `run_command` with `background`

| Input | Without `background` | With `background: true` |
| --- | --- | --- |
| `timeout_seconds` | 1 to 1800, default 120 | 1 to 86400, default 14400 |
| Result | `status`, `output`, `log_path?` when the command ends | `job_id`, `log_path`, and `status: "Started"` at once |

The schema allows `timeout_seconds` up to 86400. The handler returns a tool error for a value above 1800 without `background`. Emit the notice **Job j1 started: <command>** on start, cutting the command to 80 characters. [Built-in tool contracts](../reference/BuiltInTools.md#run_command)

## `job` tool

| `command` | Other inputs | Successful output |
| --- | --- | --- |
| `list` | none | One line per job of this session: ID, state, run time, and the command cut to 80 characters |
| `status` | `id` | Every `JobInfo` field, with the tail last |
| `stop` | `id` | The final `JobInfo`. Stopping a finished job returns its info unchanged. |

An unknown ID returns a tool error. The tool is available to direct model calls only, and the Code Mode host never exposes it. [Tool permissions](../reference/ToolPermissions.md)

## `/job` command

The front ends recognize a message that starts with `/job` followed by a space and a nonempty command. The rest of the trimmed message, line breaks included, is the command. A bare `/job` shows its usage: `/job <command>` starts a background job.

The session driver handles it at once, without the work slot or the queue, so it also works during a turn. It calls `JobManager::start` with the default four-hour deadline and returns a command status outside history: **Job j1 started: <command>**, or the tool error's text, such as the two-job limit. Record the job's origin as `Owner`. [Job manager](#job-manager)

- **ACP mode.** A prompt whose text starts with `/job ` runs the command and answers with the status as an `agent_message_chunk` and `end_turn`, without queuing. List `job` in `available_commands_update`, described as "Run a shell command as a background job", with the input hint `command`. [Prompts](Channels.md#prompts)
- **Bridges.** Telegram and QQ pass `/job …` through as prompt text, like `/compact`. Telegram adds `job` to its command menu.

## When a job finishes

When a job ends by exiting or by its deadline, the manager sends `JobFinished { id, text }` to the session driver. A job ended by `stop` or `shutdown` sends nothing. The text is:

```text
[Job j1 finished: status 101 after 58m · cargo test --workspace]
Last output:
<the tail, up to 2 KiB>
```

Use `TimedOut` in place of the status for a job that reached its deadline, and add `Earlier output was dropped.` when `output_dropped` is set. For a job the owner started with `/job`, the first line ends with ` · started by the owner with /job`.

- **Session driver.** The driver never queues it. It emits `SessionEvent::JobFinished { job_id, text }`, and nothing joins the queue until the bridge sends the text back.
- **ACP mode.** `alyosha acp` sends the extension notification `_alyosha/job_finished` with `sessionId`, `jobId`, and `text`. ACP reserves method names beginning with `_` for extensions, and a client that does not know one ignores it. [ACP extensibility](https://agentclientprotocol.com/protocol/extensibility) · [ACP mode](Channels.md)
- **Bridges.** `alyosha-bridge` turns the notification into an event. The bridge shows **⚙ Job finished:** in the chat, then sends the text as a `session/prompt`, like a scheduled task. QQ sends the reply under its scheduled-reply rules. [The runner](Schedule.md#the-runner) · [Delivering on QQ](Schedule.md#delivering-on-qq)

## Verification

These run under `cargo test` with the scripted model, except those that start a real sandbox, which run with the shell integration checks on NixOS. [Verify on the host](EnvironmentInterface.md#verify-on-the-host)

- Start `sleep 2; echo done` with `background: true`. Check that the call returns `Started` at once, that the turn ends, that a `JobFinished` event later carries the status and `done` with nothing queued, and that submitting its text, as a bridge does, runs a turn.
- Start three jobs. Check that the third returns a tool error.
- Pass `timeout_seconds: 3600` without `background`. Check for a tool error. Give a job a 2-second deadline with `sleep 60`. Check for a `TimedOut` message.
- Call `job stop` on a running job. Check that its sandbox and children are gone, its state is `Stopped`, and no message is queued.
- Press Stop during a turn while a job runs. Check that the turn ends `Stopped` and the job keeps running.
- Fill the session log budget while a job writes output. Check that `output_dropped` is set, `status` returns the latest tail, and `read_file` on the `log_path` returns the saved start.
- End the session with a job running. Check that its sandbox exits within 5 seconds and no message is sent.
- In ACP mode, finish a job. Check for one `_alyosha/job_finished` notification and that nothing enters the queue until a prompt arrives.
- Type `/job sleep 1; echo hi` during a scripted turn. Check that the job starts at once, that nothing enters history or the queue until it finishes, and that the finished message says the owner started it. Type a bare `/job`. Check for the usage text and no job.
- In ACP mode, send `/job true` as a prompt. Check for the started status and `end_turn` without a turn, and that `available_commands_update` lists `job`.

[Architecture overview](../README.md)
