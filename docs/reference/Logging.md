# Logging

Every Alyosha program writes a debug log to the container's systemd journal, which you read from the host. It records what happened, not what was said: events, names, IDs, durations, and errors, never the conversation. The conversation is kept only in the session database. [Durable sessions](../SessionStore.md) [Architecture overview](../README.md) · [Configuration](Configuration.md)

## Where the logs are

```bash
alyosha logs qq -f
```

`alyosha logs` runs `journalctl -M alyosha` for a service: `telegram`, `qq`, `gproxy`, or `db`, or every Alyosha program without one. Each program logs under an identifier of its own, so `journalctl -M alyosha -t alyosha-acp-guest` shows only guest sessions. [The host command](../Deployment.md#the-host-command)

| Program | Identifier | Unit |
| --- | --- | --- |
| `alyosha acp` | `alyosha-acp` | the bridge that started it |
| `alyosha acp --guest` | `alyosha-acp-guest` | the bridge that started it |
| `alyosha acp --group` | `alyosha-acp-group` | the bridge that started it |
| `alyosha-telegram` | `alyosha-telegram` | `alyosha-telegram.service` |
| `alyosha-qq` | `alyosha-qq` | `alyosha-qq.service` |
| `alyosha-schedule` | `alyosha-schedule` | the `alyosha acp` that started it |
| a command's unit | — | `alyosha-run-*@*.service`; only systemd's start and stop lines, never the command's output |

The system journal is readable by root and the `systemd-journal` group. The users `alyosha` and `alyosha-run` are in neither, so the file tools, shell commands, and MCP servers cannot read it. [Container limits](ToolPermissions.md#container-limits)

Each entry carries the level as its priority, and the session ID as a field when there is one.

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

## Retention

The container's journald keeps at most 500 MB and 14 days: `SystemMaxUse=500M` and `MaxRetentionSec=14day`, set by the container module. The journal rotates and drops the oldest entries itself.

## Before logging starts

An error that stops a program before its logging is set up goes to standard error, which systemd also writes to the service's journal. A bridge's `alyosha acp` child inherits the bridge's standard error. GPROXY logs to the journal as its own service. [Loading](Configuration.md#loading) · [Run GPROXY as a service](../LLMClientGateway.md#run-gproxy-as-a-service)

Startup warnings say where their details are, for example *details in the log: alyosha logs qq*.

## Implementation

Use [`tracing`](https://docs.rs/tracing) with [`tracing-journald`](https://docs.rs/tracing-journald), which sends each event to the journal's socket with the program's identifier and its level as the priority, when systemd started the program with its output in the journal, which it marks with `JOURNAL_STREAM`. A bridge's `alyosha acp` child inherits the variable. Otherwise, as in tests and development on the host, write lines to standard error instead, each starting with the time, the level, the identifier, the process ID, and the session ID when there is one. Put this in `alyosha-config`, which every program already uses, so each program sets up logging the same way before loading the configuration. Format secrets through the secret wrapper, which prints `"<secret>"`. [Configuration crate](Configuration.md#crate)

## Verification

These run under `cargo test` with logging to standard error, except the journal check, which runs in the container.

- Run a scripted turn with a message, a tool call, and a reply that each contain a marker string. Check that the marker appears nowhere in the log, and that the call's name, ID, and outcome do.
- Read a secret and log it at `debug`. Check that its value does not appear.
- Have a fake MCP server print a 5 KiB line to standard error. Check for one line prefixed with its name and cut to 1 KiB.
- In the container, start a guest session. Check that `journalctl -M alyosha -t alyosha-acp-guest` shows its startup at priority `info`, and that `alyosha-run` cannot run `journalctl`.

[Architecture overview](../README.md)
