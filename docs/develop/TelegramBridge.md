# Implement the Telegram bridge

`alyosha-telegram` relays one Telegram private chat to one `alyosha acp` child process. Build it after the ACP mode passes its checks. [Chat channels design](../design/Channels.md#the-telegram-bridge) · [ACP mode](Channels.md) · [Architecture overview](../README.md)

## Package layout

Turn the repository root into a Cargo workspace with these packages:

| Folder | Package | Holds |
| --- | --- | --- |
| root | `alyosha` | Alyosha itself, including `alyosha acp` |
| `bridge/` | `alyosha-bridge` | A library shared by the bridges: starting `alyosha acp`, `initialize` and `session/new`, sending prompts, attributing updates to the oldest open prompt, and collecting each prompt's tool calls, text, and outcome |
| `telegram/` | `alyosha-telegram` | The Telegram side |
| `qq/` | `alyosha-qq` | The QQ side. [QQ bridge](QQBridge.md) |
| `config/` | `alyosha-config` | The configuration schema and loader, used by every program. [Configuration](Configuration.md) |
| `schedule/` | `alyosha-schedule` | The scheduled-task file, used by the bridges' task runner, and the schedule MCP server. [Scheduled tasks](Schedule.md) |

No bridge depends on Alyosha's library. `alyosha-bridge` depends on the same pinned `agent-client-protocol` version as Alyosha, in its client role. `alyosha-telegram` adds [`teloxide`](https://docs.rs/teloxide) for the Bot API. Pin both. The flake builds every binary into the `alyosha` package. Build the Telegram bridge first and move its ACP side into `alyosha-bridge` when starting the QQ bridge.

## Configuration

```toml
[telegram]
owner_id = 123456789                            # your numeric Telegram user ID
workspace = "/home/mjli/Projects/alyosha-home"  # absolute, under ~/Projects
token = "..."                                   # from @BotFather
```

Load it with `alyosha-config`, which refuses a file that others can read and checks every value. Refuse to start, with a clear error, if the `[telegram]` section is missing. Never log the token. [Configuration](../reference/Configuration.md#telegram)

Create the bot with Telegram's @BotFather, which issues the token. Get your user ID from the bridge's own log: it logs the sender ID of every message it drops.

## Startup

1. Read the configuration and token.
2. Start `alyosha acp` from `PATH` with piped standard input and output. Send its standard error to the bridge's own standard error, which the journal keeps, for failures before its log file opens. Both programs write their own log files. [Logging](../reference/Logging.md)
3. Send `initialize` with protocol version `1`, `clientInfo.name` set to `alyosha-telegram`, and no client capabilities, then `session/new` with the configured workspace and an empty `mcpServers` list.
4. Call `setMyCommands` with `new`, `cancel`, `compact`, `job`, and `tasks`, each with a one-line description.
5. Start long polling with `getUpdates`, asking only for `message` and `callback_query` updates.
6. Start the task runner for the `telegram` channel. [The runner](Schedule.md#the-runner)
7. On each `_alyosha/job_finished` notification, send **⚙ Job finished:** to the chat, then send the notification's `text` as a `session/prompt`. [When a job finishes](Jobs.md#when-a-job-finishes)

Updates that arrived while the bridge was down are delivered on startup and processed in order. Telegram keeps them for 24 hours.

## Incoming messages

Drop an update, with a log line naming the sender ID and chat type, unless the chat is private and the sender's ID equals `owner_id`. Send nothing back.

For a message from the owner:

| Message | Action |
| --- | --- |
| `/start` | Reply with a short help text listing the commands. |
| `/new` | Send `session/new` with the configured workspace. Reply **New session started.** once it returns, which can take minutes while a project's dev shell builds. [Sessions](Channels.md#sessions) |
| `/cancel` | Send `session/cancel` for the current session. |
| `/compact`, `/job …` | Send it as the text of a `session/prompt`. [`/job` command](Jobs.md#job-command) |
| `/tasks` | List scheduled tasks from the file, without the model. [`/tasks`](Schedule.md#tasks) |
| Other text | Send it as one `text` block in a `session/prompt`. |
| Anything else | Reply **Only text messages work for now.** |

Send prompts at once, without waiting for earlier ones. Alyosha queues them. Track open prompts in order. ACP updates name only the session, so attribute each update to the oldest open prompt, which is the one whose turn is running.

## Showing progress and replies

While any prompt is open, send the `typing` chat action every 5 seconds.

For each prompt, on its first `tool_call` update, send a message reading **Working…** followed by the call's title. Append each later call's title as a new line, and mark failed calls with **(failed)**. Edit that message at most once every 3 seconds, sending the latest text, so the chat stays within Telegram's rate limits. Keep only the last 20 lines.

Collect the prompt's `agent_message_chunk` text in order. When the prompt's response arrives:

| Response | Message |
| --- | --- |
| `stopReason: "end_turn"` | The collected text. If it is empty, **Done.** |
| `stopReason: "cancelled"` | Any collected text, then **Stopped.** |
| Any other stop reason | Any collected text, then the stop reason. |
| JSON-RPC error | **Failed:** and the error message. |

Send replies without a parse mode, so Telegram shows them as plain text. Split text longer than 4000 characters at the last line break before the limit, or at the limit if a line is longer, and send the parts in order.

## Approvals and questions

On `session/request_permission`, send a message with the request's `title`, a blank line, and the `content` text, split like a reply. Attach an inline keyboard to the last part with one button per option, in order, each on its own row, with the option's `name` as its label and its `optionId` as callback data prefixed by a short request number, such as `r7:0`. [Approvals design](../design/Approvals.md) · [The ACP mode](Approvals.md#the-acp-mode)

- **A button press from the owner** answers the request with `selected` and that `optionId`. Call `answerCallbackQuery`, then edit the message to remove the keyboard and add **Answered: <option name>**.
- **A button press from anyone else**, or on a request already answered or expired, gets only `answerCallbackQuery` with no change.
- **A `tool_call_update` with `failed`** for the request's call ID means Alyosha stopped waiting. Remove the keyboard and add **Expired — not approved.**
- **`/cancel`** sends `session/cancel` first, then answers every open request with `cancelled`, as ACP requires. Alyosha then treats the request as stopped, not undelivered, and ends the turn. [The ACP mode](Approvals.md#the-acp-mode)
- **Other messages** are sent as prompts as usual and wait in Alyosha's queue. They do not answer the request.

If the message cannot be sent after retries, answer the request with `cancelled`, which Alyosha treats as undelivered.

## Failures and shutdown

- **`alyosha acp` exits.** Send **Alyosha stopped unexpectedly. Restarting with a new session.**, then exit with a nonzero status. systemd restarts the bridge, which starts a new child.
- **Telegram API errors.** On a `429` reply, wait the `retry_after` it gives. On other network errors, retry with backoff up to one minute. Never drop a reply because of a temporary error.
- **SIGTERM.** Stop polling, close the child's standard input, wait up to 10 seconds for it to exit, then kill it.

## Run it in the container

Add a service to the container's `config`, next to `environment.systemPackages`. [Host module](Deployment.md#host-module)

```nix
systemd.services.alyosha-telegram = {
  wantedBy = [ "multi-user.target" ];
  wants = [ "network-online.target" "gproxy.service" ];
  after = [ "network-online.target" "gproxy.service" ];
  # Reach the commands in environment.systemPackages, such as MCP servers.
  path = [ "/run/current-system/sw" ];
  serviceConfig = {
    User = user;
    ExecStart = "${alyosha}/bin/alyosha-telegram";
    Restart = "on-failure";
    RestartSec = 5;
  };
};
```

systemd sets `HOME` for `User=`, so Alyosha finds its folders as in a shell. Follow the bridge's startup and crashes with `journalctl -M alyosha -u alyosha-telegram -f` on the host, and its log files in `~/.local/share/alyosha/logs/`. [Logging](../reference/Logging.md)

The bridge's `alyosha acp` uses the same GPROXY service as the TUI, so both can run at once. [Run GPROXY as a service](LLMClientGateway.md#run-gproxy-as-a-service)

## Verification

These run under `cargo test` with a fake Bot API server, set through `teloxide`'s API URL, and a fake ACP agent that the test scripts.

- Start without the `[telegram]` section, and with `config.toml` readable by others. Check that each refuses with a clear error and the token never appears in output.
- Send a message from another user, and one from the owner in a group. Check that the agent receives nothing, nothing is sent back, and each sender ID is logged.
- Send an owner message. Check that the agent receives one `session/prompt` with the text and that the reply arrives as plain text.
- Script two tool calls, the second failing, then a reply. Check that one **Working…** message is created and edited, shows **(failed)** on the second line, and is edited no more than once every 3 seconds.
- Script a 9,000-character reply. Check that three messages arrive in order and each is under 4,000 characters.
- Send two messages without waiting. Check that both prompts are sent at once and that each reply goes out when its own response arrives.
- Send `/cancel` and `/new`. Check for `session/cancel` and `session/new`. Send `/cancel` while an approval is open. Check that `session/cancel` is sent before the request is answered with `cancelled`.
- Send a photo. Check for the text-only reply and no prompt.
- Make the fake Bot API reply `429` with `retry_after`. Check that the bridge waits and the reply still arrives.
- Make the agent exit. Check for the unexpected-stop message and a nonzero exit status.

With a real bot, which needs a token and network access:

- Run the bridge in the container with the real `alyosha acp`. Ask it to read a file in the workspace. Check that the tool call and the reply appear in Telegram.
- Message the bot from a second account. Check that nothing comes back.
- Restart the container. Check that the service comes back on its own.

[Architecture overview](../README.md)
