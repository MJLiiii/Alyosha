# Background jobs

A background job is a shell command that keeps running while the conversation goes on: a full test suite, a long build, or a data job that takes hours. When it ends, Alyosha starts a turn by itself, so the model can look at the result and carry on without you. [Environment Interface](EnvironmentInterface.md) · [Built-in tool contracts](../reference/BuiltInTools.md) · [Architecture overview](../README.md)

An ordinary `run_command` call is limited to 30 minutes and holds the turn until it ends, so your other messages wait. A job returns at once and frees the session.

## Starting a job

You ask in your own words, and the model decides to use a job:

```text
› run the whole test suite in the background and fix what fails when it's done
  · Job j1 started: cargo test --workspace
Started the test suite as job j1. I'll look at the results when it finishes.

  (an hour later)
  · Job j1 finished: status 101 after 58m
› [Job j1 finished: status 101 after 58m · cargo test --workspace]
Two tests failed in parser.rs. …
```

The model calls `run_command` with `background: true`. The command runs in its own Bubblewrap sandbox under the same rules as any command: the workspace, the project's dev shell, the network, and the shell cache. The call returns a job ID such as `j1` straight away, and the turn goes on or ends as usual. [Shell commands](EnvironmentInterface.md#shell-commands)

You can also start a job yourself, without the model, by sending `/job` and the command in a chat:

```text
› /job cargo test --workspace
  · Job j1 started: cargo test --workspace
```

`/job` works even while a turn is running, because it does not wait for the session. It adds nothing to the conversation, so the model first hears of the job when it finishes. Its message then says you started it, and the model reports the result to you.

## Watching and stopping

The model uses the `job` tool to list jobs, check one's status and latest output, or stop it. It can read a job's full output with `read_file` through the job's `log_path`.

Stop and `/cancel` end the current turn, not jobs. To end a job, ask the model, for example "stop the test run," or end the session.

## When a job ends

When a job's command exits or reaches its time limit, Alyosha tells the bridge, and the bridge sends a message back as a prompt, so it joins the session's queue like one of yours. ACP lets only the bridge start a turn. [Chat channels](Channels.md#what-alyosha-acp-does) The message names the job, its status, its run time, and its command, followed by the last lines of output. When its turn comes, the model sees it and decides what to do next. A job you or the model stopped sends no message.

- **Telegram and QQ.** The chat shows **⚙ Job finished:**, then the reply, as for a scheduled task. In QQ the reply follows the delivery rules for scheduled results, because it answers no message of yours. [Delivering on QQ](Schedule.md#delivering-on-qq)

## Limits

- **Two jobs at once** per session. Starting a third returns a tool error.
- **Four hours by default**, set per job up to 24 hours. A job that reaches its limit is killed and reported as timed out.
- **Output** goes to the session logs and counts against their 64 MiB budget. Once the budget is used up, Alyosha keeps only the last 64 KiB of that job's output. [Large tool results](ContextManager.md#large-tool-results)

## Lifetime

Jobs belong to the process running their session and end with it. `/new` in a chat and a bridge restart or crash all kill the session's jobs. A resumed session does not bring them back. [Durable sessions](SessionStore.md#what-is-saved)

## Trust

A job runs in the same sandbox as any command, so it reaches nothing a command cannot. Its turn meets approvals like any other. The "job finished" line comes from Alyosha, but the output lines after it come from the command and are untrusted text, like a web page.

## Deferred

- Listing or stopping jobs from a chat without the model, such as `/jobs`.
- Choosing a job's time limit with `/job`. It always uses the four-hour default.
- Jobs that outlive their session.
- More than two jobs at once, or limits set in the configuration.

[Background jobs implementation guide](../develop/Jobs.md)
