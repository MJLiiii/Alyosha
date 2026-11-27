# Chat channels

A chat channel lets you talk to Alyosha from a messaging app and is the only way to talk to it. Telegram and QQ are the first two. Alyosha has no code for any messaging app. It has one generic mode, `alyosha acp`, in which another program drives a session over standard input and output using the [Agent Client Protocol](https://agentclientprotocol.com/protocol/overview) (ACP). Each messaging app gets a separate **bridge** program that starts `alyosha acp`, passes your messages in, and sends the replies back. [Architecture overview](README.md) · [What Alyosha still needs](todo.md)

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
| Resuming a session after a restart | `session/load` |
| `/compact`, `/job` | Slash commands, sent as the text of a `session/prompt` |
| An approval or an `ask_owner` question | `session/request_permission` |

A protocol of Alyosha's own, like the JSON-lines RPC mode of the pi agent, would be less work in the core, but every bridge would have to be written for it. With ACP, an existing bridge may need no code at all.

## What `alyosha acp` does

`alyosha acp` runs the session driver and turn loop. It is the only front end; the TUI was removed on 2026-11-26. [Agent Loop](AgentLoop.md)

- **One session at a time.** `session/new` starts a session in the folder the bridge names, any existing folder, under a key the bridge chooses for the conversation, such as `owner:telegram`. If a session is already open, it ends first, so a bridge's "new conversation" command gives the model a fresh start. Memory carries lasting facts across sessions as usual. [Memory](Memory.md)
- **Sessions survive restarts.** Every history entry is saved to a database as it happens. A bridge that starts sends `session/load` with its key and gets the conversation back, so a crash, a configuration change, or a container reboot costs only the turn in progress and any queued messages. The first reply after a resume says so. There is no nightly fresh start: compaction keeps the conversation small, and the memory and skills indexes are rebuilt at each compaction. [Durable sessions](SessionStore.md)
- **Messages wait their turn.** Each `session/prompt` becomes one queued message. If a turn is running, the next prompt waits in the queue and is answered when its own turn ends.
- **You see what it does.** Each tool call is reported as an ACP tool call, with a short title, for example **MCP: chromium.browser_click** or **Remembered: thesis-repo**. Status lines such as **Compacting context…** are reported the same way, as pseudo tool calls, so the bridge shows them without any code of its own. The reply arrives as message text when the turn ends. Startup warnings, such as an MCP server that failed to start, come before the first reply.
- **Stop works.** `session/cancel` signals the active turn, and the prompt ends as cancelled.
- **The configuration decides the tools.** Alyosha starts the MCP servers in its own configuration and ignores servers the bridge offers in `session/new`, so a bridge cannot add tools. It also ignores the editor features ACP offers, such as the client's file system and terminals, because the Environment Interface already handles files and shell commands. [Environment Interface](EnvironmentInterface.md)
- **Finished jobs reach the bridge.** ACP lets only the client start a turn, so when a background job ends, Alyosha sends the bridge a notification of its own, and the bridge sends the job's message back as a prompt. [Background jobs](Jobs.md#when-a-job-ends)
- **Photos, voice, video, and files.** The bridge downloads what you send and passes it in the prompt as ACP image, audio, and resource blocks. Alyosha turns it into images and text for the model, and saves files in the session's `inbox/`. The model sends files back with `send_file`. [Photos, voice, video, and files](Media.md)
- **The model knows the app.** The system instruction gains one line naming the bridge, taken from ACP's `initialize`, such as "The owner is talking to you through alyosha-qq." The model uses it to send scheduled results to the app you wrote from. [Scheduled tasks](Schedule.md#creating-tasks)

## Trust

The bridge is a program you install and configure, so it is trusted like Alyosha's configuration. Whoever the bridge lets into your session can make Alyosha run shell commands, write files, and use websites with your saved sign-ins. A bridge lets only your own account into your session, in a private chat, and ignores everyone else's private messages without replying. Other people reach Alyosha only by mentioning it in a group. In a group you list as trusted, everyone gets your full powers through that group's own session. In any other group, a separate guest session without your tools answers them. [Group chats](#group-chats) The bridge holds the bot token. Like every other secret, it stays out of the Nix store. [Saved sign-ins](Browser.md#signing-in-to-your-accounts)

Alyosha does not authenticate the bridge, because only programs that can already start `alyosha acp` as its user can talk to it.

## Telegram first

Telegram is the first messaging app, because its bot API is official, free, and needs no public address: the bridge asks Telegram for new messages, so nothing has to reach into the container. The owner and the server are in the Netherlands, where `api.telegram.org` is reachable directly, so the bridge needs no proxy.

Alyosha has its own bridge. Two existing bridges were checked in November 2026 and not chosen. Neither is in nixpkgs, and a bridge holds the bot token and decides who controls Alyosha, so reviewing one would take about as long as writing a small one that does only what this page asks.

| Bridge | Why not |
| --- | --- |
| [telegram-acp-bot](https://github.com/mgaitan/telegram-acp-bot) 0.3.0 | Python, Telegram only, one session per chat. Refuses to start without an allowlist of user IDs. Has `/new`, `/cancel`, a queue for messages sent mid-turn, and buttons for permission requests. Offers its own MCP server for attachments and scheduled messages, which Alyosha ignores. Early alpha with one main author. |
| [OpenACP](https://docs.openacp.ai/) | Node, Telegram plus Discord and Slack, one session per forum topic in a group. Allows every user when its allowlist is empty. Adds a local REST API, a web interface, tunnels, and downloads agents from a registry, all beyond what a bridge needs. |

## The Telegram bridge

`alyosha-telegram` is a separate Rust program in this repository, built in the same package as Alyosha. It shares no code with Alyosha's core and uses the same ACP crate, so both sides speak the same protocol version. It runs as an always-on service in the container, as your user, and starts `alyosha acp` as its child. It asks Telegram for new messages, so it opens no ports. [Telegram bridge implementation](TelegramBridge.md)

```text
containers.alyosha
└─ alyosha-telegram.service (always on, runs as the user alyosha)
     ├─ long-polls api.telegram.org
     └─ alyosha acp (child process) ── same agent core
```

- **Only you in private.** In a private chat it answers only your Telegram user ID. Private messages from anyone else are dropped without a reply. In groups it answers anyone who mentions it, through the group's own session. [Group chats](#group-chats)
- **One long-lived session**, kept across restarts. `/new` starts a fresh one at any time. `/cancel` stops the current turn. `/compact` and `/job` pass through to Alyosha. `/tasks` lists scheduled tasks. Telegram shows the five in its command menu.
- **Runs scheduled tasks.** Tasks for Telegram run as prompts in this session, marked **⏰ Scheduled**. [Scheduled tasks](Schedule.md#running-tasks) Finished background jobs come in the same way, marked **⚙ Job finished**. [Background jobs](Jobs.md)
- **You see it working.** While a turn runs, the chat shows Telegram's typing indicator and one **Working…** message listing tool calls as they happen, such as `MCP: chromium.browser_click`.
- **Formatted replies.** The model writes Markdown, and the reply shows it formatted: bold, italics, code, code blocks, links, and quotes, with headings as bold lines and lists with bullets. If Telegram refuses the formatting, the same part goes again as plain text without the Markdown markers. Long replies are split into several messages, keeping code blocks whole in each. A failed turn shows its error, and a stopped turn says so.
- **Photos, voice, video, and files.** Photos, albums, voice messages, videos, GIFs, stickers, files, locations, and contacts all reach Alyosha, and files it sends arrive as photos, videos, voice messages, or files. [Photos, voice, video, and files](Media.md)
- **Approvals as buttons.** An approval or a question from Alyosha arrives as a message with one button per answer. [Approvals](Approvals.md)
- **Crashes resume.** If `alyosha acp` exits, the bridge tells you and exits, and systemd starts both again. The new child loads the same session. [Durable sessions](SessionStore.md)

Your settings go in the `[telegram]` section of the settings: your Telegram user ID and any trusted groups. The bot token is a secret of its own, set with `alyosha secret set telegram-token`. [Secrets](reference/Configuration.md#secrets) [Configuration](reference/Configuration.md#telegram)

Every bridge's `alyosha acp` uses the same GPROXY service. [Gateway lifetime](LLMClientGateway.md#provider-accounts-and-gateway-lifetime)

## The QQ bridge

`alyosha-qq` works like the Telegram bridge: a separate Rust program, an always-on service in the container, and its own `alyosha acp` child. It uses QQ's official bot API from the [QQ open platform](https://bot.q.qq.com/wiki), not an unofficial client, so it cannot get a QQ account banned. It connects to QQ over a WebSocket, which needs no public address. [QQ bridge implementation](QQBridge.md)

The official API limits replies, and that shapes the bridge:

- **Four replies per message.** The bot can answer each of your messages at most four times, within 60 minutes. The bridge sends no progress messages, only the reply. A reply longer than four messages is cut off with a note, and you can ask Alyosha to continue.
- **No editing.** QQ bots cannot edit sent messages, so there is no **Working…** message. The bridge shows QQ's typing indicator instead.
- **Late replies.** If a turn ends after the 60 minutes, or the four replies are used up, the bridge tries an ordinary message, which QQ allows only within rate limits and with permission. If that fails, the bridge keeps the reply and sends it first when you next write.
- **Messages QQ sends first are limited.** Scheduled tasks for QQ still run, but their results go out as normal or recall messages, which QQ may refuse. Undelivered results wait until you next write. Telegram has no such limits. [Delivering on QQ](Schedule.md#delivering-on-qq)

Approvals and questions arrive as text, and you answer with `/yes`, `/no`, or the number of a choice, because buttons need extra permission from QQ. The request uses one of the four replies, and your answer gives the turn a fresh set of four. [Approvals](Approvals.md)

Otherwise it behaves like the Telegram bridge. In private chats it answers only your account, identified by the `openid` QQ assigns you for this bot, and drops other private messages without replying. In groups it answers anyone who @-mentions it, through the group's own session. [Group chats](#group-chats) It runs one long-lived session with `/new`, `/cancel`, `/compact`, `/job`, and `/tasks`, runs QQ's scheduled tasks, takes photos, voice, video, and files as far as QQ passes them to a bot, and resumes its session after a crash. [Photos, voice, video, and files](Media.md) Replies use QQ's native Markdown when QQ allows the bot to send it. A bot QQ has not granted Markdown gets the same reply as plain text, with the markers removed. Its settings go in a `[qq]` section of `config.toml`, together with the bot's secret.

A bot registered by an individual can be used privately by its creator and by test users added in the bot's settings, without Tencent's review. That covers personal use.

## Two bridges at once

Each bridge runs its own `alyosha acp` session. Each has its own conversation and starts its own MCP servers, including its own Chromium. Both sessions are you, so they share GPROXY, the memory store, and one working folder, `/var/lib/alyosha/work/owner`: a file Alyosha made in Telegram is there in QQ. If both sessions write the same file at the same moment, the later write wins. [Deployment](Deployment.md#where-things-live)

The two bridges share their ACP side in a small library crate that only bridges use: starting `alyosha acp`, opening sessions, sending prompts, and collecting each prompt's updates and reply. Each bridge adds only its messaging app's side. [Package layout](TelegramBridge.md#package-layout)

## Group chats

Anyone can add the bot to a group and @-mention it there. Alyosha replies to that message in the group. The bridges hear nothing else in a group: Telegram's privacy mode and QQ's group API pass a bot only the messages addressed to it.

```text
trusted group T ── @Alyosha ──► bridge ──► alyosha acp --group (group T's session, your tools)
group A ────────── @Alyosha ──► bridge ──► alyosha acp --guest (group A's session) ─┐
group B ────────── @Alyosha ──► bridge ──► alyosha acp --guest (group B's session) ─┼─► notes on each person, by platform and user ID
your private chat ───────────► bridge ──► alyosha acp (your session, your tools) ──┘   (not shared with your own memory)
```

- **What counts as a mention.** In Telegram, `@` followed by the bot's username, or a reply to one of Alyosha's messages. In QQ, an @ of the bot, whether QQ sends the group only @s or, with its "receive all messages" mode, every message. A Telegram mention that replies to someone else's message brings the start of that message and its attachments along, so "@Alyosha is this true?" works, and so does "@Alyosha what is this?" under a photo.
- **A session per group.** Each group gets its own child, started at the first mention. It keeps the group's conversation, so follow-up questions work, and survives restarts like your session. Groups never see each other's conversations or your private chat's.
- **Trusted groups get your powers.** You list trusted groups in the bridge's section of the configuration, each under a short name you choose, with the group ID the bridge logs for every mention. Their session is a full session like yours: shell, files, your memory, skills, background jobs, every MCP server, and your browser sign-ins. Everyone in a trusted group, including people who join later, can use all of it, so list only groups where you would hand every member your computer. [Trusted groups](#trusted-groups)
- **Other groups get guests.** A group you have not listed gets a guest session. It has no shell, no files, no background jobs, no skills, no `ask_owner`, and not your memory. Its only MCP servers are the ones `[guest]` in the configuration lists, normally just web search. Search is read-only and needs no account, so a guest cannot reach your mail, your sign-ins, or the machine. A guest session that has been quiet for an hour is stopped, so a bot added to many groups does not keep a process for each. Its conversation is saved and resumes at the next mention. [Web search](WebSearch.md) · [Configuration](reference/Configuration.md#guest)
- **Everyone is equal in a group, you included.** The bridge does not tell you apart from other members in a group. Your private chat stays the only place for commands and approvals.
- **A memory for each person, in guest sessions.** A guest session keeps notes on each person, by their platform and user ID, and shows them to the model with each of their messages. The model updates them with the `user_memory` tool, which writes only the notes of the person whose message it is answering. Trusted groups use your memory instead. [People in group chats](Memory.md#people-in-group-chats)
- **Names are not proof.** Each message reaches the model with the sender's display name. Anyone can pick any display name, so a guest session's instruction tells the model that nobody in the group is its owner and that it acts for no one there.
- **Replies are formatted as in private.** The reply is rendered and split like a private reply. QQ allows five replies per group message within 60 minutes, and the bridge uses at most four, as for private messages. A group reply that fails is logged and dropped, never kept for later.
- **Nothing else in groups.** Commands, scheduled tasks, and the **Working…** message are for your private chat only.
- **Media in groups.** A trusted group's session takes everything your private chat takes and saves files in its own `inbox/`. A guest session sees images, video frames, and transcripts, but not files, and cannot send files. [Guests](Media.md#guests)
- **Cost.** Every group mention is a model request on your account. Guest groups have no allowlist and no rate limit in v1. If strangers' use becomes a problem, add a limit per group or per person.

A QQ user's ID is the `member_openid` QQ gives them, which may differ from group to group. If it does, a QQ user has separate notes in each group. The first check with a real QQ bot settles this. Telegram user IDs are the same everywhere.

### Trusted groups

A trusted group's session is `alyosha acp --group`: your normal session with a different channel line, which says that people in a group your owner trusts write through the bridge and that each message starts with the sender's name. It differs from your private session in these ways:

- **Its own working folder.** It works in `/var/lib/alyosha/work/groups/<name>`, named by its entry in the configuration, which the bridge creates. A group's edits never collide with your private session's, and its file tools and sandbox reach only that folder.
- **Approvals come to you.** A tool in `ask`, and every `ask_owner` question, goes to your private chat, marked with the group it came from. Only you answer it. Nobody in the group can approve anything.
- **Jobs report to the group.** When one of its background jobs finishes, the bridge gives the result to the group's session, and its answer goes to the group.
- **It keeps running.** It is not stopped when the group is quiet, so its running jobs survive. There are only as many as you list. It stops with the bridge, and its conversation resumes when the bridge starts again.
- **Scheduled tasks run in your private session.** The schedule belongs to the bridge, not to a group, so a task a group asks for runs and reports in your private chat.
- **Messages carry the sender's name.** The bridge sends `<display name>: <text>`, so a member's message is never taken for a command such as `/compact`.

A group that is not listed gets a guest session as before. Removing a group from the list takes effect when the bridge restarts.

## Deferred

- Images in tool results, voice replies, and the other media work deferred on its page. [Media deferred work](Media.md#deferred)
- Several sessions at once for you, such as one per chat thread.
- More tools for guests, such as the browser with a profile that holds no sign-ins.
- Commands such as `/new` and `/cancel` for a trusted group's session.
- Rate limits for group mentions, and an allowlist of groups.
- Answering group messages that do not mention Alyosha.
- Typing indicators in QQ groups.

## Implementation

`alyosha acp` serves one ACP agent connection over standard input and output and drives the session driver. It is Alyosha's only front end. Implement it at step 7 of the build order, because it adds a front end and nothing else. The Telegram bridge drives it. [Telegram bridge](TelegramBridge.md) [Session driver](AgentLoop.md#session-driver) · [Architecture overview](README.md)

### Library and process

Alyosha speaks ACP's newline-delimited JSON-RPC directly, in `src/acp.rs`, and the bridges do the same in `bridge/`. It needs only the messages this section lists. The official [`agent-client-protocol`](https://docs.rs/agent-client-protocol) crate was the first choice; its API changes incompatibly between releases, and writing the few messages by hand removed that churn. The protocol is JSON-RPC 2.0 and its version is the integer `1`. [ACP overview](https://agentclientprotocol.com/protocol/overview)

Standard output carries only protocol messages. Send logs, warnings meant for a developer, and MCP servers' standard error to the `alyosha-acp` log, never to standard output. [Logging](reference/Logging.md) Exit when standard input closes, after ending the open session.

### Initialization

Answer `initialize` with protocol version `1` and these agent capabilities:

| Capability | Value | Why |
| --- | --- | --- |
| `loadSession` | `true` | Sessions are saved and resumed by key. [Durable sessions](SessionStore.md#acp) |
| `promptCapabilities` `image`, `audio`, `embeddedContext` | `true` | Bridges pass photos, voice, video, and files. [From the bridge to `alyosha acp`](Media.md#from-the-bridge-to-alyosha-acp) |
| `mcpCapabilities` `http`, `sse` | `false` | Bridge-supplied servers are ignored. |
| `authMethods` | empty | Only a program that can start Alyosha as its user can connect. |

Ignore the client's `fs` and `terminal` capabilities. Never call `fs/*` or `terminal/*` methods.

If the request carries `clientInfo`, keep its `name` for the channel line that each session adds to its system instruction. [The channel note](Schedule.md#the-channel-note-in-alyosha-acp)

### Sessions

```text
AcpSession {
    session_id: String        # the session's key, such as owner:telegram
    driver: SessionDriver
    first_reply_sent: bool    # startup warnings go before the first reply
}
```

On `session/new`:

1. Check that `cwd` is an absolute path to an existing directory. Otherwise return a JSON-RPC invalid-params error and keep any open session.
2. If a session is open, cancel its active work, wait for the turn to close, and end it, shutting down its MCP servers. Any prompts still waiting in its queue end with `stopReason: "cancelled"`.
3. Create a session in the session store under the key in `_meta.alyosha.sessionKey`, or `acp:<random>` without one, and start it with `cwd` as its workspace, using the normal startup: configured MCP servers, memory index, skills index, and the dev shell. Answer only once startup has finished, which can take up to the dev shell's 10-minute limit. Ignore the request's `mcpServers` list. [Project dev shell](EnvironmentInterface.md#project-dev-shell) [Session startup](AgentLoop.md#session-driver)
4. Return the key as the `sessionId`, then send `available_commands_update` with two commands: `compact`, described as "Summarize older conversation to free context.", and `job`, as the Background jobs page describes. [`/job` command](Jobs.md#job-command)

`session/load` treats its `sessionId` as a key and resumes that key's current session, replaying the conversation first, or answers `-32002` when there is none. [Durable sessions: ACP](SessionStore.md#acp)

Requests naming any other `sessionId` get a JSON-RPC error.

### Prompts

Join the prompt's `text` blocks with blank lines. Render each `resource_link` block as its name and URI on its own line. That text is one user message. Keep `image`, `audio`, and `resource` blocks as the message's attachments, in order; they are prepared when its turn starts. [Preparing a message](Media.md#preparing-a-message) If the text is exactly `/compact` and there are no attachments, run manual compaction instead of queuing a message. [Compaction command](ContextManager.md#run-compact)

Enqueue the message and keep the JSON-RPC request open until that message's turn closes. Several prompts may be open at once. Each is answered in queue order. ACP does not say whether a client may send a second prompt during a turn, so accept it rather than reject it.

While the turn runs, send `session/update` notifications for that session:

| Event | Update |
| --- | --- |
| A tool call starts | `tool_call` with the call ID as `toolCallId`, the status title as `title`, a `kind`, and `status: "in_progress"` |
| A tool call finishes | `tool_call_update` with `status: "completed"`, or `"failed"` for a tool error or cancelled result |
| Assistant text, alongside tool calls or as the final reply | `agent_message_chunk` with the text, in the order the response gave it |
| A file from `send_file` | `agent_message_chunk` with a `resource_link` to the session's copy of the file [Sending files out](Media.md#sending-files-out) |

Map `kind` as follows: `read_file` is `read`, `write_file` and `edit_file` are `edit`, `run_command` is `execute`, and everything else is `other`. Send no tool output in the update, because the bridge forwards updates to a chat app.

Send a status notice during a turn or command, such as **Compacting context…**, as a pseudo tool call: `tool_call` with a `toolCallId` of the form `alyosha-status-<n>`, the notice's text as `title`, `kind: "think"`, and `status: "in_progress"`. When the work it describes ends, send `tool_call_update` with `status: "completed"`, or `"failed"` if it failed. Bridges show it like any tool call, so they need no code for it. Send no status lines for approvals, such as **Answered: …** or **Not answered in time**, because the bridges show their own. [The ACP mode](Approvals.md#the-acp-mode)

Before the first reply in a session, send each startup warning, such as a failed MCP server or a skipped skill, as its own `agent_message_chunk`. [MCP startup warnings](MCP.md#start-servers-and-register-tools)

Answer the prompt from the turn outcome: [Turn outcomes](AgentState.md#state-transitions)

| Turn outcome | Prompt response |
| --- | --- |
| `Completed` | `stopReason: "end_turn"` |
| `Stopped` | `stopReason: "cancelled"` |
| `Failed(reason)` | A JSON-RPC error whose message gives the reason |

A prompt starting with `/job ` starts a background job without queuing and answers the same way. [`/job` command](Jobs.md#job-command) A `/compact` prompt sends the command's status text, such as **No older context to compact** or the busy message, as an `agent_message_chunk` and ends with `end_turn`.

### Approvals and questions

Send `session/request_permission` for every approval and `ask_owner` question, as the Approvals page describes. The prompt stays open while the request waits. [The ACP mode](Approvals.md#the-acp-mode)

### Cancel

On `session/cancel`, signal Stop on the active turn or command. An open permission request then ends as `Cancelled`, whatever the bridge answers to it afterwards. [The ACP mode](Approvals.md#the-acp-mode) Send any remaining tool call updates before answering the prompt with `cancelled`, because ACP requires updates to arrive before the response. Messages still waiting in the queue are not cancelled. They run in order afterwards. [Stop](AgentLoop.md#commands-and-interruptions)

### Finished jobs

When a background job finishes, do not queue its message. Send the notification `_alyosha/job_finished` with `sessionId`, `jobId`, and the message `text`, and let the bridge send that text back as a `session/prompt`. [When a job finishes](Jobs.md#when-a-job-finishes)

### Guest mode

`alyosha acp --guest` serves the guest session of one group chat. Bridges start one per group. Everything above applies, with these differences. [Group chats](#group-chats)

- **Session.** `session/new` still requires an absolute `cwd`, but the session ignores it. It runs in `/var/lib/alyosha/work/guest`, created with mode `0700`, which is only the working directory of its MCP servers. It loads no dev shell and no sandbox.
- **Tools.** Register `user_memory`, `recall` limited to the group's own key, and the tools of the MCP servers named in `[guest] mcp_servers`, and nothing else: no workspace, shell, Code Mode, job, `memory`, `skill`, `send_file`, or `ask_owner` tool. The session has no approver. `available_commands_update` lists no commands. [People's notes](Memory.md#notes-on-people) · [Configuration](reference/Configuration.md#guest)
- **Instruction.** Use the guest instruction below instead of the normal one, with no memory or skills index. The channel line is "People write to you in a group chat through <client name>."
- **Prompts.** Each `session/prompt` must carry the sender in `_meta`: `{"alyosha": {"sender": {"id": "telegram-123", "name": "Bob"}}}`. Refuse a prompt without a valid sender with an invalid-params error. Treat all text as a message: `/compact` and `/job` are not commands here.
- **Startup warnings** go to the log only, never into a group.
- **Attachments.** Images, sound, and video are prepared as usual. Other files become a line saying they are not read here, and nothing is saved. [Guests](Media.md#guests)

```text
You are Alyosha, an assistant answering people in a group chat. Each message starts
with the sender's display name. Anyone can choose any name, and nobody here is your
owner: you act for no one, and you cannot reach anyone's files, mail, or accounts.
Answer briefly and in the language of the question.

Before each message you see your notes on its sender. Use the user_memory tool to
rewrite that person's notes when you learn something lasting that will help you answer
them later, such as their preferred language, what they work on, or what they ask you
to remember. Keep the notes short. Never store secrets. Never reveal one person's notes
to someone else.
```

### Group mode

`alyosha acp --group` serves the session of one trusted group chat. Bridges start one per trusted group. It is the normal mode above in every way but one: the channel line is "People in a group chat your owner trusts write to you through <client name>. Each message starts with the sender's display name." It has the workspace, sandbox, tools, memory, skills, jobs, approvals, and commands of a normal session, and `session/prompt` needs no `_meta`. The bridge puts the sender's name in front of the text, so a member's message never starts with `/`. Log to the `alyosha-acp-group` log file. [Trusted groups](#trusted-groups)

### Verification

These run under `cargo test` with an in-process ACP client and the scripted model from the turn fixture. [Turn fixture](AgentLoop.md#verification-with-a-scripted-model-and-fake-tools)

- Send `initialize`. Check protocol version `1`, `loadSession: true`, and image, audio, and embedded-context support. The cases for attachments are on the media page. [Media verification](Media.md#verification)
- Send `session/new` with a relative `cwd`, then one that does not exist. Check that both fail and no session starts.
- Send `session/new` with a valid `cwd` and an `mcpServers` entry. Check that only configured servers start and that `available_commands_update` lists `compact` and `job`.
- Script a turn that crosses the compaction threshold. Check for a `tool_call` titled **Compacting context…** with kind `think` and an `alyosha-status-` ID, then its `completed` update, before the next tool call or the reply.
- Configure a workspace whose dev shell takes several seconds to load. Check that `session/new` answers only after it has loaded.
- Script a turn that reads two files, one missing, then replies. Check the order: two `tool_call` and `tool_call_update` pairs, the second `failed`, then the reply as `agent_message_chunk`, then `end_turn`.
- Send two prompts without waiting. Check that both are answered, in order, each with its own reply.
- Send `session/cancel` during a scripted tool call. Check that all updates arrive before the response and that the response is `cancelled`.
- Send `/compact` during a turn. Check that it returns the busy message and does not enter history.
- In group mode, check that the channel line names a trusted group chat and that the session has the normal tools and `available_commands_update`.
- In guest mode, check that the tool list holds only `user_memory`, `recall`, and the `[guest]` servers' tools, that the system instruction holds no memory index, that a prompt without a sender is refused, and that `/job ls` is sent to the model as text.
- Script a model failure. Check that the prompt gets a JSON-RPC error and the next prompt still runs.
- Send `session/new` while a session is open with a waiting prompt. Check that the waiting prompt ends `cancelled` and the old session's MCP servers exit.
- Parse every line written to standard output during these cases. Check that each is a valid JSON-RPC message.
- Resume a session with `session/load`. The cases are on the Durable sessions page. [Verification](SessionStore.md#verification)
- Close standard input. Check that the process exits and no MCP server process remains.

With a messaging-app bridge, which needs a bot account and network access:

- Send a message from your own account. Check that the reply arrives and that tool calls appear as the bridge shows them.
- Send a message from another account. Check that the bridge ignores it and Alyosha receives nothing.

[Architecture overview](README.md)
