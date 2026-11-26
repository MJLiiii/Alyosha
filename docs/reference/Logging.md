# Logging

Every Alyosha program writes a debug log, a plain text file you can read on the host while Alyosha runs in the container. It records what happened, not what was said: events, names, IDs, durations, and errors, never the conversation. The conversation is kept only in the session database. [Durable sessions](../SessionStore.md) [Architecture overview](../README.md) · [Configuration](Configuration.md)

## Where the logs are

The logs live in `~/alyosha/state/logs/`. The container mounts that folder from the host, so on the host you can follow a log with:

```bash
tail -f ~/alyosha/state/logs/alyosha-acp.2026-11-25.log
```

The folder has mode `0700` and each file `0600`. It is outside every workspace and is not mounted in the shell sandbox, so the file tools and shell commands cannot read it. MCP servers, which are trusted and run as your user, can. [Container limits](ToolPermissions.md#container-limits)

## One file per program per day

| Program | File |
| --- | --- |
| `alyosha acp` | `alyosha-acp.<date>.log` |
| `alyosha acp --guest` | `alyosha-acp-guest.<date>.log` |
| `alyosha acp --group` | `alyosha-acp-group.<date>.log` |
| `alyosha-telegram` | `alyosha-telegram.<date>.log` |
| `alyosha-qq` | `alyosha-qq.<date>.log` |
| `alyosha-schedule` | `alyosha-schedule.<date>.log` |

The date is the local date in the configured timezone. Each line starts with the time, the level, the process ID, and the session ID when there is one. Several processes can share a file, such as the two bridges' `alyosha acp` children. Each line is appended with one write to a file opened with `O_APPEND`, so lines never mix.

## What is logged

| Logged | Never logged |
| --- | --- |
| Startup: configuration path, workspace, model, MCP servers started, tools registered, and every startup warning in full | Your messages, the model's replies, and summaries |
| Model requests: route, model, duration, token usage, GPROXY's request ID, and errors | Tool arguments and tool output |
| Tool calls: name and call ID when each starts, then its duration and outcome kind | Approval request bodies and `ask_owner` questions |
| Waiting for the owner: when an approval or `ask_owner` question is sent, and how it ended (answered, expired, undeliverable, or cancelled) with how long it took | |
| Jobs, compaction, approvals answered or expired, sessions created, resumed, or refused as busy, session store errors, and bridge delivery failures with their error codes | Secrets: tokens, keys, passwords, and the configuration's secret fields |
| MCP servers' standard error, each line prefixed with the server's name and cut to 1 KiB | |

MCP servers' standard error is their own diagnostics, which Alyosha cannot check. A server that prints content there puts it in the log.

The bridges also log what they need for setup: the sender ID of every message they drop, which is how you find your Telegram user ID or QQ `openid`. [Telegram configuration](../TelegramBridge.md#configuration) · [QQ configuration](../QQBridge.md#configuration)

## Level

The default level is `info`. Set `ALYOSHA_LOG` in a program's environment to a [`tracing` filter](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html) such as `debug` or `alyosha::mcp=debug` for more detail. More detail never adds the content listed above.

## Retention and size

Every program deletes log files older than 14 days when it starts. Each file is capped at 50 MiB. At the cap, the writer adds one line, `Log limit reached; the rest of today is dropped`, and drops further lines until the date changes.

## Before logging starts

An error that stops a program before its log is open, such as a configuration error, goes to standard error. A bridge's standard error, and that of its `alyosha acp` child, goes to the systemd journal. GPROXY logs to the journal as its own service. [Loading](Configuration.md#loading) · [Run GPROXY as a service](../LLMClientGateway.md#run-gproxy-as-a-service)

Startup warnings name the file that holds their details, for example *details in ~/alyosha/state/logs/alyosha-acp.2026-11-25.log*.

## Implementation

Use [`tracing`](https://docs.rs/tracing) with a small writer of Alyosha's own for the daily file, the `O_APPEND` whole-line writes, and the size cap. Put it in `alyosha-config`, which every program already uses, so each program opens its log the same way right after loading the configuration. Format secrets through the configuration's secret wrapper, which prints `"<secret>"`. [Configuration crate](Configuration.md#crate)

## Verification

These run under `cargo test` against a temporary data folder with a fixed clock.

- Start a program. Check that the folder is `0700`, the file `0600`, and that the file name has the program and the local date.
- Write from two processes at once. Check that every line is whole.
- Run a scripted turn with a message, a tool call, and a reply that each contain a marker string. Check that the marker appears nowhere in the log, and that the call's name, ID, and outcome do.
- Load a configuration with secrets and log it at `debug`. Check that no secret appears.
- Have a fake MCP server print a 5 KiB line to standard error. Check for one line prefixed with its name and cut to 1 KiB.
- Pass the size cap. Check for one limit line and nothing after it until the date changes.
- Leave a 15-day-old and a 13-day-old file. Start a program. Check that only the older one is gone.

[Architecture overview](../README.md)
