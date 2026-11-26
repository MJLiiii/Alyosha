# Configuration

All of Alyosha's settings live in one file, `~/alyosha/config/config.toml`. You write it, and Alyosha never changes it. Every Alyosha program reads it: `alyosha acp`, the Telegram and QQ bridges, and the schedule server. [Implementation](#implementation) · [Architecture overview](../README.md)

## Rules

- **Mode 600.** The file holds your secrets, so every program refuses to start if anyone but you can read it.
- **Strict.** An unknown section or key is an error, and the program refuses to start. A misspelled `ask` would otherwise turn off an approval without notice. The error names the line and the key.
- **Optional.** A missing file is the same as an empty one. Every section has defaults except `[mcp.servers.*]`, `[telegram]`, and `[qq]`. Without them there are no MCP servers and the bridges refuse to start.
- **Read at start.** `alyosha acp` reads the file when a session starts. The bridges and the schedule server read it when they start. Restart a bridge's service after editing.
- **TOML 1.0.** Inline tables must fit on one line, so write longer tables, such as an MCP server's `env`, as their own sections.
- **What Alyosha writes goes elsewhere.** The gateway's master key, admin key, and gateway key are in `~/alyosha/state/secrets/gproxy.env`. You write `config.toml`, and Alyosha writes `gproxy.env`. In the container the `config` folder is read-only. [Run GPROXY as a service](../LLMClientGateway.md#run-gproxy-as-a-service)

## Folders

Alyosha's folders are fixed. Only their root can move: the environment variable `ALYOSHA_HOME`, an absolute path, replaces `~/alyosha`, for tests and development.

| Folder | Holds |
| --- | --- |
| `~/alyosha/config` | `config.toml`, `skills/`, and the browser's saved sign-ins. You write it; read-only in the container. |
| `~/alyosha/state` | `secrets/gproxy.env`, GPROXY's database, the Google sign-in store, `schedule.toml`, `logs/`, and `backups/`. |
| `~/work` | The sessions' working folders, only inside the container: `owner`, `groups/<name>`, and `guest`. |
| `~/.cache/alyosha` | The shell cache and each session's private folders, only inside the container. |

Memory, people's notes, and the conversations are in the session database. [Deployment](../Deployment.md#what-it-can-see)

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
token = "..."

[qq]
app_id = "102000000"
owner_openid = "..."
secret = "..."

[qq.trusted_groups]
friends = "12D5F61BED16A072D96DB3D9E04A7B3D"

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
| `timezone` | IANA name | `"Europe/Amsterdam"` | Your timezone. Used for the clock line on each message and for scheduled tasks. [Clock line](../AgentLoop.md#user-message-turn) · [Scheduled tasks](../Schedule.md#what-a-task-is) |

### `[model]`

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `provider` | string | `"chatgpt"` | The GPROXY provider name. `alyosha gateway login` creates it with this name. [Gateway login](../LLMClientGateway.md#run-gproxy-as-a-service) |
| `api` | `"codex"`, `"openai"`, `"claudeapi"`, `"aistudio"`, or `"deepseek"` | `"codex"` | The provider's native API, which decides how the client encodes requests. [Native requests](../LLMClientGateway.md#native-requests-and-credentials) |
| `name` | string | `"gpt-6.2-sol"` | The exact model ID. |
| `effort` | `"low"`, `"medium"`, or `"high"` | none | How hard the model reasons, for every session: private chats, trusted groups, and guests. Higher is slower and uses more quota. Without it, the provider's default applies. Only the `codex` and `openai` APIs take it for now; with any other `api`, the configuration is refused. [Native requests](../LLMClientGateway.md#native-requests-and-credentials) |

### `[gateway]`

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `url` | URL | `"http://127.0.0.1:18787"` | The GPROXY service's address. Must match the service's port. |

### `[approvals]`

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `timeout_minutes` | integer, at least 1 | `60` | How long an approval or `ask_owner` question waits before counting as unanswered. [Approvals](../Approvals.md#rules) |

### `[telegram]`

Needed only by `alyosha-telegram`. Its presence also lets scheduled tasks use the `telegram` channel. [Telegram bridge](../TelegramBridge.md#configuration)

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `owner_id` | integer | required | Your numeric Telegram user ID. |
| `token` | string | required | The bot token from @BotFather. |
| `trusted_groups` | table of integers | `{}` | Groups whose members get your full powers through a session of their own, each as `<name> = <chat ID>`. The name is the group's folder, `~/work/groups/<name>`: it matches `[a-z0-9][a-z0-9_-]{0,31}` and is unique across both bridges. The bridge logs the chat ID of every mention. [Trusted groups](../Channels.md#trusted-groups) |

### `[qq]`

Needed only by `alyosha-qq`. Its presence also lets scheduled tasks use the `qq` channel. [QQ bridge](../QQBridge.md#configuration)

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `app_id` | string | required | The bot's AppID. |
| `secret` | string | required | The bot's AppSecret. |
| `owner_openid` | string | `""` | Your `user_openid` for this bot. While empty, the bridge answers nobody and logs senders' IDs. |
| `reply_chunk` | integer, 200 to 4000 | `1500` | Characters per QQ message. |
| `trusted_groups` | table of strings | `{}` | Groups whose members get your full powers through a session of their own, each as `<name> = "<group_openid>"`, with names as for Telegram. The bridge logs the `group_openid` of every mention. [Trusted groups](../Channels.md#trusted-groups) |

### `[guest]`

Guest sessions answer anyone who mentions Alyosha in a group chat that is not in the bridge's `trusted_groups`. They never get your built-in tools. [Group chats](../Channels.md#group-chats)

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `mcp_servers` | array of strings | `[]` | The MCP servers a guest session starts, by name. Each must be a configured `[mcp.servers.<name>]` with no tool in `ask`, because nobody in a group can approve. List only servers that reach no account of yours, such as web search. |

### `[mcp.servers.*]`

One section per MCP server. `<name>` matches `[a-z0-9][a-z0-9-]{0,31}` and becomes part of its tool names, such as `mcp__search__search`. [MCP configuration](../MCP.md#v1-configuration)

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `command` | string | required | The program to start, found on `PATH`. |
| `args` | array of strings | `[]` | Its arguments, passed unchanged. |
| `tools` | array of strings | every tool | The tools to register. |
| `ask` | array of strings | `[]` | Tools that wait for your approval. When `tools` is set, each must also be in it, or the program refuses to start. A name the server does not offer gives a startup warning. [Approvals](../Approvals.md) |
| `env` | table of strings | `{}` | Environment variables for the server, written as `[mcp.servers.<name>.env]`. |

## Who reads what

| Program | Sections it uses |
| --- | --- |
| `alyosha acp` | Top level, `[model]`, `[gateway]`, `[approvals]`, `[mcp.servers.*]`, and in guest mode `[guest]` |
| `alyosha-telegram` | Top level, for its task runner, and `[telegram]` |
| `alyosha-qq` | Top level, for its task runner, and `[qq]` |
| `alyosha-schedule` | Top level, and whether `[telegram]` and `[qq]` exist |

Every program checks the whole file, so a mistake in any section stops all of them with the same error.

[Architecture overview](../README.md)

## Implementation

Put the configuration schema and loader in one library crate that every Alyosha program uses. [Architecture overview](../README.md)

### Crate

Add a `config/` package, `alyosha-config`, to the Cargo workspace. `alyosha`, `alyosha-bridge`, `alyosha-telegram`, `alyosha-qq`, and `alyosha-schedule` all depend on it. Use [`serde`](https://serde.rs) with [`toml`](https://docs.rs/toml). [Package layout](../TelegramBridge.md#package-layout)

```text
Config {
    timezone: chrono_tz::Tz
    model: ModelConfig            # provider, api, name
    gateway: GatewayConfig        # url
    approvals: ApprovalsConfig    # timeout_minutes
    telegram: Option<TelegramConfig>
    qq: Option<QqConfig>
    mcp: McpConfig                # servers: BTreeMap<String, McpServerConfig>
}

load() -> Result<Config, ConfigError>
```

Mark every struct `#[serde(deny_unknown_fields)]` and give each optional key its default from the reference. Keep secrets in a wrapper type whose `Debug` and `Display` print `"<secret>"`, so a logged `Config` never shows them. [MCP server fields](../MCP.md#v1-configuration)

### Loading

1. Resolve the root from `ALYOSHA_HOME`, or `$HOME/alyosha` if it is unset. A relative `ALYOSHA_HOME` is an error. The file is `<root>/config/config.toml`. The same `Paths` value gives every program `<root>/config`, `<root>/state`, `$HOME/work`, and `${XDG_CACHE_HOME:-$HOME/.cache}/alyosha`. [Folders](#folders)
2. If the file does not exist, return the defaults.
3. Open it and check its mode on the open handle. If the group or others have any permission, fail with an error that gives the path and the command `chmod 600 <path>`.
4. Parse it. On a syntax error, an unknown key, or a wrong type, fail with the `toml` crate's message, which names the line and key.
5. Check the values the types cannot: `timezone` is a known IANA name, `gateway.url` is an `http` URL, `timeout_minutes` is at least 1, `reply_chunk` is 200 to 4000, each trusted group name matches its pattern and is used once across both bridges, each group ID appears once, each MCP server name matches its pattern, and each `ask` entry is also in that server's `tools` when `tools` is set. Report every failed check, not only the first.

Each program calls `load` once at startup, then opens its log file. [Logging](Logging.md) `alyosha acp` writes it to standard error and exits with status 2 before answering `initialize`. The bridges and the schedule server log it and exit, which systemd reports as a failed start.

A bridge that finds its own section missing exits with an error naming the section. `alyosha-schedule` treats a missing `[telegram]` or `[qq]` as that channel being unavailable. [The MCP server](../Schedule.md#the-mcp-server)

### Verification

These run under `cargo test` against temporary configuration folders.

- Load with no file. Check that every default from the reference is set and there are no MCP servers.
- Load the reference's example with secrets filled in. Check every value.
- Make the file readable by the group. Check for the mode error and the `chmod` hint.
- Add an unknown key, `asks`, to an MCP server. Check that loading fails and the message names the line and key.
- Set `timezone = "Europe/Amstredam"`, `timeout_minutes = 0`, and a trusted group named `My Group` together. Check that all three are reported at once.
- Name a Telegram group and a QQ group `friends`. Check that loading fails and names both.
- Set `ALYOSHA_HOME=/tmp/a`. Check that the file is read from `/tmp/a/config/config.toml`.
- Give an MCP server `tools = ["search"]` and `ask = ["send"]`. Check that loading fails and names the `ask` entry.
- Format a loaded `Config` with `{:?}`. Check that no token or secret appears.
- Start `alyosha-telegram` without `[telegram]`. Check that it exits with an error naming the section.

[Architecture overview](../README.md)
