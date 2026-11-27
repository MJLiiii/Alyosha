# Configuration

All of Alyosha's settings live in one file, `/etc/alyosha/config.toml` in the container. You write them as `services.alyosha.settings` in your host's NixOS configuration, and the container module generates the file. Alyosha never changes it. Every Alyosha program reads it: `alyosha acp`, the Telegram and QQ bridges, and the schedule server. Secrets are not settings: each is a file of its own, set with `alyosha secret set`. [Secrets](#secrets) · [Host module](../Deployment.md#host-module) · [Implementation](#implementation) · [Architecture overview](../README.md)

## Rules

- **No secrets.** The file is generated into the world-readable Nix store, so it holds no token, password, or key. The module refuses a setting under a name that a secret replaces, such as `telegram.token`.
- **Strict.** An unknown section or key is an error. The module runs `alyosha config check` on the generated file, so the rebuild fails and names the line and the key. A misspelled `ask` would otherwise turn off an approval without notice.
- **Optional.** A missing file is the same as an empty one. Every section has defaults except `[mcp.servers.*]`, `[telegram]`, and `[qq]`. Without them there are no MCP servers and the bridges refuse to start.
- **Read at start.** `alyosha acp` reads the file when a session starts. The bridges and the schedule server read it when they start. `nixos-rebuild switch` restarts the bridges when the file changes.
- **TOML 1.0.** The generated file is TOML; in Nix, write the same structure as attribute sets.

## Secrets

Each secret is a file in `/var/lib/alyosha-secrets/` in the container, owned by root and readable by the `alyosha` group, except `gproxy.env`, which only root reads. `alyosha secret set <name>` writes one and restarts the services that use it. [The host command](../Deployment.md#the-host-command)

| Name | Holds | Used by |
| --- | --- | --- |
| `gproxy.env` | `GPROXY_MASTER_KEY`, `GPROXY_ADMIN_PASSWORD`, and `GPROXY_BOOTSTRAP_ADMIN_API_KEY`, written by `alyosha gateway init` | GPROXY, through systemd as root; `alyosha gateway` commands |
| `gateway-key` | The gateway key Alyosha sends to GPROXY, written by `alyosha gateway login` | `alyosha acp` |
| `telegram-token` | The bot token from @BotFather | `alyosha-telegram` |
| `qq-secret` | The bot's AppSecret | `alyosha-qq` |
| `browser-state.json` | The browser's saved sign-ins | the browser's MCP server, by its path [Saved sign-ins](../Browser.md#saved-sign-ins) |
| `gog-keyring-password` | The password of gogcli's encrypted keyring | gogcli's MCP server, through `secret_env` [Email and calendar](../EmailCalendar.md#signing-in) |
| any other name | A secret an MCP server needs | the servers that name it in `secret_env` |

A name matches `[a-z0-9][a-z0-9._-]{0,63}`. A program reads a secret when it starts. A missing secret that a program needs stops it with an error naming the secret and the command that sets it.

## Folders

Alyosha's folders are fixed paths in the container. Environment variables replace them for tests and development on the host.

| Folder | Holds | Variable |
| --- | --- | --- |
| `/etc/alyosha/config.toml` | The settings | `ALYOSHA_CONFIG` |
| `/var/lib/alyosha-secrets` | The secrets | `ALYOSHA_SECRETS` |
| `/var/lib/alyosha` | The home and state of the user `alyosha`: `work/` (`owner`, `groups/<name>`, `guest`), `gog/`, `schedule.toml`, and the skills at `.agents/skills` | `ALYOSHA_STATE` |
| `/var/cache/alyosha` | `shell/`, the commands' shared cache, and `sessions/`, each session's private folder | `ALYOSHA_CACHE` |

GPROXY's data is in `/var/lib/gproxy`. Memory, people's notes, and the conversations are in the session database. The logs are in the container's journal. [Where things live](../Deployment.md#where-things-live)

## Example

```toml
timezone = "Europe/Amsterdam"

[model]
provider = "chatgpt"
api = "codex"
name = "gpt-6.2-sol"

[gateway]
url = "http://127.0.0.1:18787"

[media]
transcribe_provider = "openai"

[approvals]
timeout_minutes = 60

[telegram]
owner_id = 123456789

[qq]
app_id = "102000000"
owner_openid = "..."

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
| `images` | boolean | `true` | Whether the model can see images. With `false`, images you send reach it as a line saying they are not shown. [Images](../Media.md#images) |
| `effort` | `"low"`, `"medium"`, or `"high"` | none | How hard the model reasons, for every session: private chats, trusted groups, and guests. Higher is slower and uses more quota. Without it, the provider's default applies. Only the `codex` and `openai` APIs take it for now; with any other `api`, the configuration is refused. [Native requests](../LLMClientGateway.md#native-requests-and-credentials) |

### `[gateway]`

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `url` | URL | `"http://127.0.0.1:18787"` | The GPROXY service's address. Must match the service's port. |

### `[media]`

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `transcribe_provider` | string | none | The GPROXY provider, on the `openai` channel, that transcribes voice messages and the sound of videos. `alyosha gateway login` creates it and asks for your OpenAI API key. It may be the same as `[model] provider` when that is an `openai` provider. Without it, voice messages reach the model as a line saying transcription is not set up. [Transcription](../Media.md#transcription) |
| `transcribe_model` | string | `"gpt-transcribe"` | The transcription model. |

### `[approvals]`

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `timeout_minutes` | integer, at least 1 | `60` | How long an approval or `ask_owner` question waits before counting as unanswered. [Approvals](../Approvals.md#rules) |

### `[telegram]`

Needed only by `alyosha-telegram`, which also needs the secret `telegram-token`. Its presence also lets scheduled tasks use the `telegram` channel. [Telegram bridge](../TelegramBridge.md#configuration)

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `owner_id` | integer | required | Your numeric Telegram user ID. |
| `trusted_groups` | table of integers | `{}` | Groups whose members get your full powers through a session of their own, each as `<name> = <chat ID>`. The name is the group's working folder, `work/groups/<name>`: it matches `[a-z0-9][a-z0-9_-]{0,31}` and is unique across both bridges. The bridge logs the chat ID of every mention. [Trusted groups](../Channels.md#trusted-groups) |

### `[qq]`

Needed only by `alyosha-qq`, which also needs the secret `qq-secret`. Its presence also lets scheduled tasks use the `qq` channel. [QQ bridge](../QQBridge.md#configuration)

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `app_id` | string | required | The bot's AppID. |
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
| `env` | table of strings | `{}` | Environment variables for the server, written as `[mcp.servers.<name>.env]`. No secrets. |
| `secret_env` | table of strings | `{}` | Environment variables whose values are secrets, each as `<variable> = "<secret name>"`, such as `GOG_KEYRING_PASSWORD = "gog-keyring-password"`. Alyosha reads the secret when it starts the server. [Secrets](#secrets) |

## Who reads what

| Program | Sections it uses |
| --- | --- |
| `alyosha acp` | Top level, `[model]`, `[gateway]`, `[media]`, `[approvals]`, `[mcp.servers.*]`, and in guest mode `[guest]` |
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
Paths::from_env() -> Result<Paths, ConfigError>     # config, secrets, state, cache
Secrets::read(&self, name: &str) -> Result<Secret, ConfigError>
```

Mark every struct `#[serde(deny_unknown_fields)]` and give each optional key its default from the reference. The file holds no secrets. Read each secret through a `Secrets` handle on the secrets folder, into a wrapper type whose `Debug` and `Display` print `"<secret>"`, so a logged value never shows them. [MCP server fields](../MCP.md#v1-configuration)

### Loading

1. Resolve the paths: each folder in [Folders](#folders), or its variable when set. A relative path in a variable is an error.
2. If the file does not exist, return the defaults.
3. Parse it. On a syntax error, an unknown key, or a wrong type, fail with the `toml` crate's message, which names the line and key.
4. Check the values the types cannot: `timezone` is a known IANA name, `gateway.url` is an `http` URL, `timeout_minutes` is at least 1, `reply_chunk` is 200 to 4000, each trusted group name matches its pattern and is used once across both bridges, each group ID appears once, each MCP server name matches its pattern, and each `ask` entry is also in that server's `tools` when `tools` is set, and each `secret_env` value is a valid secret name. Report every failed check, not only the first.

`alyosha config check <file>` runs steps 3 and 4 on a file and prints the errors, for the module's build-time check. It does not read secrets.

Each program calls `load` once at startup, after setting up its logging. [Logging](Logging.md) `alyosha acp` writes it to standard error and exits with status 2 before answering `initialize`. The bridges and the schedule server log it and exit, which systemd reports as a failed start.

A bridge that finds its own section or its secret missing exits with an error naming it. `alyosha-schedule` treats a missing `[telegram]` or `[qq]` as that channel being unavailable. [The MCP server](../Schedule.md#the-mcp-server)

### Verification

These run under `cargo test` against temporary configuration folders.

- Load with no file. Check that every default from the reference is set and there are no MCP servers.
- Load the reference's example. Check every value.
- Add `token = "x"` to `[telegram]`. Check that loading fails as an unknown key.
- Add an unknown key, `asks`, to an MCP server. Check that loading fails and the message names the line and key.
- Set `timezone = "Europe/Amstredam"`, `timeout_minutes = 0`, and a trusted group named `My Group` together. Check that all three are reported at once.
- Name a Telegram group and a QQ group `friends`. Check that loading fails and names both.
- Set `ALYOSHA_CONFIG=/tmp/a.toml` and `ALYOSHA_SECRETS=/tmp/s`. Check that the file and the secrets are read from there, and that `ALYOSHA_SECRETS=s` is an error.
- Give an MCP server `tools = ["search"]` and `ask = ["send"]`. Check that loading fails and names the `ask` entry.
- Read a secret and format it with `{:?}`. Check that its value does not appear. Read a missing one. Check that the error names it and `alyosha secret set`.
- Start `alyosha-telegram` without `[telegram]`. Check that it exits with an error naming the section.

[Architecture overview](../README.md)
