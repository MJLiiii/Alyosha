# Configuration

All of Alyosha's settings live in one file, `${XDG_CONFIG_HOME:-$HOME/.config}/alyosha/config.toml`. You write it, and Alyosha never changes it. Every Alyosha program reads it: the TUI, `alyosha acp`, the Telegram and QQ bridges, and the schedule server. [Implementation guide](../develop/Configuration.md) · [Architecture overview](../README.md)

## Rules

- **Mode 600.** The file holds your secrets, so every program refuses to start if anyone but you can read it.
- **Strict.** An unknown section or key is an error, and the program refuses to start. A misspelled `ask` would otherwise turn off an approval without notice. The error names the line and the key.
- **Optional.** A missing file is the same as an empty one. Every section has defaults except `[mcp.servers.*]`, `[telegram]`, and `[qq]`. Without them there are no MCP servers and the bridges refuse to start.
- **Read at start.** The TUI and `alyosha acp` read the file when a session starts. The bridges and the schedule server read it when they start. Restart a bridge's service after editing.
- **TOML 1.0.** Inline tables must fit on one line, so write longer tables, such as an MCP server's `env`, as their own sections.
- **What Alyosha writes goes elsewhere.** The gateway's master key, admin key, and gateway key are in `gproxy.env` beside this file. You write `config.toml`, and Alyosha writes `gproxy.env`. [Run GPROXY as a service](../develop/LLMClientGateway.md#run-gproxy-as-a-service)

The folders for memory, skills, and data follow the XDG locations and are not configurable. The TUI takes its workspace from where you start it. [TUI startup](../design/TUI.md#starting)

## Example

```toml
timezone = "Europe/Amsterdam"

[model]
provider = "chatgpt"
api = "codex"
name = "gpt-6.2-sol"

[gateway]
url = "http://127.0.0.1:18787"

[approvals]
timeout_minutes = 60

[telegram]
owner_id = 123456789
workspace = "/home/mjli/work/alyosha-home"
token = "..."

[qq]
app_id = "102000000"
owner_openid = "..."
workspace = "/home/mjli/work/alyosha-qq"
secret = "..."

[guest]
mcp_servers = ["search"]

[mcp.servers.search]
command = "open-websearch"
tools = ["search"]

[mcp.servers.search.env]
MODE = "stdio"
```

## Settings

### Top level

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `timezone` | IANA name | `"Europe/Amsterdam"` | Your timezone. Used for the clock line on each message, for scheduled tasks, and for the 04:00 fresh start of chat sessions. [Clock line](../design/AgentLoop.md#user-message-turn) · [Scheduled tasks](../design/Schedule.md#what-a-task-is) |

### `[model]`

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `provider` | string | `"chatgpt"` | The GPROXY provider name. `alyosha gateway login` creates it with this name. [Gateway login](../develop/LLMClientGateway.md#run-gproxy-as-a-service) |
| `api` | `"codex"`, `"openai"`, `"claudeapi"`, `"aistudio"`, or `"deepseek"` | `"codex"` | The provider's native API, which decides how the client encodes requests. [Native requests](../develop/LLMClientGateway.md#native-requests-and-credentials) |
| `name` | string | `"gpt-6.2-sol"` | The exact model ID. |

### `[gateway]`

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `url` | URL | `"http://127.0.0.1:18787"` | The GPROXY service's address. Must match the service's port. |

### `[approvals]`

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `timeout_minutes` | integer, at least 1 | `60` | How long an approval or `ask_owner` question waits before counting as unanswered. [Approvals](../design/Approvals.md#rules) |

### `[telegram]`

Needed only by `alyosha-telegram`. Its presence also lets scheduled tasks use the `telegram` channel. [Telegram bridge](../develop/TelegramBridge.md#configuration)

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `owner_id` | integer | required | Your numeric Telegram user ID. |
| `workspace` | absolute path | required | The bridge session's workspace. |
| `token` | string | required | The bot token from @BotFather. |
| `trusted_groups` | array of integers | `[]` | Group chat IDs whose members get your full powers through a session of their own. The bridge logs the chat ID of every mention. [Trusted groups](../design/Channels.md#trusted-groups) |

### `[qq]`

Needed only by `alyosha-qq`. Its presence also lets scheduled tasks use the `qq` channel. [QQ bridge](../develop/QQBridge.md#configuration)

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `app_id` | string | required | The bot's AppID. |
| `secret` | string | required | The bot's AppSecret. |
| `owner_openid` | string | `""` | Your `user_openid` for this bot. While empty, the bridge answers nobody and logs senders' IDs. |
| `workspace` | absolute path | required | The bridge session's workspace. Use a different folder from Telegram's. |
| `reply_chunk` | integer, 200 to 4000 | `1500` | Characters per QQ message. |
| `trusted_groups` | array of strings | `[]` | `group_openid`s whose members get your full powers through a session of their own. The bridge logs the `group_openid` of every mention. [Trusted groups](../design/Channels.md#trusted-groups) |

### `[guest]`

Guest sessions answer anyone who mentions Alyosha in a group chat that is not in the bridge's `trusted_groups`. They never get your built-in tools. [Group chats](../design/Channels.md#group-chats)

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `mcp_servers` | array of strings | `[]` | The MCP servers a guest session starts, by name. Each must be a configured `[mcp.servers.<name>]` with no tool in `ask`, because nobody in a group can approve. List only servers that reach no account of yours, such as web search. |

### `[mcp.servers.<name>]`

One section per MCP server. `<name>` matches `[a-z0-9][a-z0-9-]{0,31}` and becomes part of its tool names, such as `mcp__search__search`. [MCP configuration](../develop/MCP.md#v1-configuration)

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `command` | string | required | The program to start, found on `PATH`. |
| `args` | array of strings | `[]` | Its arguments, passed unchanged. |
| `tools` | array of strings | every tool | The tools to register. |
| `ask` | array of strings | `[]` | Tools that wait for your approval. When `tools` is set, each must also be in it, or the program refuses to start. A name the server does not offer gives a startup warning. [Approvals](../design/Approvals.md) |
| `env` | table of strings | `{}` | Environment variables for the server, written as `[mcp.servers.<name>.env]`. |

## Who reads what

| Program | Sections it uses |
| --- | --- |
| `alyosha` (TUI and `acp`) | Top level, `[model]`, `[gateway]`, `[approvals]`, `[mcp.servers.*]`, and in guest mode `[guest]` |
| `alyosha-telegram` | Top level, for its task runner, and `[telegram]` |
| `alyosha-qq` | Top level, for its task runner, and `[qq]` |
| `alyosha-schedule` | Top level, and whether `[telegram]` and `[qq]` exist |

Every program checks the whole file, so a mistake in any section stops all of them with the same error.

[Architecture overview](../README.md)
