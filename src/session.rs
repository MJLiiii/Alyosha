//! Session startup: everything `alyosha acp` does to open a
//! session in a workspace.
//!
//! It checks the workspace, starts the configured MCP servers, opens the
//! memory and skills stores, loads the dev shell, registers every tool, and
//! builds the normal system instruction, which each compaction rebuilds. A
//! resumed session first closes a turn a restart cut off. See
//! `docs/AgentLoop.md#session-driver` and
//! `docs/SessionStore.md#resume`.

use std::{
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use alyosha_config::{Config, Paths};
use chrono::Utc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{
    BoxFuture,
    agent_loop::{
        DEFAULT_INSTRUCTION, Persistence, SessionDriver, SessionEvent, SessionSetup,
    },
    approvals::{self, Approver},
    clock::{Clock, SystemClock},
    context_manager::RouteLimits,
    environment::{Environment, SessionLogs, shell},
    gateway_admin::GatewayFiles,
    jobs::{self, JobEnvironment},
    llm_client::{GatewayClient, ModelClient, ModelError, ModelReply, ModelRequest, Timeouts},
    mcp::McpManager,
    memory::{self, MemoryStore},
    session_store::{self, INTERRUPTED_LINE, RESUMED_LINE},
    skills::{self, SkillStore},
    tools::{
        RegistrationError, SessionServices, ToolRegistry,
        builtin::{edit_file_tool, read_file_tool, run_command_tool, write_file_tool},
        code_mode::execute_code_tool,
    },
    user_notes::{GUEST_INSTRUCTION, UserNotes, user_memory_tool},
};

/// How long the dev shell may take to load.
pub const DEV_SHELL_LIMIT: Duration = Duration::from_secs(10 * 60);

/// What differs between front ends.
pub struct FrontEnd {
    /// How the owner answers approvals and questions.
    pub approver: Option<Arc<dyn Approver>>,
    /// In ACP mode, the line naming the chat bridge.
    pub channel_line: Option<String>,
    /// The log file that startup warnings name.
    pub log_file: Option<PathBuf>,
    /// Shows **Loading dev shell…** while the dev shell loads.
    pub on_loading_dev_shell: Option<Box<dyn Fn(bool) + Send + Sync>>,
    /// Where the session's history is saved. Without it, history lives
    /// only in memory.
    pub persistence: Option<Persistence>,
}

/// A started session.
pub struct Started {
    pub driver: SessionDriver,
    pub events: mpsc::UnboundedReceiver<SessionEvent>,
    pub workspace: PathBuf,
    pub model: String,
    /// Each in full, for the front end and the log.
    pub warnings: Vec<String>,
}

/// Why a session could not start.
#[derive(Debug, thiserror::Error)]
pub enum StartupError {
    #[error("{0}")]
    Workspace(String),
    #[error("a built-in tool could not be registered: {0}")]
    Registry(#[from] RegistrationError),
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("the session store failed: {0}")]
    Store(String),
}

/// Prepares a saved session: a resumed one gets the turn a restart cut off
/// closed, and the resume line goes first among its startup warnings.
async fn prepare_persistence(
    persistence: Option<Persistence>,
    warnings: &mut Vec<String>,
) -> Result<Option<Persistence>, StartupError> {
    let Some(mut persistence) = persistence else {
        return Ok(None);
    };
    if persistence.resumed {
        let interrupted =
            session_store::repair(persistence.store.as_ref(), &mut persistence.stored)
                .await
                .map_err(|error| StartupError::Store(error.to_string()))?;
        let line = if interrupted {
            format!("{RESUMED_LINE} {INTERRUPTED_LINE}")
        } else {
            RESUMED_LINE.to_owned()
        };
        warnings.insert(0, line);
        tracing::info!(
            key = %persistence.stored.key,
            interrupted,
            "resumed a session"
        );
    } else {
        tracing::info!(key = %persistence.stored.key, "created a session");
    }
    Ok(Some(persistence))
}

/// The normal system instruction: the default, the memory and skills
/// sections, and the channel line.
fn normal_instruction(
    memory: &MemoryStore,
    skills: &SkillStore,
    channel_line: Option<&str>,
) -> String {
    let mut instruction = format!(
        "{DEFAULT_INSTRUCTION}\n\n{}\n{}",
        memory::instruction_section(&memory.index()),
        skills::instruction_section(&skills.index())
    );
    if let Some(line) = channel_line {
        instruction.push('\n');
        instruction.push_str(line);
        instruction.push('\n');
    }
    instruction
}

/// Resolves a workspace: any existing folder, without symlinks.
pub fn resolve_workspace(path: &Path) -> Result<PathBuf, StartupError> {
    let resolved = path
        .canonicalize()
        .map_err(|error| StartupError::Workspace(format!("{}: {error}", path.display())))?;
    if !resolved.is_dir() {
        return Err(StartupError::Workspace(format!(
            "{} is not a folder",
            resolved.display()
        )));
    }
    Ok(resolved)
}

/// A client for a session whose provider is not signed in: every request
/// fails with the `gateway login` hint.
struct NotSignedIn;

impl ModelClient for NotSignedIn {
    fn generate(
        &self,
        _request: ModelRequest,
        _cancel: CancellationToken,
    ) -> BoxFuture<'_, Result<ModelReply, ModelError>> {
        Box::pin(async { Err(ModelError::NotSignedIn) })
    }
}

/// The client every session uses: GPROXY with the gateway key from
/// `gproxy.env`. Sessions never sign in.
pub fn model_client(config: &Config, paths: &Paths) -> Arc<dyn ModelClient> {
    match GatewayFiles::new(paths).gateway_key() {
        Ok(Some(key)) => Arc::new(
            GatewayClient::new(
                &config.gateway.url,
                &key,
                config.model.api,
                Timeouts::default(),
            )
            .with_effort(config.model.effort),
        ),
        _ => Arc::new(NotSignedIn),
    }
}

/// Opens a session in `workspace`.
pub async fn start(
    config: &Config,
    paths: &Paths,
    workspace: &Path,
    client: Arc<dyn ModelClient>,
    front_end: FrontEnd,
) -> Result<Started, StartupError> {
    start_with_clock(
        config,
        paths,
        workspace,
        client,
        front_end,
        Arc::new(SystemClock),
    )
    .await
}

pub async fn start_with_clock(
    config: &Config,
    paths: &Paths,
    workspace: &Path,
    client: Arc<dyn ModelClient>,
    front_end: FrontEnd,
    clock: Arc<dyn Clock>,
) -> Result<Started, StartupError> {
    let workspace = resolve_workspace(workspace)?;
    let mut warnings = Vec::new();
    let details = front_end
        .log_file
        .as_ref()
        .map(|path| format!(", details in {}", path.display()))
        .unwrap_or_default();

    // Private per-session folders: logs, status files, the generated resolv.conf.
    let session_dir = paths.data_dir.join("sessions").join(format!(
        "{}-{}",
        std::process::id(),
        crate::environment::random_suffix()
    ));
    let logs = Arc::new(SessionLogs::new(session_dir.join("logs")));
    let mut sandbox = shell::SandboxConfig::discover(
        paths.data_dir.join("shell-cache"),
        session_dir.join("private"),
    );
    if sandbox.programs.is_none() {
        warnings.push(format!(
            "The shell sandbox is unavailable: bwrap, pasta, or bash was not found{details}"
        ));
    }
    if workspace.join("flake.nix").is_file() {
        if let Some(loading) = &front_end.on_loading_dev_shell {
            loading(true);
        }
        match shell::load_dev_shell(&workspace, DEV_SHELL_LIMIT).await {
            Ok(variables) => sandbox.dev_shell = variables,
            Err(error) => warnings.push(format!(
                "The dev shell did not load, so commands use the default tools: {error}{details}"
            )),
        }
        if let Some(loading) = &front_end.on_loading_dev_shell {
            loading(false);
        }
    }

    let skills_root = paths.config_dir.join("skills");
    let environment = Environment::new(
        &workspace,
        Some(skills_root.clone()),
        logs.clone(),
        sandbox.clone(),
    )?;
    let timezone = config.timezone;
    let memory = MemoryStore::open(
        &paths.data_dir.join("memory"),
        "owner",
        Arc::new(move || Utc::now().with_timezone(&timezone).date_naive()),
    );
    let skills = SkillStore::open(&skills_root);
    let memory_index = memory.index();
    if !memory_index.unreadable.is_empty() {
        warnings.push(format!(
            "{} memory files could not be read: {}",
            memory_index.unreadable.len(),
            memory_index.unreadable.join(", ")
        ));
    }
    let skill_index = skills.index();
    if !skill_index.invalid.is_empty() {
        warnings.push(format!(
            "{} skills could not be loaded: {}",
            skill_index.invalid.len(),
            skill_index.invalid.join(", ")
        ));
    }

    let (mcp, mcp_warnings) = McpManager::start(&config.mcp, &workspace).await;
    let mut registry = ToolRegistry::new();
    registry.register(read_file_tool())?;
    registry.register(write_file_tool())?;
    registry.register(edit_file_tool())?;
    registry.register(run_command_tool())?;
    registry.register(jobs::job_tool())?;
    registry.register(execute_code_tool())?;
    registry.register(memory::memory_tool())?;
    registry.register(skills::skill_tool())?;
    registry.register(approvals::ask_owner_tool())?;
    let mut mcp_problems = mcp_warnings;
    for tool in mcp.tools() {
        let name = tool.definition.name.clone();
        if let Err(error) = registry.register(tool) {
            mcp_problems.push(format!("{name}: {error}"));
        }
    }
    for problem in &mcp_problems {
        tracing::warn!(problem = %problem, "MCP startup problem");
    }
    if !mcp_problems.is_empty() {
        warnings.push(format!(
            "MCP: {} problem{} at startup ({}){details}",
            mcp_problems.len(),
            if mcp_problems.len() == 1 { "" } else { "s" },
            mcp_problems.join("; ")
        ));
    }

    let system_instruction =
        normal_instruction(&memory, &skills, front_end.channel_line.as_deref());
    let rebuild_instruction: Arc<dyn Fn() -> String + Send + Sync> = {
        let memory_dir = paths.data_dir.join("memory");
        let skills_root = skills_root.clone();
        let channel_line = front_end.channel_line.clone();
        Arc::new(move || {
            let memory = MemoryStore::open(
                &memory_dir,
                "owner",
                Arc::new(move || Utc::now().with_timezone(&timezone).date_naive()),
            );
            normal_instruction(
                &memory,
                &SkillStore::open(&skills_root),
                channel_line.as_deref(),
            )
        })
    };
    let persistence = prepare_persistence(front_end.persistence, &mut warnings).await?;

    let job_environment = JobEnvironment {
        sandbox,
        workspace: workspace.clone(),
        cwd: PathBuf::new(),
        skills_root: environment.skills_root.clone(),
        logs,
    };
    let services = SessionServices {
        environment: Some(environment),
        memory: Some(memory),
        skills: Some(skills),
        jobs: None,
        approver: front_end.approver,
        approval_timeout: Some(Duration::from_secs(config.approvals.timeout_minutes * 60)),
        mcp: Some(mcp),
        ..SessionServices::default()
    };
    let (driver, events) = SessionDriver::start(SessionSetup {
        provider: config.model.provider.clone(),
        model: config.model.name.clone(),
        timezone: config.timezone,
        client,
        registry,
        clock,
        system_instruction,
        limits: RouteLimits::for_model(&config.model.name),
        services,
        jobs: Some(job_environment),
        persistence,
        rebuild_instruction: Some(rebuild_instruction),
    });
    for warning in &warnings {
        tracing::warn!(warning = %warning, "startup warning");
    }
    tracing::info!(
        workspace = %workspace.display(),
        model = %config.model.name,
        "session started"
    );
    Ok(Started {
        driver,
        events,
        workspace,
        model: config.model.name.clone(),
        warnings,
    })
}

/// Opens a guest session for a group chat: only `user_memory` and the MCP
/// servers `[guest]` lists, no workspace, sandbox, memory index, skills,
/// jobs, or approver. Its MCP servers run in `<data>/guest`.
/// See `docs/Channels.md#guest-mode`.
pub async fn start_guest(
    config: &Config,
    paths: &Paths,
    client: Arc<dyn ModelClient>,
    channel_line: Option<String>,
    clock: Arc<dyn Clock>,
    persistence: Option<Persistence>,
) -> Result<Started, StartupError> {
    let folder = paths.data_dir.join("guest");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&folder)?;
    let mut warnings = Vec::new();
    let servers = config.mcp.only(&config.guest.mcp_servers);
    let (mcp, mcp_warnings) = McpManager::start(&servers, &folder).await;
    let mut registry = ToolRegistry::new();
    registry.register(user_memory_tool())?;
    let mut problems = mcp_warnings;
    for tool in mcp.tools() {
        let name = tool.definition.name.clone();
        // `[guest]` refuses servers with tools in `ask`; check again here.
        if tool.definition.requires_approval {
            problems.push(format!("{name}: needs approval, which guests cannot give"));
            continue;
        }
        if let Err(error) = registry.register(tool) {
            problems.push(format!("{name}: {error}"));
        }
    }
    if !problems.is_empty() {
        warnings.push(format!("MCP: {}", problems.join("; ")));
    }
    let mut system_instruction = GUEST_INSTRUCTION.to_owned();
    if let Some(line) = &channel_line {
        system_instruction.push('\n');
        system_instruction.push_str(line);
        system_instruction.push('\n');
    }
    let persistence = prepare_persistence(persistence, &mut warnings).await?;
    let timezone = config.timezone;
    let services = SessionServices {
        mcp: Some(mcp),
        users: Some(UserNotes::open(
            &paths.data_dir.join("memory"),
            Arc::new(move || Utc::now().with_timezone(&timezone).date_naive()),
        )),
        ..SessionServices::default()
    };
    let (driver, events) = SessionDriver::start(SessionSetup {
        provider: config.model.provider.clone(),
        model: config.model.name.clone(),
        timezone: config.timezone,
        client,
        registry,
        clock,
        system_instruction,
        limits: RouteLimits::for_model(&config.model.name),
        services,
        jobs: None,
        persistence,
        rebuild_instruction: None,
    });
    for warning in &warnings {
        tracing::warn!(warning = %warning, "guest startup warning");
    }
    tracing::info!(model = %config.model.name, "guest session started");
    Ok(Started {
        driver,
        events,
        workspace: folder,
        model: config.model.name.clone(),
        warnings,
    })
}
