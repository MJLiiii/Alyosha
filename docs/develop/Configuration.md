# Implement the configuration

Put the configuration schema and loader in one library crate that every Alyosha program uses. [Configuration reference](../reference/Configuration.md) · [Architecture overview](../README.md)

## Crate

Add a `config/` package, `alyosha-config`, to the Cargo workspace. `alyosha`, `alyosha-bridge`, `alyosha-telegram`, `alyosha-qq`, and `alyosha-schedule` all depend on it. Use [`serde`](https://serde.rs) with [`toml`](https://docs.rs/toml). [Package layout](TelegramBridge.md#package-layout)

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

Mark every struct `#[serde(deny_unknown_fields)]` and give each optional key its default from the reference. Keep secrets in a wrapper type whose `Debug` and `Display` print `"<secret>"`, so a logged `Config` never shows them. [MCP server fields](MCP.md#v1-configuration)

## Loading

1. Resolve the path from `XDG_CONFIG_HOME`, or `$HOME/.config` if it is unset or not absolute.
2. If the file does not exist, return the defaults.
3. Open it and check its mode on the open handle. If the group or others have any permission, fail with an error that gives the path and the command `chmod 600 <path>`.
4. Parse it. On a syntax error, an unknown key, or a wrong type, fail with the `toml` crate's message, which names the line and key.
5. Check the values the types cannot: `timezone` is a known IANA name, `gateway.url` is an `http` URL, `timeout_minutes` is at least 1, `reply_chunk` is 200 to 4000, each workspace is an absolute path under `$HOME/Projects`, each MCP server name matches its pattern, and each `ask` entry is also in that server's `tools` when `tools` is set. Report every failed check, not only the first.

Each program calls `load` once at startup, then opens its log file. [Logging](../reference/Logging.md) The TUI prints the error and exits with status 2 before touching the terminal. `alyosha acp` writes it to standard error and exits with status 2 before answering `initialize`. The bridges and the schedule server log it and exit, which systemd reports as a failed start. [TUI startup](TUI.md#startup)

A bridge that finds its own section missing exits with an error naming the section. `alyosha-schedule` treats a missing `[telegram]` or `[qq]` as that channel being unavailable. [The MCP server](Schedule.md#the-mcp-server)

## Verification

These run under `cargo test` against temporary configuration folders.

- Load with no file. Check that every default from the reference is set and there are no MCP servers.
- Load the reference's example with secrets filled in. Check every value.
- Make the file readable by the group. Check for the mode error and the `chmod` hint.
- Add an unknown key, `asks`, to an MCP server. Check that loading fails and the message names the line and key.
- Set `timezone = "Europe/Amstredam"`, `timeout_minutes = 0`, and a workspace outside `~/Projects` together. Check that all three are reported at once.
- Give an MCP server `tools = ["search"]` and `ask = ["send"]`. Check that loading fails and names the `ask` entry.
- Format a loaded `Config` with `{:?}`. Check that no token or secret appears.
- Start `alyosha-telegram` without `[telegram]`. Check that it exits with an error naming the section.

[Architecture overview](../README.md)
