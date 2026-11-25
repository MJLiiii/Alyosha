# Implement scheduled tasks

Build two pieces: the `alyosha-schedule` MCP server, and the task runner that both bridges use. They share one library for the task file. Build them after the Telegram bridge works. [Scheduled tasks design](../design/Schedule.md) · [Telegram bridge](TelegramBridge.md) · [Architecture overview](../README.md)

## Package layout

Add a `schedule/` package, `alyosha-schedule`, to the Cargo workspace. It holds a library for the task file and due times, and the MCP server binary. `alyosha-bridge` depends on the library for the runner. Alyosha's core depends on neither. The MCP server answers JSON-RPC over stdio directly, like Alyosha's MCP client. Use [`croner`](https://docs.rs/croner) for cron expressions and [`chrono-tz`](https://docs.rs/chrono-tz) for the timezone. [Package layout](TelegramBridge.md#package-layout)

## Configuration

```toml
timezone = "Europe/Amsterdam"   # top level, shared with the clock line

[mcp.servers.schedule]
command = "alyosha-schedule"
```

The tools appear as `mcp__schedule__add`, `mcp__schedule__list`, and `mcp__schedule__remove`. The server finds `config.toml` and the task file through `HOME` and the XDG variables, like Alyosha. The MCP Client passes those variables on, so the server, the bridges, and Alyosha all use the same folders. [Start servers](MCP.md#start-servers-and-register-tools) A channel is valid when its section, `[telegram]` or `[qq]`, is present. [MCP configuration](MCP.md#v1-configuration)

## The task file

`${XDG_DATA_HOME:-$HOME/.local/share}/alyosha/schedule.toml`, mode `600`, beside the memory store:

```toml
[[task]]
id = "t7k2p9"                          # six random lowercase letters and digits
channel = "telegram"
prompt = "Check my inbox and summarize anything important."
cron = "0 8 * * *"                     # repeating: five-field cron, in the configured timezone
created = 2026-11-25T10:02:00+01:00
last_run = 2026-11-26T08:00:00+01:00   # set by the runner; absent until the first run

[[task]]
id = "m3x8qa"
channel = "qq"
prompt = "Remind the owner to call the bank."
at = 2026-11-28T17:00:00+01:00         # one-time: a date and time with its offset
created = 2026-11-25T10:05:00+01:00
```

Each task has exactly one of `at` and `cron`. A missing file means no tasks.

Every writer, the MCP server and both runners, takes an exclusive `flock` on `schedule.lock` in the same folder, reads the file, changes it, writes a temporary file beside it, renames it into place, and releases the lock. Readers take the same lock. If the file does not parse, writers refuse to change it, the server's tools return the parse error, and the runners log it and run nothing until it is fixed, so a hand edit with a mistake loses no tasks.

## The MCP server

| Tool | Arguments | Result |
| --- | --- | --- |
| `add` | `channel` (`"telegram"` or `"qq"`), `prompt` (1 to 2000 characters), and exactly one of `at` (RFC 3339 with offset) or `cron` (five fields) | The new task's ID, channel, and next run time in the configured timezone |
| `list` | none | Every task with its ID, channel, time or cron, next run, last run, and prompt |
| `remove` | `id` | Confirmation, or an error if the ID does not exist |

Reject an `add` whose channel is not configured, whose `at` is in the past, or whose cron does not parse or never fires. Return these as tool errors with the reason, so the model can correct the call. Describe each tool and argument in its schema, including that `channel` is the app the owner should receive the result in.

## The channel note in `alyosha acp`

When `initialize` carries `clientInfo`, `alyosha acp` appends one line to the session's system instruction: "The owner is talking to you through `<clientInfo.name>`." Each bridge sends its program name, `alyosha-telegram` or `alyosha-qq`. The line is fixed for the session like the rest of the instruction. Without `clientInfo`, there is no line. [Initialization](Channels.md#initialization) · [Default instruction](../design/LLMClientGateway.md#default-instruction-for-normal-requests)

## The runner

The runner lives in `alyosha-bridge`. Each bridge starts it with its channel name. Once a minute, just after the minute turns, it takes the lock, reads the file, and checks its own channel's tasks against the current time in the configured timezone:

| Task | Due when | What happens |
| --- | --- | --- |
| One-time | `at` is at or before now | Run it, marked **late** if `at` is more than 2 minutes ago. Remove it from the file. |
| Repeating | The latest cron time at or before now is after `last_run`, or after `created` if it has never run | If that time is within the last 2 minutes and the task's previous run has finished, run it. Otherwise skip it. Set `last_run` to that time either way. |

To run a task, the bridge first shows **⏰ Scheduled:** and the prompt in the chat, plus **(late)** when late, then sends a `session/prompt` whose text is:

```text
[Scheduled task t7k2p9, due 2026-11-26 08:00]
Check my inbox and summarize anything important.
```

The prompt joins the session's queue like any message, and its reply goes out as usual. The runner keeps the task's ID with the open prompt, so a repeating task is skipped while its last run is still open.

## Delivering on QQ

A scheduled prompt has no QQ message ID. Send its reply parts as active messages: the send request without `msg_id`. If that fails with `40034100` or `40034105`, send one part with `is_wakeup: true` and the first part of the reply, cut to fit with **(more when you next write)**. If that fails too, for example with `40034122`, keep the whole reply as the QQ bridge keeps any undelivered reply, and send it first after your next message. Log each failure with its code. [Sending replies](QQBridge.md#sending-replies)

## `/tasks`

Both bridges handle `/tasks` themselves. Read the file under the lock and reply with one line per task, sorted by next run: ID, channel, next run in the configured timezone, **repeats** for cron tasks, and the prompt cut to 80 characters. With no tasks, reply **No scheduled tasks.** Add `tasks` to the Telegram command menu. [Telegram commands](TelegramBridge.md#incoming-messages)

## Verification

These run under `cargo test`, with a fake clock for the runner.

- Call `add` with an unconfigured channel, a past `at`, an invalid cron, both `at` and `cron`, and neither. Check that each returns a tool error naming the problem and the file is unchanged.
- Add a one-time and a repeating task, list them, remove one. Check the file after each step and that `list` shows next run times in the configured timezone.
- Run two writers at once, the server adding while a runner updates `last_run`. Check that both changes survive.
- Break the file's syntax by hand. Check that `add` refuses, the runner runs nothing and logs the error, and fixing the file restores both.
- Advance the clock past a one-time task. Check that one prompt with the scheduled prefix is sent, the chat shows **⏰ Scheduled:**, and the task leaves the file.
- Start the runner 3 hours after a one-time task's time. Check that it runs once, marked **late**.
- Start the runner 3 hours after a daily task's time. Check that it does not run and `last_run` moves to the missed time. Advance to the next day and check that it runs.
- Keep a repeating task's prompt open past its next time. Check that the next run is skipped.
- Give the Telegram and QQ runners one task each. Check that each runs only its own.
- Script a QQ scheduled reply where active sends fail with `40034105`. Check that one `is_wakeup` message is sent. Make that fail too and check that the reply is sent first after the next owner message.
- Send `/tasks` with and without tasks. Check the reply and that no prompt reaches the agent.
- Start `alyosha acp` with `clientInfo.name = "alyosha-qq"`. Check that the system instruction ends with the channel line, and that it is absent without `clientInfo`.

With real bots:

- In Telegram, ask "remind me in two minutes to stretch." Check that the reminder arrives marked **⏰ Scheduled**.
- Ask the same in QQ. Check that it arrives in QQ, and record whether it came as a normal message or a recall message.

[Architecture overview](../README.md)
