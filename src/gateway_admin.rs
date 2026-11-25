//! `alyosha gateway init` and `alyosha gateway login`: setting up GPROXY and
//! the provider through its admin API.
//!
//! Sessions never sign in. These two commands are the only code that uses the
//! admin key. See `docs/develop/LLMClientGateway.md#run-gproxy-as-a-service`.

use std::{
    fs::{self, DirBuilder, OpenOptions},
    io::{self, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    time::Duration,
};

use alyosha_config::{Api, Config, Paths};
use reqwest::Method;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::llm_client::{GatewayClient, ModelClient, ModelRequest, Timeouts};

/// The variable `gateway login` adds to `gproxy.env`. GPROXY ignores it.
pub const GATEWAY_KEY_VAR: &str = "ALYOSHA_GATEWAY_KEY";

/// Where the gateway's secrets and data live.
#[derive(Debug, Clone)]
pub struct GatewayFiles {
    /// `$XDG_CONFIG_HOME/alyosha/gproxy.env`, mode 600.
    pub env_file: PathBuf,
    /// `$XDG_DATA_HOME/alyosha/gproxy`.
    pub data_dir: PathBuf,
}

impl GatewayFiles {
    pub fn new(paths: &Paths) -> Self {
        Self {
            env_file: paths.config_dir.join("gproxy.env"),
            data_dir: paths.data_dir.join("gproxy"),
        }
    }

    /// The variables in `gproxy.env`.
    pub fn read_env(&self) -> io::Result<Vec<(String, String)>> {
        Ok(parse_env(&fs::read_to_string(&self.env_file)?))
    }

    /// The gateway key every session sends, if `gateway login` created one.
    pub fn gateway_key(&self) -> io::Result<Option<String>> {
        Ok(lookup(&self.read_env()?, GATEWAY_KEY_VAR))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum GatewayError {
    #[error(
        "{} already exists. A new master key cannot open the existing database. To start over, move the old data folder {} away first.",
        .0.env_file.display(),
        .0.data_dir.display()
    )]
    AlreadyInitialized(Box<GatewayFiles>),
    #[error("{} is missing. Run `alyosha gateway init` and start the gproxy service first.", .0.display())]
    NotInitialized(PathBuf),
    #[error("{0}")]
    Io(#[from] io::Error),
    #[error("the gateway admin API failed: {0}")]
    Admin(String),
    #[error("sign-in did not finish: {0}")]
    Login(String),
}

/// Creates the data folder and `gproxy.env` with a fresh master key and the
/// bootstrap admin credentials. Refuses if the file exists.
pub fn init(files: &GatewayFiles) -> Result<String, GatewayError> {
    if files.env_file.exists() {
        return Err(GatewayError::AlreadyInitialized(Box::new(files.clone())));
    }
    if let Some(parent) = files.env_file.parent() {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)?;
    }
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&files.data_dir)?;
    let text = format!(
        "GPROXY_MASTER_KEY={}\nGPROXY_ADMIN_PASSWORD={}\nGPROXY_BOOTSTRAP_ADMIN_API_KEY={}\n",
        random_hex(32)?,
        random_hex(24)?,
        random_hex(32)?,
    );
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&files.env_file)
        .map_err(|error| match error.kind() {
            io::ErrorKind::AlreadyExists => {
                GatewayError::AlreadyInitialized(Box::new(files.clone()))
            }
            _ => GatewayError::Io(error),
        })?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    Ok(format!(
        "Wrote {} and created {}. Start the gproxy service, then run `alyosha gateway login`.",
        files.env_file.display(),
        files.data_dir.display()
    ))
}

fn random_hex(bytes: usize) -> io::Result<String> {
    let mut buffer = vec![0u8; bytes];
    getrandom::fill(&mut buffer).map_err(|error| io::Error::other(error.to_string()))?;
    Ok(buffer.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn parse_env(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let (name, value) = line.split_once('=')?;
            Some((name.trim().to_owned(), value.trim().to_owned()))
        })
        .collect()
}

fn lookup(variables: &[(String, String)], name: &str) -> Option<String> {
    variables
        .iter()
        .rev()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.clone())
        .filter(|value| !value.is_empty())
}

/// What `gateway login` needs from the terminal.
pub trait LoginTerminal {
    fn say(&mut self, text: &str);
    /// Reads a line without echoing it.
    fn read_secret(&mut self, prompt: &str) -> io::Result<String>;
}

/// The real terminal.
pub struct StdTerminal;

impl LoginTerminal for StdTerminal {
    fn say(&mut self, text: &str) {
        println!("{text}");
    }

    fn read_secret(&mut self, prompt: &str) -> io::Result<String> {
        rpassword::prompt_password(prompt)
    }
}

/// Creates the gateway key and the provider when missing, signs in, replaces
/// the provider's old credentials, and sends one small request.
pub async fn login(
    config: &Config,
    files: &GatewayFiles,
    terminal: &mut dyn LoginTerminal,
) -> Result<(), GatewayError> {
    if !files.env_file.exists() {
        return Err(GatewayError::NotInitialized(files.env_file.clone()));
    }
    let variables = files.read_env()?;
    let admin_key = lookup(&variables, "GPROXY_BOOTSTRAP_ADMIN_API_KEY").ok_or_else(|| {
        GatewayError::Admin(format!(
            "{} has no GPROXY_BOOTSTRAP_ADMIN_API_KEY",
            files.env_file.display()
        ))
    })?;
    let admin = Admin {
        http: reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .build()
            .expect("valid client settings"),
        base: config.gateway.url.trim_end_matches('/').to_owned(),
        key: admin_key,
    };

    let gateway_key = match lookup(&variables, GATEWAY_KEY_VAR) {
        Some(key) => key,
        None => {
            let admin_user =
                lookup(&variables, "GPROXY_ADMIN_USER").unwrap_or_else(|| "admin".into());
            let key = admin.create_gateway_key(&admin_user).await?;
            append_variable(&files.env_file, GATEWAY_KEY_VAR, &key)?;
            terminal.say("Created the gateway key Alyosha uses.");
            key
        }
    };

    let provider = &config.model.provider;
    let channel = channel(config.model.api);
    let provider_id = match admin.find_provider(provider).await? {
        Some(id) => id,
        None => {
            let id = admin.create_provider(provider, channel).await?;
            terminal.say(&format!("Created the provider {provider} ({channel})."));
            id
        }
    };
    admin.disable_body_capture().await?;
    let old_credentials = admin.credentials(&provider_id).await?;

    let credential_id = if config.model.api == Api::Codex {
        admin.device_login(&provider_id, terminal).await?
    } else {
        let key = terminal.read_secret(&format!("API key for {provider}: "))?;
        let key = key.trim();
        if key.is_empty() {
            return Err(GatewayError::Login("no key was entered".into()));
        }
        admin.add_api_key(&provider_id, key).await?
    };
    for old in old_credentials.iter().filter(|id| **id != credential_id) {
        admin.delete_credential(old).await?;
    }
    terminal.say(&format!("Signed in {provider}."));

    let client = GatewayClient::new(
        &config.gateway.url,
        &gateway_key,
        config.model.api,
        Timeouts::default(),
    );
    let request = ModelRequest {
        provider: provider.clone(),
        model: config.model.name.clone(),
        system_instruction: "Reply with one word.".into(),
        summary: None,
        history: vec![crate::agent_state::HistoryEntry::User(
            crate::agent_state::UserEntry {
                message_id: crate::agent_state::MessageId(0),
                origin: crate::agent_state::MessageOrigin::Owner,
                text: "Say OK.".into(),
            },
        )],
        tools: Vec::new(),
        max_output_tokens: None,
    };
    match client.generate(request, CancellationToken::new()).await {
        Ok(reply) => {
            terminal.say(&format!(
                "Test request to {} succeeded: {}",
                config.model.name,
                reply.entry.text().trim()
            ));
            Ok(())
        }
        Err(error) => Err(GatewayError::Login(format!(
            "the test request failed: {error}"
        ))),
    }
}

/// The GPROXY channel for a configured API.
pub fn channel(api: Api) -> &'static str {
    match api {
        Api::Codex => "codex",
        Api::Openai => "openai",
        Api::Claudeapi => "claudeapi",
        Api::Aistudio => "aistudio",
        Api::Deepseek => "deepseek",
    }
}

fn append_variable(path: &Path, name: &str, value: &str) -> io::Result<()> {
    let mut text = fs::read_to_string(path)?;
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&format!("{name}={value}\n"));
    // Replace the file atomically, keeping mode 600.
    let staging = path.with_extension("env.new");
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&staging)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    fs::rename(&staging, path)
}

struct Admin {
    http: reqwest::Client,
    base: String,
    key: String,
}

impl Admin {
    async fn call(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value, GatewayError> {
        let mut request = self
            .http
            .request(method.clone(), format!("{}/admin/api{path}", self.base))
            .bearer_auth(&self.key);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.map_err(|error| {
            GatewayError::Admin(format!(
                "nothing answered at {}: {error}. Check `systemctl status gproxy`.",
                self.base
            ))
        })?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            let message = crate::llm_client::error_message(&text).unwrap_or_default();
            return Err(GatewayError::Admin(format!(
                "{method} {path}: HTTP {} {message}",
                status.as_u16()
            )));
        }
        Ok(serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    async fn create_gateway_key(&self, admin_user: &str) -> Result<String, GatewayError> {
        let users = self.call(Method::GET, "/users", None).await?;
        let user_id = items(&users)
            .into_iter()
            .find(|user| {
                ["name", "username", "userName"]
                    .iter()
                    .any(|field| user[*field].as_str() == Some(admin_user))
            })
            .and_then(|user| id_of(&user))
            .ok_or_else(|| GatewayError::Admin(format!("no gateway user named {admin_user}")))?;
        let created = self
            .call(
                Method::POST,
                "/api-keys",
                Some(json!({"userId": user_id, "name": "alyosha", "management": false})),
            )
            .await?;
        created["token"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| GatewayError::Admin("the new key's token was missing".into()))
    }

    async fn find_provider(&self, name: &str) -> Result<Option<String>, GatewayError> {
        let providers = self.call(Method::GET, "/providers", None).await?;
        Ok(items(&providers)
            .into_iter()
            .find(|provider| provider["name"].as_str() == Some(name))
            .and_then(|provider| id_of(&provider)))
    }

    async fn create_provider(&self, name: &str, channel: &str) -> Result<String, GatewayError> {
        let created = self
            .call(
                Method::POST,
                "/providers",
                Some(json!({"name": name, "channel": channel})),
            )
            .await?;
        id_of(&created)
            .ok_or_else(|| GatewayError::Admin("the new provider's ID was missing".into()))
    }

    async fn disable_body_capture(&self) -> Result<(), GatewayError> {
        self.call(
            Method::PATCH,
            "/settings",
            Some(json!({"logging": {"enableUpstreamLogBody": false, "enableDownstreamLogBody": false}})),
        )
        .await
        .map(drop)
    }

    async fn credentials(&self, provider_id: &str) -> Result<Vec<String>, GatewayError> {
        let all = self
            .call(
                Method::GET,
                &format!("/credentials?providerId={provider_id}"),
                None,
            )
            .await?;
        Ok(items(&all)
            .into_iter()
            .filter(|credential| credential["providerId"].as_str() == Some(provider_id))
            .filter_map(|credential| id_of(&credential))
            .collect())
    }

    async fn add_api_key(&self, provider_id: &str, key: &str) -> Result<String, GatewayError> {
        let created = self
            .call(
                Method::POST,
                "/credentials",
                Some(json!({"providerId": provider_id, "authKind": "api_key", "secret": {"api_key": key}})),
            )
            .await?;
        id_of(&created)
            .ok_or_else(|| GatewayError::Admin("the new credential's ID was missing".into()))
    }

    async fn delete_credential(&self, id: &str) -> Result<(), GatewayError> {
        self.call(Method::DELETE, &format!("/credentials/{id}"), None)
            .await
            .map(drop)
    }

    async fn device_login(
        &self,
        provider_id: &str,
        terminal: &mut dyn LoginTerminal,
    ) -> Result<String, GatewayError> {
        let started = self
            .call(
                Method::POST,
                "/credential-login/device/start",
                Some(json!({"providerId": provider_id, "label": "alyosha"})),
            )
            .await?;
        let session = started["loginSessionId"]
            .as_str()
            .ok_or_else(|| GatewayError::Admin("the login session ID was missing".into()))?
            .to_owned();
        let address = started["verificationUriComplete"]
            .as_str()
            .or(started["verificationUri"].as_str())
            .unwrap_or_default();
        terminal.say(&format!(
            "Open {address} on any device and enter the code {}",
            started["userCode"].as_str().unwrap_or_default()
        ));
        let mut interval = started["intervalSecs"].as_u64().unwrap_or(5);
        loop {
            tokio::time::sleep(Duration::from_secs(interval)).await;
            let outcome = self
                .call(
                    Method::POST,
                    "/credential-login/device/poll",
                    Some(json!({"loginSessionId": session})),
                )
                .await?;
            match outcome["status"].as_str().unwrap_or_default() {
                "pending" => interval = outcome["intervalSecs"].as_u64().unwrap_or(interval),
                "ready" => {
                    return outcome["credentialId"]
                        .as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| {
                            GatewayError::Admin("the credential ID was missing".into())
                        });
                }
                "denied" => return Err(GatewayError::Login("the sign-in was denied".into())),
                "expired" => {
                    return Err(GatewayError::Login(
                        "the code expired; run the command again".into(),
                    ));
                }
                other => {
                    return Err(GatewayError::Admin(format!(
                        "unexpected login status {other:?}"
                    )));
                }
            }
        }
    }
}

/// The items of a list response, which may be bare or wrapped.
fn items(value: &Value) -> Vec<Value> {
    let list = ["items", "data", "results"]
        .iter()
        .find_map(|key| value.get(*key))
        .unwrap_or(value);
    list.as_array().cloned().unwrap_or_default()
}

fn id_of(value: &Value) -> Option<String> {
    match &value["id"] {
        Value::String(id) => Some(id.clone()),
        Value::Number(id) => Some(id.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn files(root: &Path) -> GatewayFiles {
        GatewayFiles::new(&Paths::new(
            root,
            Some(root.join("config").as_os_str()),
            Some(root.join("data").as_os_str()),
        ))
    }

    #[test]
    fn init_writes_private_secrets_once() {
        let root = tempfile::tempdir().unwrap();
        let files = files(root.path());
        init(&files).unwrap();
        let mode = fs::metadata(&files.env_file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert!(files.data_dir.is_dir());
        let variables = files.read_env().unwrap();
        let master = lookup(&variables, "GPROXY_MASTER_KEY").unwrap();
        assert_eq!(master.len(), 64);
        assert!(master.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(lookup(&variables, "GPROXY_ADMIN_PASSWORD").is_some());
        assert!(lookup(&variables, "GPROXY_BOOTSTRAP_ADMIN_API_KEY").is_some());
        assert_eq!(files.gateway_key().unwrap(), None);

        let before = fs::read_to_string(&files.env_file).unwrap();
        assert!(matches!(
            init(&files),
            Err(GatewayError::AlreadyInitialized(_))
        ));
        assert_eq!(fs::read_to_string(&files.env_file).unwrap(), before);
    }

    #[test]
    fn a_variable_is_appended_without_losing_the_mode() {
        let root = tempfile::tempdir().unwrap();
        let files = files(root.path());
        init(&files).unwrap();
        append_variable(&files.env_file, GATEWAY_KEY_VAR, "gk-123").unwrap();
        assert_eq!(files.gateway_key().unwrap().as_deref(), Some("gk-123"));
        let mode = fs::metadata(&files.env_file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(files.read_env().unwrap().len(), 4);
    }
}
