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

- **One session at a time.** `session/new` starts a session in the folder the bridge names, any existing folder. If a session is already open, it ends first, so a bridge's "new conversation" command gives the model a fresh start. Memory carries lasting facts across sessions as usual. [Memory](Memory.md)
- **A fresh session every night.** A chat session could otherwise last for weeks, and much of what a session holds is meant to last one session: its saved command and MCP output, its memory and skills indexes, its MCP servers and configuration, its dev shell, and its history. So at 04:00 in your timezone, if nothing is running, queued, or waiting for you and no background job is running, `alyosha acp` ends the session and starts a new one in the same folder. The bridge sees no change. If anything is busy at 04:00, that night is skipped, so a reset never cuts into a conversation or an overnight job. The next reply starts with *Started a fresh session at 04:00. Earlier conversation is gone; memory keeps lasting facts.* [Daily fresh session](../develop/Channels.md#daily-fresh-session)
- **Messages wait their turn.** Each `session/prompt` becomes one queued message. If a turn is running, the next prompt waits in the queue and is answered when its own turn ends, just as in the TUI.
- **You see what it does.** Each tool call is reported as an ACP tool call, titled like the TUI status area, for example **MCP: chromium.browser_click** or **Remembered: thesis-repo**. Status lines such as **Compacting context…** are reported the same way, as pseudo tool calls, so the bridge shows them without any code of its own. The reply arrives as message text when the turn ends. Startup warnings, such as an MCP server that failed to start, come before the first reply.
- **Stop works as in the TUI.** `session/cancel` signals the active turn, and the prompt ends as cancelled.
- **The configuration decides the tools.** Alyosha starts the MCP servers in its own configuration and ignores servers the bridge offers in `session/new`, so a bridge cannot add tools. It also ignores the editor features ACP offers, such as the client's file system and terminals, because the Environment Interface already handles files and shell commands. [Environment Interface](EnvironmentInterface.md)
- **Finished jobs reach the bridge.** ACP lets only the client start a turn, so when a background job ends, Alyosha sends the bridge a notification of its own, and the bridge sends the job's message back as a prompt. [Background jobs](Jobs.md#when-a-job-ends)
- **Text only.** Alyosha accepts text and links. Photos, files, and voice wait until tool results and prompts support images.
- **The model knows the app.** The system instruction gains one line naming the bridge, taken from ACP's `initialize`, such as "The owner is talking to you through alyosha-qq." The model uses it to send scheduled results to the app you wrote from. [Scheduled tasks](Schedule.md#creating-tasks)

## Trust

The bridge is a program you install and configure, so it is trusted like Alyosha's configuration. Whoever the bridge lets into your session can make Alyosha run shell commands, write files, and use websites with your saved sign-ins. A bridge lets only your own account into your session, in a private chat, and ignores everyone else's private messages without replying. Other people reach Alyosha only by mentioning it in a group. In a group you list as trusted, everyone gets your full powers through that group's own session. In any other group, a separate guest session without your tools answers them. [Group chats](#group-chats) The bridge holds the bot token. Like every other secret, it stays out of the Nix store. [Saved sign-ins](Browser.md#signing-in-to-your-accounts)

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

- **Only you in private.** In a private chat it answers only your Telegram user ID. Private messages from anyone else are dropped without a reply. In groups it answers anyone who mentions it, through the group's own session. [Group chats](#group-chats)
- **One long-lived session**, started fresh each night at 04:00 when idle. `/new` starts a fresh one at any time. `/cancel` stops the current turn. `/compact` and `/job` pass through to Alyosha. `/tasks` lists scheduled tasks. Telegram shows the five in its command menu.
- **Runs scheduled tasks.** Tasks for Telegram run as prompts in this session, marked **⏰ Scheduled**. [Scheduled tasks](Schedule.md#running-tasks) Finished background jobs come in the same way, marked **⚙ Job finished**. [Background jobs](Jobs.md)
- **You see it working.** While a turn runs, the chat shows Telegram's typing indicator and one **Working…** message listing tool calls as they happen, such as `MCP: chromium.browser_click`.
- **Formatted replies.** The model writes Markdown, and the reply shows it formatted: bold, italics, code, code blocks, links, and quotes, with headings as bold lines and lists with bullets. If Telegram refuses the formatting, the same part goes again as plain text without the Markdown markers. Long replies are split into several messages, keeping code blocks whole in each. A failed turn shows its error, and a stopped turn says so.
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

Otherwise it behaves like the Telegram bridge. In private chats it answers only your account, identified by the `openid` QQ assigns you for this bot, and drops other private messages without replying. In groups it answers anyone who @-mentions it, through the group's own session. [Group chats](#group-chats) It runs one long-lived session with `/new`, `/cancel`, `/compact`, `/job`, and `/tasks`, runs QQ's scheduled tasks, accepts text only, and starts fresh after a crash. Replies use QQ's native Markdown when QQ allows the bot to send it. A bot QQ has not granted Markdown gets the same reply as plain text, with the markers removed. Its settings go in a `[qq]` section of `config.toml`, together with the bot's secret.

A bot registered by an individual can be used privately by its creator and by test users added in the bot's settings, without Tencent's review. That covers personal use.

## Two bridges at once

Each bridge runs its own `alyosha acp` session. The two share GPROXY and the memory store, but each has its own conversation and starts its own MCP servers, including its own Chromium. Give each bridge its own workspace folder, so the two sessions never edit the same files.

The two bridges share their ACP side in a small library crate that only bridges use: starting `alyosha acp`, opening sessions, sending prompts, and collecting each prompt's updates and reply. Each bridge adds only its messaging app's side. [Package layout](../develop/TelegramBridge.md#package-layout)

## Group chats

Anyone can add the bot to a group and @-mention it there. Alyosha replies to that message in the group. The bridges hear nothing else in a group: Telegram's privacy mode and QQ's group API pass a bot only the messages addressed to it.

```text
trusted group T ── @Alyosha ──► bridge ──► alyosha acp --group (group T's session, your tools)
group A ────────── @Alyosha ──► bridge ──► alyosha acp --guest (group A's session) ─┐
group B ────────── @Alyosha ──► bridge ──► alyosha acp --guest (group B's session) ─┼─► memory/users/<platform>-<user id>.md
your private chat ───────────► bridge ──► alyosha acp (your session, your tools) ──┘   (not shared with your own memory)
```

- **What counts as a mention.** In Telegram, `@` followed by the bot's username, or a reply to one of Alyosha's messages. In QQ, an @ of the bot, whether QQ sends the group only @s or, with its "receive all messages" mode, every message. A Telegram mention that replies to someone else's message brings the start of that message along, so "@Alyosha is this true?" works.
- **A session per group.** Each group gets its own child, started at the first mention. It keeps the group's conversation, so follow-up questions work, and starts fresh at 04:00 like your session. Groups never see each other's conversations or your private chat's.
- **Trusted groups get your powers.** You list trusted groups in the bridge's section of the configuration, by the group ID the bridge logs for every mention. Their session is a full session like yours: shell, files, your memory, skills, background jobs, every MCP server, and your browser sign-ins. Everyone in a trusted group, including people who join later, can use all of it, so list only groups where you would hand every member your computer. [Trusted groups](#trusted-groups)
- **Other groups get guests.** A group you have not listed gets a guest session. It has no shell, no files, no background jobs, no skills, no `ask_owner`, and not your memory. Its only MCP servers are the ones `[guest]` in the configuration lists, normally just web search. Search is read-only and needs no account, so a guest cannot reach your mail, your sign-ins, or the machine. A guest session that has been quiet for an hour is stopped, so a bot added to many groups does not keep a process for each. [Web search](WebSearch.md) · [Configuration](../reference/Configuration.md#guest)
- **Everyone is equal in a group, you included.** The bridge does not tell you apart from other members in a group. Your private chat stays the only place for commands and approvals.
- **A memory for each person, in guest sessions.** A guest session keeps one notes file per person, named by their platform and user ID, and shows it to the model with each of their messages. The model updates it with the `user_memory` tool, which writes only the file of the person whose message it is answering. Trusted groups use your memory instead. [People in group chats](Memory.md#people-in-group-chats)
- **Names are not proof.** Each message reaches the model with the sender's display name. Anyone can pick any display name, so a guest session's instruction tells the model that nobody in the group is its owner and that it acts for no one there.
- **Replies are formatted as in private.** The reply is rendered and split like a private reply. QQ allows five replies per group message within 60 minutes, and the bridge uses at most four, as for private messages. A group reply that fails is logged and dropped, never kept for later.
- **Nothing else in groups.** Commands, scheduled tasks, and the **Working…** message are for your private chat only. A mention with a photo or file and no text gets **Only text messages work for now.**
- **Cost.** Every group mention is a model request on your account. Guest groups have no allowlist and no rate limit in v1. If strangers' use becomes a problem, add a limit per group or per person.

A QQ user's ID is the `member_openid` QQ gives them, which may differ from group to group. If it does, a QQ user has a separate notes file in each group. The first check with a real QQ bot settles this. Telegram user IDs are the same everywhere.

### Trusted groups

A trusted group's session is `alyosha acp --group`: your normal session with a different channel line, which says that people in a group your owner trusts write through the bridge and that each message starts with the sender's name. It differs from your private session in these ways:

- **Its own workspace.** It works in `groups/<group ID>` inside the bridge's workspace, which the bridge creates, so a group's edits never collide with your private session's. Its file tools and sandbox reach what yours reach from that folder.
- **Approvals come to you.** A tool in `ask`, and every `ask_owner` question, goes to your private chat, marked with the group it came from. Only you answer it. Nobody in the group can approve anything.
- **Jobs report to the group.** When one of its background jobs finishes, the bridge gives the result to the group's session, and its answer goes to the group.
- **It keeps running.** It is not stopped when the group is quiet, so its running jobs and conversation survive. There are only as many as you list. It ends when the bridge stops.
- **Scheduled tasks run in your private session.** The schedule belongs to the bridge, not to a group, so a task a group asks for runs and reports in your private chat.
- **Messages carry the sender's name.** The bridge sends `<display name>: <text>`, so a member's message is never taken for a command such as `/compact`.

A group that is not listed gets a guest session as before. Removing a group from the list takes effect when the bridge restarts.

## Deferred

- Photos, files, and voice messages.
- Resuming a session after a restart with `session/load`, which needs saved history.
- Carrying the previous session's summary into the nightly fresh session.
- Choosing the time of the nightly fresh session.
- Several sessions at once for you, such as one per chat thread.
- More tools for guests, such as the browser with a profile that holds no sign-ins.
- Commands such as `/new` and `/cancel` for a trusted group's session.
- Rate limits for group mentions, and an allowlist of groups.
- Answering group messages that do not mention Alyosha.
- Typing indicators in QQ groups.
- Turning the TUI into an ACP client, so that it is a bridge too.

[Chat channels implementation guide](../develop/Channels.md)
