//! The configuration schema and loader that every Alyosha program shares.
//!
//! All settings live in one strict file, `$XDG_CONFIG_HOME/alyosha/config.toml`,
//! which the owner writes and Alyosha never changes. Reference:
//! `docs/reference/Configuration.md`. Implementation guide:
//! `docs/develop/Configuration.md`.

use std::{
    collections::BTreeMap,
    ffi::OsStr,
    fmt,
    fs::File,
    io::{self, Read},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

use chrono_tz::Tz;

pub mod logging;
use serde::Deserialize;

/// Every setting, with defaults filled in and values checked.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// The owner's timezone, for clock lines, scheduled tasks, and the nightly
    /// fresh session.
    pub timezone: Tz,
    pub model: ModelConfig,
    pub gateway: GatewayConfig,
    pub approvals: ApprovalsConfig,
    /// Present only when the file has a `[telegram]` section.
    pub telegram: Option<TelegramConfig>,
    /// Present only when the file has a `[qq]` section.
    pub qq: Option<QqConfig>,
    pub guest: GuestConfig,
    pub mcp: McpConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            timezone: chrono_tz::Europe::Amsterdam,
            model: ModelConfig::default(),
            gateway: GatewayConfig::default(),
            approvals: ApprovalsConfig::default(),
            telegram: None,
            qq: None,
            guest: GuestConfig::default(),
            mcp: McpConfig::default(),
        }
    }
}

/// `[model]`: the provider account and model.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelConfig {
    /// The GPROXY provider name.
    pub provider: String,
    /// The provider's native API, which decides how requests are encoded.
    pub api: Api,
    /// The exact model ID.
    pub name: String,
    /// How hard the model reasons; `None` keeps the provider's default.
    pub effort: Option<Effort>,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            provider: "chatgpt".to_owned(),
            api: Api::Codex,
            name: "gpt-6.2-sol".to_owned(),
            effort: None,
        }
    }
}

/// `[model] effort`: the reasoning effort sent with every request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    Low,
    Medium,
    High,
}

impl Effort {
    /// The value as the Responses API spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

/// A provider's native API, named after its GPROXY channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Api {
    Codex,
    Openai,
    Claudeapi,
    Aistudio,
    Deepseek,
}

/// `[gateway]`: where the GPROXY service listens.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GatewayConfig {
    pub url: String,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            url: "http://127.0.0.1:18787".to_owned(),
        }
    }
}

/// `[approvals]`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ApprovalsConfig {
    /// How long an approval or `ask_owner` question waits for an answer.
    pub timeout_minutes: u64,
}

impl Default for ApprovalsConfig {
    fn default() -> Self {
        Self {
            timeout_minutes: 60,
        }
    }
}

/// `[telegram]`, needed only by `alyosha-telegram`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelegramConfig {
    /// The owner's numeric Telegram user ID.
    pub owner_id: i64,
    /// The bridge session's workspace, an absolute path.
    pub workspace: PathBuf,
    /// The bot token from @BotFather.
    pub token: Secret,
    /// Group chat IDs whose members get the owner's full powers.
    #[serde(default)]
    pub trusted_groups: Vec<i64>,
}

/// `[qq]`, needed only by `alyosha-qq`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QqConfig {
    pub app_id: String,
    pub secret: Secret,
    /// The owner's `user_openid`. While empty, the bridge answers nobody.
    #[serde(default)]
    pub owner_openid: String,
    pub workspace: PathBuf,
    /// Characters per QQ message.
    #[serde(default = "default_reply_chunk")]
    pub reply_chunk: u32,
    /// `group_openid`s whose members get the owner's full powers.
    #[serde(default)]
    pub trusted_groups: Vec<String>,
}

fn default_reply_chunk() -> u32 {
    1500
}

/// `[guest]`: what guest sessions in group chats may use.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GuestConfig {
    /// The MCP servers a guest session starts, by name.
    pub mcp_servers: Vec<String>,
}

impl McpConfig {
    /// Only the named servers.
    pub fn only(&self, names: &[String]) -> McpConfig {
        McpConfig {
            servers: self
                .servers
                .iter()
                .filter(|(name, _)| names.contains(name))
                .map(|(name, server)| (name.clone(), server.clone()))
                .collect(),
        }
    }
}

/// `[mcp]`: the configured MCP servers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct McpConfig {
    /// Keyed by the server's name, which becomes part of its tool names.
    pub servers: BTreeMap<String, McpServerConfig>,
}

/// `[mcp.servers.<name>]`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerConfig {
    /// The program to start, found on `PATH`.
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// The tools to register. `None` registers every tool.
    #[serde(default)]
    pub tools: Option<Vec<String>>,
    /// Tools that wait for the owner's approval.
    #[serde(default)]
    pub ask: Vec<String>,
    /// Environment variables for the server. They often hold tokens, so each
    /// value is a [`Secret`].
    #[serde(default)]
    pub env: BTreeMap<String, Secret>,
}

/// A secret value. `Debug` and `Display` print `"<secret>"`, so a logged
/// configuration never shows it.
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The secret itself, for the one place that needs it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("\"<secret>\"")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("\"<secret>\"")
    }
}

/// Why the configuration could not be loaded.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("$HOME is not set to an absolute path")]
    NoHome,
    #[error("cannot read {}: {source}", path.display())]
    Io { path: PathBuf, source: io::Error },
    #[error(
        "{path} can be read by other users, but it holds secrets. Run: chmod 600 {path}",
        path = path.display()
    )]
    Mode { path: PathBuf },
    #[error("{}: {message}", path.display())]
    Parse { path: PathBuf, message: String },
    #[error("{}:\n- {}", path.display(), problems.join("\n- "))]
    Invalid {
        path: PathBuf,
        problems: Vec<String>,
    },
}

/// Alyosha's folders on the host, which follow the XDG locations and are not
/// configurable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub home: PathBuf,
    /// `$XDG_CONFIG_HOME/alyosha`: `config.toml`, `gproxy.env`, `skills/`.
    pub config_dir: PathBuf,
    /// `$XDG_DATA_HOME/alyosha`: logs, memory, the gateway's data, caches.
    pub data_dir: PathBuf,
}

impl Paths {
    /// The folders for the current user, from `$HOME` and the XDG variables.
    pub fn from_env() -> Result<Self, ConfigError> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|home| home.is_absolute())
            .ok_or(ConfigError::NoHome)?;
        Ok(Self::new(
            &home,
            std::env::var_os("XDG_CONFIG_HOME").as_deref(),
            std::env::var_os("XDG_DATA_HOME").as_deref(),
        ))
    }

    /// An XDG variable counts only when it is an absolute path.
    pub fn new(
        home: &Path,
        xdg_config_home: Option<&OsStr>,
        xdg_data_home: Option<&OsStr>,
    ) -> Self {
        let pick = |value: Option<&OsStr>, fallback: &str| {
            value
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .unwrap_or_else(|| home.join(fallback))
                .join("alyosha")
        };
        Self {
            home: home.to_owned(),
            config_dir: pick(xdg_config_home, ".config"),
            data_dir: pick(xdg_data_home, ".local/share"),
        }
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }
}

/// Loads the configuration from its standard location.
pub fn load() -> Result<Config, ConfigError> {
    let paths = Paths::from_env()?;
    load_from(&paths.config_file())
}

/// `$XDG_CONFIG_HOME/alyosha/config.toml`, or `$HOME/.config/...` when
/// `XDG_CONFIG_HOME` is unset or not absolute.
pub fn config_path(xdg_config_home: Option<&OsStr>, home: &Path) -> PathBuf {
    let base = xdg_config_home
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| home.join(".config"));
    base.join("alyosha").join("config.toml")
}

/// Loads the file at `path`. A missing file gives the defaults.
pub fn load_from(path: &Path) -> Result<Config, ConfigError> {
    let io_error = |source| ConfigError::Io {
        path: path.to_owned(),
        source,
    };
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(error) => return Err(io_error(error)),
    };
    // Check the mode on the open handle, so the file checked is the file read.
    let mode = file.metadata().map_err(io_error)?.mode();
    if mode & 0o077 != 0 {
        return Err(ConfigError::Mode {
            path: path.to_owned(),
        });
    }
    let mut text = String::new();
    file.read_to_string(&mut text).map_err(io_error)?;
    parse(&text).map_err(|error| match error {
        Problem::Parse(message) => ConfigError::Parse {
            path: path.to_owned(),
            message,
        },
        Problem::Invalid(problems) => ConfigError::Invalid {
            path: path.to_owned(),
            problems,
        },
    })
}

enum Problem {
    Parse(String),
    Invalid(Vec<String>),
}

/// The file as written, before the checks the types cannot make.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    #[serde(default = "default_timezone")]
    timezone: String,
    #[serde(default)]
    model: ModelConfig,
    #[serde(default)]
    gateway: GatewayConfig,
    #[serde(default)]
    approvals: ApprovalsConfig,
    telegram: Option<TelegramConfig>,
    qq: Option<QqConfig>,
    #[serde(default)]
    guest: GuestConfig,
    #[serde(default)]
    mcp: McpConfig,
}

fn default_timezone() -> String {
    "Europe/Amsterdam".to_owned()
}

fn parse(text: &str) -> Result<Config, Problem> {
    let file: ConfigFile =
        toml::from_str(text).map_err(|error| Problem::Parse(error.to_string()))?;
    let mut problems = Vec::new();

    let timezone = file.timezone.parse::<Tz>().unwrap_or_else(|_| {
        problems.push(format!(
            "timezone: {:?} is not a known IANA timezone name",
            file.timezone
        ));
        chrono_tz::Europe::Amsterdam
    });
    if file.model.effort.is_some() && !matches!(file.model.api, Api::Codex | Api::Openai) {
        problems.push("model.effort: only the codex and openai APIs support it for now".to_owned());
    }
    if !is_http_url(&file.gateway.url) {
        problems.push(format!(
            "gateway.url: {:?} is not an http:// URL",
            file.gateway.url
        ));
    }
    if file.approvals.timeout_minutes < 1 {
        problems.push("approvals.timeout_minutes: must be at least 1".to_owned());
    }
    if let Some(telegram) = &file.telegram {
        check_workspace("telegram.workspace", &telegram.workspace, &mut problems);
    }
    if let Some(qq) = &file.qq {
        check_workspace("qq.workspace", &qq.workspace, &mut problems);
        if !(200..=4000).contains(&qq.reply_chunk) {
            problems.push(format!(
                "qq.reply_chunk: {} is not between 200 and 4000",
                qq.reply_chunk
            ));
        }
    }
    for (name, server) in &file.mcp.servers {
        if !is_server_name(name) {
            problems.push(format!(
                "mcp.servers.{name}: a server name must match [a-z0-9][a-z0-9-]{{0,31}}"
            ));
        }
        if let Some(tools) = &server.tools {
            for asked in server.ask.iter().filter(|asked| !tools.contains(asked)) {
                problems.push(format!(
                    "mcp.servers.{name}.ask: {asked:?} is not in this server's tools"
                ));
            }
        }
    }

    for name in &file.guest.mcp_servers {
        match file.mcp.servers.get(name) {
            None => problems.push(format!(
                "guest.mcp_servers: {name:?} is not a configured [mcp.servers.{name}]"
            )),
            Some(server) if !server.ask.is_empty() => problems.push(format!(
                "guest.mcp_servers: {name:?} has tools in ask, and nobody in a group can approve them"
            )),
            Some(_) => {}
        }
    }

    if !problems.is_empty() {
        return Err(Problem::Invalid(problems));
    }
    Ok(Config {
        timezone,
        model: file.model,
        gateway: file.gateway,
        approvals: file.approvals,
        telegram: file.telegram,
        qq: file.qq,
        guest: file.guest,
        mcp: file.mcp,
    })
}

fn is_http_url(url: &str) -> bool {
    url.strip_prefix("http://").is_some_and(|rest| {
        let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
        !host.is_empty() && !rest.contains(char::is_whitespace)
    })
}

fn check_workspace(key: &str, path: &Path, problems: &mut Vec<String>) {
    if !path.is_absolute() {
        problems.push(format!("{key}: {} is not an absolute path", path.display()));
    }
}

fn is_server_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    (1..=32).contains(&bytes.len())
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
        && bytes[0] != b'-'
}

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::fs::PermissionsExt};

    use super::*;

    const HOME: &str = "/home/mjli";

    /// The reference's example, with secrets filled in.
    const EXAMPLE: &str = r#"
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
workspace = "/home/mjli/Projects/alyosha-home"
token = "telegram-token-1234"
trusted_groups = [-1001234567890]

[qq]
app_id = "102000000"
owner_openid = "openid-5678"
workspace = "/home/mjli/Projects/alyosha-qq"
secret = "qq-secret-9012"

[guest]
mcp_servers = ["search"]

[mcp.servers.search]
command = "open-websearch"
tools = ["search"]

[mcp.servers.search.env]
MODE = "stdio"
API_TOKEN = "mcp-token-3456"
"#;

    /// Writes `text` as a configuration file with the given mode.
    fn write(text: &str, mode: u32) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, text).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        (dir, path)
    }

    fn load_text(text: &str) -> Result<Config, ConfigError> {
        let (_dir, path) = write(text, 0o600);
        load_from(&path)
    }

    #[test]
    fn the_path_follows_xdg() {
        let home = Path::new(HOME);
        assert_eq!(
            config_path(None, home),
            Path::new("/home/mjli/.config/alyosha/config.toml")
        );
        assert_eq!(
            config_path(Some(OsStr::new("/xdg")), home),
            Path::new("/xdg/alyosha/config.toml")
        );
        assert_eq!(
            config_path(Some(OsStr::new("relative")), home),
            Path::new("/home/mjli/.config/alyosha/config.toml")
        );
    }

    #[test]
    fn a_missing_file_gives_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let config = load_from(&dir.path().join("config.toml")).unwrap();
        assert_eq!(config, Config::default());
        assert_eq!(config.timezone, chrono_tz::Europe::Amsterdam);
        assert_eq!(config.model.provider, "chatgpt");
        assert_eq!(config.model.api, Api::Codex);
        assert_eq!(config.model.name, "gpt-6.2-sol");
        assert_eq!(config.gateway.url, "http://127.0.0.1:18787");
        assert_eq!(config.approvals.timeout_minutes, 60);
        assert!(config.telegram.is_none() && config.qq.is_none());
        assert!(config.mcp.servers.is_empty());
        // An empty file is the same as a missing one.
        assert_eq!(load_text("").unwrap(), Config::default());
    }

    #[test]
    fn the_example_loads() {
        let config = load_text(EXAMPLE).unwrap();
        let telegram = config.telegram.as_ref().unwrap();
        assert_eq!(telegram.owner_id, 123456789);
        assert_eq!(
            telegram.workspace,
            Path::new("/home/mjli/Projects/alyosha-home")
        );
        assert_eq!(telegram.token.expose(), "telegram-token-1234");
        assert_eq!(telegram.trusted_groups, [-1001234567890]);
        let qq = config.qq.as_ref().unwrap();
        assert_eq!(qq.app_id, "102000000");
        assert_eq!(qq.owner_openid, "openid-5678");
        assert_eq!(qq.secret.expose(), "qq-secret-9012");
        assert_eq!(qq.reply_chunk, 1500);
        assert!(qq.trusted_groups.is_empty());
        let search = &config.mcp.servers["search"];
        assert_eq!(search.command, "open-websearch");
        assert!(search.args.is_empty());
        assert_eq!(search.tools, Some(vec!["search".to_owned()]));
        assert!(search.ask.is_empty());
        assert_eq!(search.env["MODE"].expose(), "stdio");
        assert_eq!(config.approvals.timeout_minutes, 60);
    }

    #[test]
    fn a_file_others_can_read_is_refused() {
        let (_dir, path) = write(EXAMPLE, 0o640);
        let error = load_from(&path).unwrap_err();
        assert!(matches!(error, ConfigError::Mode { .. }));
        let message = error.to_string();
        assert!(
            message.contains(&format!("chmod 600 {}", path.display())),
            "{message}"
        );
    }

    #[test]
    fn an_unknown_key_names_its_line_and_key() {
        let error =
            load_text("[mcp.servers.search]\ncommand = \"x\"\nasks = [\"send\"]\n").unwrap_err();
        let message = error.to_string();
        assert!(matches!(error, ConfigError::Parse { .. }), "{message}");
        assert!(message.contains("line 3"), "{message}");
        assert!(message.contains("asks"), "{message}");
        assert!(load_text("[modle]\nname = \"x\"\n").is_err());
        assert!(load_text("[approvals]\ntimeout_minutes = \"60\"\n").is_err());
    }

    #[test]
    fn every_failed_check_is_reported_at_once() {
        let error = load_text(
            r#"
timezone = "Europe/Amstredam"
[approvals]
timeout_minutes = 0
[telegram]
owner_id = 1
workspace = "bot"
token = "t"
"#,
        )
        .unwrap_err();
        let ConfigError::Invalid { problems, .. } = &error else {
            panic!("expected invalid values, got {error}")
        };
        assert_eq!(problems.len(), 3, "{problems:?}");
        assert!(problems[0].contains("Europe/Amstredam"));
        assert!(problems[1].contains("timeout_minutes"));
        assert!(problems[2].contains("telegram.workspace"));
    }

    #[test]
    fn other_values_are_checked() {
        for (text, needle) in [
            ("[gateway]\nurl = \"https://example.com\"\n", "gateway.url"),
            ("[gateway]\nurl = \"http://\"\n", "gateway.url"),
            (
                "[qq]\napp_id = \"1\"\nsecret = \"s\"\nworkspace = \"/home/mjli/Projects/q\"\nreply_chunk = 100\n",
                "reply_chunk",
            ),
            (
                "[qq]\napp_id = \"1\"\nsecret = \"s\"\nworkspace = \"relative/x\"\n",
                "qq.workspace",
            ),
            ("[mcp.servers.Search]\ncommand = \"x\"\n", "server name"),
            ("[mcp.servers.-x]\ncommand = \"x\"\n", "server name"),
        ] {
            let message = load_text(text).unwrap_err().to_string();
            assert!(message.contains(needle), "{text}: {message}");
        }
    }

    #[test]
    fn an_ask_entry_must_be_among_the_tools() {
        let message = load_text(
            "[mcp.servers.google]\ncommand = \"gog\"\ntools = [\"search\"]\nask = [\"send\"]\n",
        )
        .unwrap_err()
        .to_string();
        assert!(message.contains("mcp.servers.google.ask"), "{message}");
        assert!(message.contains("\"send\""), "{message}");
        // Without `tools`, every tool is registered, so any `ask` entry is allowed.
        load_text("[mcp.servers.google]\ncommand = \"gog\"\nask = [\"send\"]\n").unwrap();
    }

    #[test]
    fn effort_is_optional_and_only_for_the_responses_apis() {
        assert_eq!(load_text("").unwrap().model.effort, None);
        let config = load_text("[model]\neffort = \"high\"\n").unwrap();
        assert_eq!(config.model.effort, Some(Effort::High));
        let message = load_text("[model]\napi = \"claudeapi\"\neffort = \"low\"\n")
            .unwrap_err()
            .to_string();
        assert!(message.contains("model.effort"), "{message}");
        assert!(load_text("[model]\neffort = \"extreme\"\n").is_err());
    }

    #[test]
    fn guest_servers_must_exist_and_need_no_approval() {
        let message = load_text("[guest]\nmcp_servers = [\"search\"]\n")
            .unwrap_err()
            .to_string();
        assert!(message.contains("guest.mcp_servers"), "{message}");
        let message = load_text(
            "[guest]\nmcp_servers = [\"google\"]\n[mcp.servers.google]\ncommand = \"gog\"\nask = [\"send\"]\n",
        )
        .unwrap_err()
        .to_string();
        assert!(
            message.contains("nobody in a group can approve"),
            "{message}"
        );
        let config = load_text(EXAMPLE).unwrap();
        assert_eq!(config.guest.mcp_servers, ["search"]);
        let only = config.mcp.only(&config.guest.mcp_servers);
        assert_eq!(only.servers.keys().collect::<Vec<_>>(), ["search"]);
        assert!(config.mcp.only(&[]).servers.is_empty());
    }

    #[test]
    fn debug_output_hides_secrets() {
        let printed = format!("{:?}", load_text(EXAMPLE).unwrap());
        for secret in ["telegram-token-1234", "qq-secret-9012", "mcp-token-3456"] {
            assert!(!printed.contains(secret), "{secret} leaked: {printed}");
        }
        assert!(printed.contains("<secret>"));
    }
}
