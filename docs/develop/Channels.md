# Implement the ACP mode

`alyosha acp` serves one ACP agent connection over standard input and output and drives the same session driver as the TUI. Implement it at step 7 of the build order, after the TUI path works, because it adds a front end and nothing else. The Telegram bridge drives it. [Telegram bridge](TelegramBridge.md) [Chat channels design](../design/Channels.md) · [Session driver](AgentLoop.md#session-driver) · [Architecture overview](../README.md)

## Library and process

Alyosha speaks ACP's newline-delimited JSON-RPC directly, in `src/acp.rs`, and the bridges do the same in `bridge/`. It needs only the messages this guide lists. The official [`agent-client-protocol`](https://docs.rs/agent-client-protocol) crate was the first choice; its API changes incompatibly between releases, and writing the few messages by hand removed that churn. The protocol is JSON-RPC 2.0 and its version is the integer `1`. [ACP overview](https://agentclientprotocol.com/protocol/overview)

Standard output carries only protocol messages. Send logs, warnings meant for a developer, and MCP servers' standard error to the `alyosha-acp` log file, never to standard output. [Logging](../reference/Logging.md) Exit when standard input closes, after ending the open session.

## Initialization

Answer `initialize` with protocol version `1` and these agent capabilities:

| Capability | Value | Why |
| --- | --- | --- |
| `loadSession` | `false` | History lives only in memory. |
| `promptCapabilities.image` | `true` | Inline JPEG, PNG, or WebP. |
| `promptCapabilities` `audio`, `embeddedContext` | `false` | Neither is supported. |
| `mcpCapabilities` `http`, `sse` | `false` | Bridge-supplied servers are ignored. |
| `authMethods` | empty | Only a program that can start Alyosha as you can connect. |

Ignore the client's `fs` and `terminal` capabilities. Never call `fs/*` or `terminal/*` methods.

If the request carries `clientInfo`, keep its `name` for the channel line that each session adds to its system instruction. [The channel note](Schedule.md#the-channel-note-in-alyosha-acp)

## Sessions

```text
AcpSession {
    session_id: String        # issued by Alyosha, unique per process
    driver: SessionDriver     # the same driver the TUI uses
    first_reply_sent: bool    # startup warnings go before the first reply
}
```

On `session/new`:

1. Check that `cwd` is an absolute path to an existing directory. Otherwise return a JSON-RPC invalid-params error and keep any open session.
2. If a session is open, cancel its active work, wait for the turn to close, and end it, shutting down its MCP servers. Any prompts still waiting in its queue end with `stopReason: "cancelled"`.
3. Start a new session with `cwd` as its workspace, using the normal startup: configured MCP servers, memory index, skills index, and the dev shell. Answer only once startup has finished, which can take up to the dev shell's 10-minute limit. Ignore the request's `mcpServers` list. [Project dev shell](EnvironmentInterface.md#project-dev-shell) [Session startup](AgentLoop.md#session-driver)
4. Return the new `sessionId`, then send `available_commands_update` with two commands: `compact`, described as "Summarize older conversation to free context.", and `job`, as the jobs guide describes. [`/job` command](Jobs.md#job-command)

Requests naming any other `sessionId` get a JSON-RPC error.

## Prompts

Join the prompt's `text` blocks with blank lines. Render each `resource_link` block as its name and URI on its own line. Accept at most four `image` blocks with `mimeType` and standard base64 `data` (up to 2 MiB decoded per image). Invalid or oversized blocks get an invalid-params error. Text and images together form one user message. If it has no images and the text is exactly `/compact`, run manual compaction instead of queuing a message. [Compaction command](ContextManager.md#compact-command)

Enqueue the message and keep the JSON-RPC request open until that message's turn closes. History keeps up to 16 MiB of recent encoded images per session, dropping old image data but keeping the text. Failed turns do not replay their images in later requests. Several prompts may be open at once. Each is answered in queue order. ACP does not say whether a client may send a second prompt during a turn, so accept it rather than reject it.

While the turn runs, send `session/update` notifications for that session:

| Event | Update |
| --- | --- |
| A tool call starts | `tool_call` with the call ID as `toolCallId`, the TUI status line as `title`, a `kind`, and `status: "in_progress"` |
| A tool call finishes | `tool_call_update` with `status: "completed"`, or `"failed"` for a tool error or cancelled result |
| Assistant text, alongside tool calls or as the final reply | `agent_message_chunk` with the text, in the order the response gave it |

Map `kind` as follows: `read_file` is `read`, `write_file` and `edit_file` are `edit`, `run_command` is `execute`, and everything else is `other`. Send no tool output in the update, because the bridge forwards updates to a chat app.

Send a status notice during a turn or command, such as **Compacting context…**, as a pseudo tool call: `tool_call` with a `toolCallId` of the form `alyosha-status-<n>`, the TUI's text as `title`, `kind: "think"`, and `status: "in_progress"`. When the work it describes ends, send `tool_call_update` with `status: "completed"`, or `"failed"` if it failed. Bridges show it like any tool call, so they need no code for it. Do not send the TUI's own lines for approvals, **Answered: …** and **Not answered in time**, because the bridges show their own. [The ACP mode](Approvals.md#the-acp-mode)

Before the first reply in a session, send each startup warning, such as a failed MCP server or a skipped skill, as its own `agent_message_chunk`. [MCP startup warnings](MCP.md#start-servers-and-register-tools)

Answer the prompt from the turn outcome: [Turn outcomes](AgentState.md#state-transitions)

| Turn outcome | Prompt response |
| --- | --- |
| `Completed` | `stopReason: "end_turn"` |
| `Stopped` | `stopReason: "cancelled"` |
| `Failed(reason)` | A JSON-RPC error whose message gives the reason |

A text-only prompt starting with `/job ` starts a background job without queuing and answers the same way. [`/job` command](Jobs.md#job-command) A `/compact` prompt sends the command's status text, such as **No older context to compact** or the busy message, as an `agent_message_chunk` and ends with `end_turn`.

## Daily fresh session

Once a minute, read the time in the configured timezone. On each local date, the first check at or after 04:00 decides, and no later check that date does:

- **Busy:** active work, a queued message, an open prompt, an open permission request, or a running background job. Skip until the next date.
- **Idle:** renew the session.

A session opened at or after 04:00 counts as that date's fresh session, so its first renewal is at 04:00 the next date. This includes the session a renewal opens, whose own check must not renew it again. A session opened before 04:00 renews at 04:00 the same date.

To renew, end the session as `session/new` would: shut down its MCP servers and remove its session logs. Read `config.toml` again and start a new session in the same `cwd`, with the same channel line, under the **same `sessionId`**. Send `available_commands_update` again. Set `first_reply_sent` to false and put this line first among the startup warnings: *Started a fresh session at 04:00. Earlier conversation is gone; memory keeps lasting facts.* A prompt that arrives during the renewal waits in the new session's queue. [Sessions](#sessions) · [Session startup](AgentLoop.md#session-driver)

If the configuration fails to load or the workspace is no longer valid, keep the current session, log the error, and try again on the next date. [Logging](../reference/Logging.md)

## Approvals and questions

Send `session/request_permission` for every approval and `ask_owner` question, as the approvals guide describes. The prompt stays open while the request waits. [The ACP mode](Approvals.md#the-acp-mode)

## Cancel

On `session/cancel`, signal Stop on the active turn or command. An open permission request then ends as `Cancelled`, whatever the bridge answers to it afterwards. [The ACP mode](Approvals.md#the-acp-mode) Send any remaining tool call updates before answering the prompt with `cancelled`, because ACP requires updates to arrive before the response. Messages still waiting in the queue are not cancelled. They run in order afterwards. [Stop](../design/AgentLoop.md#commands-and-interruptions)

## Finished jobs

When a background job finishes, do not queue its message. Send the notification `_alyosha/job_finished` with `sessionId`, `jobId`, and the message `text`, and let the bridge send that text back as a `session/prompt`. [When a job finishes](Jobs.md#when-a-job-finishes)

## Guest mode

`alyosha acp --guest` serves the guest session of one group chat. Bridges start one per group. Everything above applies, with these differences. [Group chats](../design/Channels.md#group-chats)

- **Session.** `session/new` still requires an absolute `cwd`, but the session ignores it. It runs in `${XDG_DATA_HOME:-$HOME/.local/share}/alyosha/guest`, created with mode `0700`, which is only the working directory of its MCP servers. It loads no dev shell and no sandbox.
- **Tools.** Register `user_memory` and the tools of the MCP servers named in `[guest] mcp_servers`, and nothing else: no workspace, shell, Code Mode, job, `memory`, `skill`, or `ask_owner` tool. The session has no approver. `available_commands_update` lists no commands. [People's notes](Memory.md#peoples-notes) · [Configuration](../reference/Configuration.md#guest)
- **Instruction.** Use the guest instruction below instead of the normal one, with no memory or skills index. The channel line is "People write to you in a group chat through <client name>."
- **Prompts.** Each `session/prompt` must carry the sender in `_meta`: `{"alyosha": {"sender": {"id": "telegram-123", "name": "Bob"}}}`. Refuse a prompt without a valid sender with an invalid-params error. Treat all text as a message: `/compact` and `/job` are not commands here.
- **Startup warnings** go to the log only, never into a group.

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

## Group mode

`alyosha acp --group` serves the session of one trusted group chat. Bridges start one per trusted group. It is the normal mode above in every way but one: the channel line is "People in a group chat your owner trusts write to you through <client name>. Each message starts with the sender's display name." It has the workspace, sandbox, tools, memory, skills, jobs, approvals, and commands of a normal session, and `session/prompt` needs no `_meta`. The bridge puts the sender's name in front of the text, so a member's message never starts with `/`. Log to the `alyosha-acp-group` log file. [Trusted groups](../design/Channels.md#trusted-groups)

## Verification

These run under `cargo test` with an in-process ACP client and the scripted model from the turn fixture. [Turn fixture](AgentLoop.md#verification-with-a-scripted-model-and-fake-tools)

- Send `initialize`. Check protocol version `1`, `loadSession: false`, image support, and no audio or embedded-context support. Verify an inline image makes it to the model request and an invalid image is rejected.
- Send `session/new` with a relative `cwd`, then one that does not exist. Check that both fail and no session starts.
- Send `session/new` with a valid `cwd` and an `mcpServers` entry. Check that only configured servers start and that `available_commands_update` lists `compact` and `job`.
- Script a turn that crosses the compaction threshold. Check for a `tool_call` titled **Compacting context…** with kind `think` and an `alyosha-status-` ID, then its `completed` update, before the next tool call or the reply.
- Configure a workspace whose dev shell takes several seconds to load. Check that `session/new` answers only after it has loaded.
- Script a turn that reads two files, one missing, then replies. Check the order: two `tool_call` and `tool_call_update` pairs, the second `failed`, then the reply as `agent_message_chunk`, then `end_turn`.
- Send two prompts without waiting. Check that both are answered, in order, each with its own reply.
- Send `session/cancel` during a scripted tool call. Check that all updates arrive before the response and that the response is `cancelled`.
- Send `/compact` during a turn. Check that it returns the busy message and does not enter history.
- In group mode, check that the channel line names a trusted group chat and that the session has the normal tools and `available_commands_update`.
- In guest mode, check that the tool list holds only `user_memory` and the `[guest]` servers' tools, that the system instruction holds no memory index, that a prompt without a sender is refused, and that `/job ls` is sent to the model as text.
- Script a model failure. Check that the prompt gets a JSON-RPC error and the next prompt still runs.
- Send `session/new` while a session is open with a waiting prompt. Check that the waiting prompt ends `cancelled` and the old session's MCP servers exit.
- Parse every line written to standard output during these cases. Check that each is a valid JSON-RPC message.
- With a fake clock, reach 04:00 while idle. Check that the `sessionId` is unchanged, that history and the checkpoint are empty, that the old MCP server processes exited and new ones started, that the session logs were removed, and that the next prompt's first `agent_message_chunk` is the fresh-session line.
- Open a session at 10:00 and keep it idle past several checks. Check that it does not renew. After it renews at 04:00 the next date, keep it idle for hours. Check that it does not renew again that date.
- Reach 04:00 with a background job running. Check that nothing changes at 04:00 or later that date, and that the session renews at 04:00 the next date once idle.
- Send a prompt during the renewal. Check that it is answered in the new session.
- Break `config.toml` before 04:00. Check that the old session stays usable and the error is logged.
- Close standard input. Check that the process exits and no MCP server process remains.

With a messaging-app bridge, which needs a bot account and network access:

- Send a message from your own account. Check that the reply arrives and that tool calls appear as the bridge shows them.
- Send a message from another account. Check that the bridge ignores it and Alyosha receives nothing.

[Architecture overview](../README.md)
