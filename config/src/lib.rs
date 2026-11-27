//! The configuration schema and loader that every Alyosha program shares.
//!
//! All settings live in one strict file, `/etc/alyosha/config.toml`, which
//! the container module generates from the owner's Nix settings and Alyosha
//! never changes. It holds no secrets: each secret is a file of its own in
//! the secrets folder. Reference: `docs/reference/Configuration.md`.

use std::{
    collections::{BTreeMap, HashMap},
    ffi::OsString,
    fmt, fs,
    io::{self},
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
    pub media: MediaConfig,
    pub recall: RecallConfig,
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
            media: MediaConfig::default(),
            recall: RecallConfig::default(),
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
    /// Whether the model can see images.
    pub images: bool,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            provider: "chatgpt".to_owned(),
            api: Api::Codex,
            name: "gpt-6.2-sol".to_owned(),
            effort: None,
            images: true,
        }
    }
}

/// `[media]`: how voice messages and the sound of videos are transcribed.
/// See `docs/Media.md#transcription`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MediaConfig {
    /// The GPROXY provider, on the `openai` channel, that transcribes.
    /// Without it, voice messages are not transcribed.
    pub transcribe_provider: Option<String>,
    /// The transcription model.
    pub transcribe_model: String,
}

impl Default for MediaConfig {
    fn default() -> Self {
        Self {
            transcribe_provider: None,
            transcribe_model: "gpt-transcribe".to_owned(),
        }
    }
}

/// `[recall]`: how past sessions are searched by meaning.
/// See `docs/Recall.md#configuration`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RecallConfig {
    /// The GPROXY provider, on the `openai` channel, that computes vectors.
    /// Without it, recall searches only by the words.
    pub embed_provider: Option<String>,
    /// The embedding model, whose vectors have 1,536 dimensions.
    pub embed_model: String,
}

impl Default for RecallConfig {
    fn default() -> Self {
        Self {
            embed_provider: None,
            embed_model: "text-embedding-3-small".to_owned(),
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

/// `[telegram]`, needed only by `alyosha-telegram`, whose token is the
/// secret `telegram-token`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelegramConfig {
    /// The owner's numeric Telegram user ID.
    pub owner_id: i64,
    /// Groups whose members get the owner's full powers, as name → chat ID.
    /// The name is the group's working folder.
    #[serde(default)]
    pub trusted_groups: BTreeMap<String, i64>,
}

/// `[qq]`, needed only by `alyosha-qq`, whose AppSecret is the secret
/// `qq-secret`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QqConfig {
    pub app_id: String,
    /// The owner's `user_openid`. While empty, the bridge answers nobody.
    #[serde(default)]
    pub owner_openid: String,
    /// Characters per QQ message.
    #[serde(default = "default_reply_chunk")]
    pub reply_chunk: u32,
    /// Groups whose members get the owner's full powers, as name →
    /// `group_openid`. The name is the group's working folder.
    #[serde(default)]
    pub trusted_groups: BTreeMap<String, String>,
}

/// The secret holding the Telegram bot token.
pub const TELEGRAM_TOKEN: &str = "telegram-token";
/// The secret holding the QQ bot's AppSecret.
pub const QQ_SECRET: &str = "qq-secret";
/// The secret holding the gateway key Alyosha sends to GPROXY.
pub const GATEWAY_KEY: &str = "gateway-key";
/// The secret holding GPROXY's own environment: its master key and admin keys.
pub const GPROXY_ENV: &str = "gproxy.env";

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
    /// Environment variables for the server. The settings are world-readable,
    /// so these hold no secrets.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Environment variables whose values are secrets: variable → secret name.
    #[serde(default)]
    pub secret_env: BTreeMap<String, String>,
}

/// A secret value. `Debug` and `Display` print `"<secret>"`, so a logged
/// value never shows it.
#[derive(Clone, PartialEq, Eq)]
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

/// Why the configuration or a secret could not be loaded.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{name} must be an absolute path, not {value:?}")]
    RelativePath { name: &'static str, value: OsString },
    #[error("cannot read {}: {source}", path.display())]
    Io { path: PathBuf, source: io::Error },
    #[error("{}: {message}", path.display())]
    Parse { path: PathBuf, message: String },
    #[error("{}:\n- {}", path.display(), problems.join("\n- "))]
    Invalid {
        path: PathBuf,
        problems: Vec<String>,
    },
    #[error("the secret {name} is not set. On the host, run: alyosha secret set {name}")]
    MissingSecret { name: String },
    #[error("{name:?} is not a valid secret name")]
    BadSecretName { name: String },
}

/// Alyosha's folders. Each is a fixed path in the container, which an
/// environment variable replaces for tests and development.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    /// `/etc/alyosha/config.toml`, or `ALYOSHA_CONFIG`.
    pub config_file: PathBuf,
    /// `/var/lib/alyosha-secrets`, or `ALYOSHA_SECRETS`.
    pub secrets_dir: PathBuf,
    /// `/var/lib/alyosha`, or `ALYOSHA_STATE`: the agent's home and state.
    pub state_dir: PathBuf,
    /// `/var/cache/alyosha`, or `ALYOSHA_CACHE`.
    pub cache_dir: PathBuf,
}

/// The variables that replace the fixed paths, in the order of [`Paths`].
pub const PATH_VARIABLES: [&str; 4] = [
    "ALYOSHA_CONFIG",
    "ALYOSHA_SECRETS",
    "ALYOSHA_STATE",
    "ALYOSHA_CACHE",
];

impl Paths {
    /// The fixed paths, each replaced by its variable when set.
    pub fn from_env() -> Result<Self, ConfigError> {
        let pick = |name: &'static str, fixed: &str| match std::env::var_os(name) {
            None => Ok(PathBuf::from(fixed)),
            Some(value) if Path::new(&value).is_absolute() => Ok(PathBuf::from(value)),
            Some(value) => Err(ConfigError::RelativePath { name, value }),
        };
        Ok(Self {
            config_file: pick(PATH_VARIABLES[0], "/etc/alyosha/config.toml")?,
            secrets_dir: pick(PATH_VARIABLES[1], "/var/lib/alyosha-secrets")?,
            state_dir: pick(PATH_VARIABLES[2], "/var/lib/alyosha")?,
            cache_dir: pick(PATH_VARIABLES[3], "/var/cache/alyosha")?,
        })
    }

    /// Every folder under one root, for tests: `config.toml`, `secrets/`,
    /// `state/`, and `cache/`.
    pub fn under(root: &Path) -> Self {
        Self {
            config_file: root.join("config.toml"),
            secrets_dir: root.join("secrets"),
            state_dir: root.join("state"),
            cache_dir: root.join("cache"),
        }
    }

    /// The variables that give a child process these same paths.
    pub fn variables(&self) -> [(&'static str, &Path); 4] {
        [
            (PATH_VARIABLES[0], &self.config_file),
            (PATH_VARIABLES[1], &self.secrets_dir),
            (PATH_VARIABLES[2], &self.state_dir),
            (PATH_VARIABLES[3], &self.cache_dir),
        ]
    }

    /// The sessions' working folders: `owner/`, `groups/<name>/`, `guest/`.
    pub fn work_dir(&self) -> PathBuf {
        self.state_dir.join("work")
    }

    /// The working folder of the owner's private chats.
    pub fn owner_work(&self) -> PathBuf {
        self.work_dir().join("owner")
    }

    /// The working folder of a trusted group, by its name in the settings.
    pub fn group_work(&self, name: &str) -> PathBuf {
        self.work_dir().join("groups").join(name)
    }

    /// The working directory of guest sessions' MCP servers.
    pub fn guest_work(&self) -> PathBuf {
        self.work_dir().join("guest")
    }

    /// The skills directory, mounted read-only from the host's `~/.agents/skills`.
    pub fn skills_dir(&self) -> PathBuf {
        self.state_dir.join(".agents").join("skills")
    }

    /// The secrets in the secrets folder.
    pub fn secrets(&self) -> Secrets {
        Secrets {
            dir: self.secrets_dir.clone(),
        }
    }
}

/// The secrets folder: one file per secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Secrets {
    pub dir: PathBuf,
}

impl Secrets {
    /// The file that holds `name`.
    pub fn path(&self, name: &str) -> Result<PathBuf, ConfigError> {
        if !is_secret_name(name) {
            return Err(ConfigError::BadSecretName {
                name: name.to_owned(),
            });
        }
        Ok(self.dir.join(name))
    }

    /// Reads a secret, without the final line break a text editor adds.
    pub fn read(&self, name: &str) -> Result<Secret, ConfigError> {
        let path = self.path(name)?;
        match fs::read_to_string(&path) {
            Ok(text) => Ok(Secret(text.trim_end_matches(['\n', '\r']).to_owned())),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                Err(ConfigError::MissingSecret {
                    name: name.to_owned(),
                })
            }
            Err(source) => Err(ConfigError::Io { path, source }),
        }
    }

    /// Whether a secret is set.
    pub fn exists(&self, name: &str) -> bool {
        self.path(name).is_ok_and(|path| path.exists())
    }
}

/// A secret's name: `[a-z0-9][a-z0-9._-]{0,63}`.
pub fn is_secret_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    (1..=64).contains(&bytes.len())
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes.iter().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')
        })
}

/// A trusted group's name, which is also its folder: `[a-z0-9][a-z0-9_-]{0,31}`.
pub fn is_group_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    (1..=32).contains(&bytes.len())
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-'))
}

/// Loads the configuration from its standard location.
pub fn load() -> Result<Config, ConfigError> {
    load_from(&Paths::from_env()?.config_file)
}

/// Loads the file at `path`. A missing file gives the defaults.
pub fn load_from(path: &Path) -> Result<Config, ConfigError> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(source) => {
            return Err(ConfigError::Io {
                path: path.to_owned(),
                source,
            });
        }
    };
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
    #[serde(default)]
    media: MediaConfig,
    #[serde(default)]
    recall: RecallConfig,
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
    if file
        .media
        .transcribe_provider
        .as_deref()
        .is_some_and(|provider| provider.trim().is_empty())
    {
        problems.push("media.transcribe_provider: must not be empty".to_owned());
    }
    if file.media.transcribe_model.trim().is_empty() {
        problems.push("media.transcribe_model: must not be empty".to_owned());
    }
    if file
        .recall
        .embed_provider
        .as_deref()
        .is_some_and(|provider| provider.trim().is_empty())
    {
        problems.push("recall.embed_provider: must not be empty".to_owned());
    }
    if file.recall.embed_model.trim().is_empty() {
        problems.push("recall.embed_model: must not be empty".to_owned());
    }
    if file.approvals.timeout_minutes < 1 {
        problems.push("approvals.timeout_minutes: must be at least 1".to_owned());
    }
    let mut group_names: HashMap<&str, &str> = HashMap::new();
    if let Some(telegram) = &file.telegram {
        let mut ids = HashMap::new();
        for (name, id) in &telegram.trusted_groups {
            check_group("telegram", name, &mut group_names, &mut problems);
            if let Some(other) = ids.insert(*id, name) {
                problems.push(format!(
                    "telegram.trusted_groups: {other} and {name} have the same chat ID"
                ));
            }
        }
    }
    if let Some(qq) = &file.qq {
        let mut ids = HashMap::new();
        for (name, id) in &qq.trusted_groups {
            check_group("qq", name, &mut group_names, &mut problems);
            if let Some(other) = ids.insert(id.as_str(), name) {
                problems.push(format!(
                    "qq.trusted_groups: {other} and {name} have the same group_openid"
                ));
            }
        }
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
        for (variable, secret) in &server.secret_env {
            if !is_secret_name(secret) {
                problems.push(format!(
                    "mcp.servers.{name}.secret_env.{variable}: {secret:?} is not a valid secret name"
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
        media: file.media,
        recall: file.recall,
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

/// A trusted group's name must be valid and used once across both bridges,
/// because it names the group's working folder.
fn check_group<'a>(
    bridge: &'a str,
    name: &'a str,
    seen: &mut HashMap<&'a str, &'a str>,
    problems: &mut Vec<String>,
) {
    if !is_group_name(name) {
        problems.push(format!(
            "{bridge}.trusted_groups: {name:?} must match [a-z0-9][a-z0-9_-]{{0,31}}"
        ));
    }
    if let Some(other) = seen.insert(name, bridge) {
        problems.push(format!(
            "{bridge}.trusted_groups: {name:?} is also a group of {other}; each name is a folder and must be unique"
        ));
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
    use super::*;

    /// The reference's example.
    const EXAMPLE: &str = r#"
timezone = "Europe/Amsterdam"

[model]
provider = "chatgpt"
api = "codex"
name = "gpt-6.2-sol"

[gateway]
url = "http://127.0.0.1:18787"

[media]
transcribe_provider = "openai"

[recall]
embed_provider = "openai"

[approvals]
timeout_minutes = 60

[telegram]
owner_id = 123456789

[telegram.trusted_groups]
family = -1001234567890

[qq]
app_id = "102000000"
owner_openid = "openid-5678"

[qq.trusted_groups]
friends = "12D5F61BED16A072D96DB3D9E04A7B3D"

[guest]
mcp_servers = ["search"]

[mcp.servers.search]
command = "open-websearch"
tools = ["search"]

[mcp.servers.search.env]
MODE = "stdio"

[mcp.servers.search.secret_env]
API_TOKEN = "search-token"
"#;

    fn load_text(text: &str) -> Result<Config, ConfigError> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, text).unwrap();
        load_from(&path)
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
        assert!(config.model.images);
        assert_eq!(config.media.transcribe_provider, None);
        assert_eq!(config.media.transcribe_model, "gpt-transcribe");
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
        assert_eq!(telegram.trusted_groups["family"], -1001234567890);
        let qq = config.qq.as_ref().unwrap();
        assert_eq!(qq.app_id, "102000000");
        assert_eq!(qq.owner_openid, "openid-5678");
        assert_eq!(qq.reply_chunk, 1500);
        assert_eq!(
            qq.trusted_groups["friends"],
            "12D5F61BED16A072D96DB3D9E04A7B3D"
        );
        let search = &config.mcp.servers["search"];
        assert_eq!(search.command, "open-websearch");
        assert!(search.args.is_empty());
        assert_eq!(search.tools, Some(vec!["search".to_owned()]));
        assert!(search.ask.is_empty());
        assert_eq!(search.env["MODE"], "stdio");
        assert_eq!(search.secret_env["API_TOKEN"], "search-token");
        assert_eq!(config.approvals.timeout_minutes, 60);
        assert_eq!(config.media.transcribe_provider.as_deref(), Some("openai"));
        assert_eq!(config.recall.embed_provider.as_deref(), Some("openai"));
        assert_eq!(config.recall.embed_model, "text-embedding-3-small");
    }

    #[test]
    fn recall_settings_are_checked() {
        let config = load_text("").unwrap();
        assert_eq!(config.recall.embed_provider, None);
        for (text, key) in [
            (
                "[recall]\nembed_provider = \" \"\n",
                "recall.embed_provider",
            ),
            ("[recall]\nembed_model = \"\"\n", "recall.embed_model"),
        ] {
            let error = load_text(text).unwrap_err().to_string();
            assert!(error.contains(key), "{error}");
        }
        assert!(load_text("[recall]\nembed = \"x\"\n").is_err());
    }

    #[test]
    fn media_settings_are_checked() {
        let config = load_text("[model]\nimages = false\n[media]\ntranscribe_model = \"whisper-1\"\n").unwrap();
        assert!(!config.model.images);
        assert_eq!(config.media.transcribe_model, "whisper-1");
        let error = load_text("[media]\ntranscribe_provider = \" \"\n")
            .unwrap_err()
            .to_string();
        assert!(error.contains("media.transcribe_provider"), "{error}");
        assert!(load_text("[media]\nlanguage = \"en\"\n").is_err());
    }

    #[test]
    fn secrets_are_not_settings() {
        for text in [
            "[telegram]\nowner_id = 1\ntoken = \"t\"\n",
            "[qq]\napp_id = \"1\"\nsecret = \"s\"\n",
        ] {
            let error = load_text(text).unwrap_err();
            assert!(
                matches!(error, ConfigError::Parse { .. }),
                "{text}: {error}"
            );
        }
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
[telegram.trusted_groups]
"My Group" = -100
"#,
        )
        .unwrap_err();
        let ConfigError::Invalid { problems, .. } = &error else {
            panic!("expected invalid values, got {error}")
        };
        assert_eq!(problems.len(), 3, "{problems:?}");
        assert!(problems[0].contains("Europe/Amstredam"));
        assert!(problems[1].contains("timeout_minutes"));
        assert!(problems[2].contains("My Group"));
    }

    #[test]
    fn group_names_are_unique_across_both_bridges() {
        let message = load_text(
            "[telegram]\nowner_id = 1\n[telegram.trusted_groups]\nfriends = -1\n[qq]\napp_id = \"1\"\n[qq.trusted_groups]\nfriends = \"G\"\n",
        )
        .unwrap_err()
        .to_string();
        assert!(message.contains("\"friends\""), "{message}");
        assert!(message.contains("telegram"), "{message}");
        let message =
            load_text("[qq]\napp_id = \"1\"\n[qq.trusted_groups]\na = \"G\"\nb = \"G\"\n")
                .unwrap_err()
                .to_string();
        assert!(message.contains("same group_openid"), "{message}");
    }

    #[test]
    fn other_values_are_checked() {
        for (text, needle) in [
            ("[gateway]\nurl = \"https://example.com\"\n", "gateway.url"),
            ("[gateway]\nurl = \"http://\"\n", "gateway.url"),
            ("[qq]\napp_id = \"1\"\nreply_chunk = 100\n", "reply_chunk"),
            ("[mcp.servers.Search]\ncommand = \"x\"\n", "server name"),
            ("[mcp.servers.-x]\ncommand = \"x\"\n", "server name"),
            (
                "[mcp.servers.x]\ncommand = \"x\"\n[mcp.servers.x.secret_env]\nA = \"../k\"\n",
                "secret_env.A",
            ),
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
    fn paths_and_their_folders() {
        let paths = Paths::under(Path::new("/r"));
        assert_eq!(paths.config_file, Path::new("/r/config.toml"));
        assert_eq!(paths.owner_work(), Path::new("/r/state/work/owner"));
        assert_eq!(
            paths.group_work("friends"),
            Path::new("/r/state/work/groups/friends")
        );
        assert_eq!(paths.guest_work(), Path::new("/r/state/work/guest"));
        assert_eq!(paths.skills_dir(), Path::new("/r/state/.agents/skills"));
    }

    #[test]
    fn secrets_are_files_and_never_printed() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = Secrets {
            dir: dir.path().to_owned(),
        };
        fs::write(dir.path().join("telegram-token"), "tok-1234\n").unwrap();
        let token = secrets.read("telegram-token").unwrap();
        assert_eq!(token.expose(), "tok-1234");
        assert!(!format!("{token:?} {token}").contains("tok-1234"));
        let message = secrets.read("qq-secret").unwrap_err().to_string();
        assert!(
            message.contains("qq-secret") && message.contains("alyosha secret set"),
            "{message}"
        );
        assert!(matches!(
            secrets.read("../telegram-token"),
            Err(ConfigError::BadSecretName { .. })
        ));
        assert!(secrets.exists("telegram-token") && !secrets.exists("qq-secret"));
        assert!(is_secret_name("browser-state.json") && !is_secret_name("A"));
        assert!(is_group_name("friends_1") && !is_group_name("My Group"));
    }
}
