# Chat channels

A chat channel lets you talk to Alyosha from a messaging app when you are away from the TUI. Telegram and QQ are the first two. Alyosha has no code for any messaging app. It has one generic mode, `alyosha acp`, in which another program drives a session over standard input and output using the [Agent Client Protocol](https://agentclientprotocol.com/protocol/overview) (ACP). Each messaging app gets a separate **bridge** program that starts `alyosha acp`, passes your messages in, and sends the replies back. [Architecture overview](../README.md) · [What Alyosha still needs](todo.md)

```text
your phone ── Telegram ──► telegram bridge ── ACP over stdio ──► alyosha acp ──► agent core
           └── QQ ────────► qq bridge ──────── ACP over stdio ──► alyosha acp ──► agent core
```

The same rule covers tools: tools come in through MCP, chat apps come in through ACP. Both keep the core small. Adding a messaging app means installing or writing a bridge, not changing Alyosha. [MCP Client](MCP.md)

## Why ACP

ACP is an open standard for a program that drives an agent running as its child process. Editors use it to drive coding agents, and bridges for Telegram, Discord, and Slack already exist. Its methods match what Alyosha already does:

| Alyosha | ACP |
| --- | --- |
| A message from you | `session/prompt` |
| Stop | `session/cancel` |
| The reply, and the status area's lines | `session/update` notifications |
| A new session | `session/new` |
| `/compact`, `/job` | Slash commands, sent as the text of a `session/prompt` |
| An approval or an `ask_owner` question | `session/request_permission` |

A protocol of Alyosha's own, like the JSON-lines RPC mode of the pi agent, would be less work in the core, but every bridge would have to be written for it. With ACP, an existing bridge may need no code at all.

## What `alyosha acp` does

`alyosha acp` runs the same session driver and turn loop as the TUI. Only the front end differs. [Agent Loop](AgentLoop.md)

- **One session at a time.** `session/new` starts a session in the folder the bridge names, which must be under `~/Projects`. If a session is already open, it ends first, so a bridge's "new conversation" command gives the model a fresh start. Memory carries lasting facts across sessions as usual. [Memory](Memory.md)
- **A fresh session every night.** A chat session could otherwise last for weeks, and much of what a session holds is meant to last one session: its saved command and MCP output, its memory and skills indexes, its MCP servers and configuration, its dev shell, and its history. So at 04:00 in your timezone, if nothing is running, queued, or waiting for you and no background job is running, `alyosha acp` ends the session and starts a new one in the same folder. The bridge sees no change. If anything is busy at 04:00, that night is skipped, so a reset never cuts into a conversation or an overnight job. The next reply starts with *Started a fresh session at 04:00. Earlier conversation is gone; memory keeps lasting facts.* [Daily fresh session](../develop/Channels.md#daily-fresh-session)
- **Messages wait their turn.** Each `session/prompt` becomes one queued message. If a turn is running, the next prompt waits in the queue and is answered when its own turn ends, just as in the TUI.
- **You see what it does.** Each tool call is reported as an ACP tool call, titled like the TUI status area, for example **MCP: chromium.browser_click** or **Remembered: thesis-repo**. Status lines such as **Compacting context…** are reported the same way, as pseudo tool calls, so the bridge shows them without any code of its own. The reply arrives as message text when the turn ends. Startup warnings, such as an MCP server that failed to start, come before the first reply.
- **Stop works as in the TUI.** `session/cancel` signals the active turn, and the prompt ends as cancelled.
- **The configuration decides the tools.** Alyosha starts the MCP servers in its own configuration and ignores servers the bridge offers in `session/new`, so a bridge cannot add tools. It also ignores the editor features ACP offers, such as the client's file system and terminals, because the Environment Interface already handles files and shell commands. [Environment Interface](EnvironmentInterface.md)
- **Finished jobs reach the bridge.** ACP lets only the client start a turn, so when a background job ends, Alyosha sends the bridge a notification of its own, and the bridge sends the job's message back as a prompt. [Background jobs](Jobs.md#when-a-job-ends)
- **Text only.** Alyosha accepts text and links. Photos, files, and voice wait until tool results and prompts support images.
- **The model knows the app.** The system instruction gains one line naming the bridge, taken from ACP's `initialize`, such as "The owner is talking to you through alyosha-qq." The model uses it to send scheduled results to the app you wrote from. [Scheduled tasks](Schedule.md#creating-tasks)

## Trust

The bridge is a program you install and configure, so it is trusted like Alyosha's configuration. Whoever the bridge lets through can make Alyosha run shell commands, write files, and use websites with your saved sign-ins. A bridge must answer only your own account on the messaging app and ignore everyone else without replying. The bridge holds the bot token. Like every other secret, it stays out of the Nix store. [Saved sign-ins](Browser.md#signing-in-to-your-accounts)

Alyosha does not authenticate the bridge, because only programs that can already start `alyosha acp` as your user can talk to it.

## Telegram first

Telegram is the first messaging app, because its bot API is official, free, and needs no public address: the bridge asks Telegram for new messages, so nothing has to reach into the container. The owner and the server are in the Netherlands, where `api.telegram.org` is reachable directly, so the bridge needs no proxy.

Alyosha has its own bridge. Two existing bridges were checked in November 2026 and not chosen. Neither is in nixpkgs, and a bridge holds the bot token and decides who controls Alyosha, so reviewing one would take about as long as writing a small one that does only what this page asks.

| Bridge | Why not |
| --- | --- |
| [telegram-acp-bot](https://github.com/mgaitan/telegram-acp-bot) 0.3.0 | Python, Telegram only, one session per chat. Refuses to start without an allowlist of user IDs. Has `/new`, `/cancel`, a queue for messages sent mid-turn, and buttons for permission requests. Offers its own MCP server for attachments and scheduled messages, which Alyosha ignores. Early alpha with one main author. |
| [OpenACP](https://docs.openacp.ai/) | Node, Telegram plus Discord and Slack, one session per forum topic in a group. Allows every user when its allowlist is empty. Adds a local REST API, a web interface, tunnels, and downloads agents from a registry, all beyond what a bridge needs. |

## The Telegram bridge

`alyosha-telegram` is a separate Rust program in this repository, built in the same package as Alyosha. It shares no code with Alyosha's core and uses the same ACP crate, so both sides speak the same protocol version. It runs as an always-on service in the container, as your user, and starts `alyosha acp` as its child. It asks Telegram for new messages, so it opens no ports. [Implementation guide](../develop/TelegramBridge.md)

```text
containers.alyosha
└─ alyosha-telegram.service (always on, runs as you)
     ├─ long-polls api.telegram.org
     └─ alyosha acp (child process) ── same agent core
```

- **Only you.** It answers only your Telegram user ID, and only in a private chat with the bot. Messages from anyone else, and from any group, are dropped without a reply.
- **One long-lived session**, started fresh each night at 04:00 when idle. `/new` starts a fresh one at any time. `/cancel` stops the current turn. `/compact` and `/job` pass through to Alyosha. `/tasks` lists scheduled tasks. Telegram shows the five in its command menu.
- **Runs scheduled tasks.** Tasks for Telegram run as prompts in this session, marked **⏰ Scheduled**. [Scheduled tasks](Schedule.md#running-tasks) Finished background jobs come in the same way, marked **⚙ Job finished**. [Background jobs](Jobs.md)
- **You see it working.** While a turn runs, the chat shows Telegram's typing indicator and one **Working…** message listing tool calls as they happen, such as `MCP: chromium.browser_click`.
- **Plain-text replies.** The reply arrives as plain text, split into several messages when it is longer than Telegram allows. A failed turn shows its error, and a stopped turn says so.
- **Text only.** A photo, file, or voice message gets a short reply saying only text works for now.
- **Approvals as buttons.** An approval or a question from Alyosha arrives as a message with one button per answer. [Approvals](Approvals.md)
- **Crashes start fresh.** If `alyosha acp` exits, the bridge tells you and exits, and systemd starts both again with a new session.

Your settings go in a `[telegram]` section of `~/.config/alyosha/config.toml`: your Telegram user ID and the workspace folder. The bot token goes in the same file, which must be readable only by you. [Configuration](../reference/Configuration.md#telegram)

Both the bridge's `alyosha acp` and a TUI session use the same GPROXY service, so you can use the TUI while the bridge runs. [Gateway lifetime](LLMClientGateway.md#provider-accounts-and-gateway-lifetime)

## The QQ bridge

`alyosha-qq` works like the Telegram bridge: a separate Rust program, an always-on service in the container, and its own `alyosha acp` child. It uses QQ's official bot API from the [QQ open platform](https://bot.q.qq.com/wiki), not an unofficial client, so it cannot get a QQ account banned. It connects to QQ over a WebSocket, which needs no public address. [Implementation guide](../develop/QQBridge.md)

The official API limits replies, and that shapes the bridge:

- **Four replies per message.** The bot can answer each of your messages at most four times, within 60 minutes. The bridge sends no progress messages, only the reply. A reply longer than four messages is cut off with a note, and you can ask Alyosha to continue.
- **No editing.** QQ bots cannot edit sent messages, so there is no **Working…** message. The bridge shows QQ's typing indicator instead.
- **Late replies.** If a turn ends after the 60 minutes, or the four replies are used up, the bridge tries an ordinary message, which QQ allows only within rate limits and with permission. If that fails, the bridge keeps the reply and sends it first when you next write.
- **Messages QQ sends first are limited.** Scheduled tasks for QQ still run, but their results go out as normal or recall messages, which QQ may refuse. Undelivered results wait until you next write. Telegram has no such limits. [Delivering on QQ](Schedule.md#delivering-on-qq)

Approvals and questions arrive as text, and you answer with `/yes`, `/no`, or the number of a choice, because buttons need extra permission from QQ. The request uses one of the four replies, and your answer gives the turn a fresh set of four. [Approvals](Approvals.md)

Otherwise it behaves like the Telegram bridge. It answers only your account, identified by the `openid` QQ assigns you for this bot, and drops everything else without replying. It runs one long-lived session with `/new`, `/cancel`, `/compact`, `/job`, and `/tasks`, runs QQ's scheduled tasks, accepts text only, and starts fresh after a crash. Its settings go in a `[qq]` section of `config.toml`, together with the bot's secret.

A bot registered by an individual can be used privately by its creator and by test users added in the bot's settings, without Tencent's review. That covers personal use.

## Two bridges at once

Each bridge runs its own `alyosha acp` session. The two share GPROXY and the memory store, but each has its own conversation and starts its own MCP servers, including its own Chromium. Give each bridge its own workspace folder, so the two sessions never edit the same files.

The two bridges share their ACP side in a small library crate that only bridges use: starting `alyosha acp`, opening sessions, sending prompts, and collecting each prompt's updates and reply. Each bridge adds only its messaging app's side. [Package layout](../develop/TelegramBridge.md#package-layout)

## Deferred

- Photos, files, and voice messages.
- Resuming a session after a restart with `session/load`, which needs saved history.
- Carrying the previous session's summary into the nightly fresh session.
- Choosing the time of the nightly fresh session.
- Several sessions at once, such as one per chat thread.
- Turning the TUI into an ACP client, so that it is a bridge too.

[Chat channels implementation guide](../develop/Channels.md)
