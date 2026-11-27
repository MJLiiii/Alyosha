//! Shell commands: each runs in a systemd unit of its own, started from the
//! session's template, as the user `alyosha-run`.
//!
//! The container module declares one template per working folder, which fixes
//! the user, mounts, limits, and network rules. Alyosha writes the command's
//! description into a run folder, starts the unit, reads its output file as it
//! grows, and reads systemd's result when it ends. See
//! `docs/EnvironmentInterface.md#command-launch` and
//! `docs/Deployment.md#command-units`.

use std::{
    collections::BTreeMap,
    fs::{self, DirBuilder, File, OpenOptions},
    io::Read,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use serde::{Deserialize, Serialize};
use tokio::{io::AsyncReadExt, process::Command};
use tokio_util::sync::CancellationToken;

use crate::tools::ToolError;

/// How a command ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellStatus {
    /// The exit code, `128 + signal` for a signal.
    Exited(i32),
    TimedOut,
    Stopped,
}

impl std::fmt::Display for ShellStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ShellStatus::Exited(code) => write!(f, "{code}"),
            ShellStatus::TimedOut => f.write_str("TimedOut"),
            ShellStatus::Stopped => f.write_str("Stopped"),
        }
    }
}

/// A finished command and everything it printed, stdout and stderr in
/// arrival order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    pub status: ShellStatus,
    pub output: Vec<u8>,
}

/// Where commands run.
#[derive(Debug, Clone)]
pub enum Runner {
    /// Commands are unavailable, for the reason given.
    Unavailable(String),
    /// Each command runs in a fresh unit from `template`, such as
    /// `alyosha-run-owner`, with its run folder under `runs_dir`.
    Units { template: String, runs_dir: PathBuf },
    /// A plain child process in the workspace, without any isolation. Only
    /// for tests and development outside the container, chosen with
    /// `ALYOSHA_COMMANDS=direct`.
    Direct,
}

impl Default for Runner {
    fn default() -> Self {
        Self::Unavailable("this session runs no commands".into())
    }
}

/// The variable that chooses [`Runner::Direct`].
pub const DIRECT_VARIABLE: &str = "ALYOSHA_COMMANDS";

/// The folder the container module creates for run folders.
pub const RUNS_DIR: &str = "/run/alyosha-run";

/// The session's command settings.
#[derive(Debug, Clone, Default)]
pub struct SandboxConfig {
    pub runner: Runner,
    /// The packaged Bash, as a store path.
    pub bash: Option<PathBuf>,
    /// The directories of the default tools: Bash, coreutils, findutils, Git,
    /// and ripgrep.
    pub default_path: Vec<PathBuf>,
    /// Exported variables of the workspace's dev shell.
    pub dev_shell: Vec<(String, String)>,
    /// The CA bundle's real path in the Nix store, if found.
    pub ca_bundle: Option<PathBuf>,
}

impl SandboxConfig {
    /// Finds Bash and the default tools on `PATH`, which Alyosha's package sets
    /// to their store paths, and the CA bundle.
    pub fn discover(runner: Runner) -> Self {
        let find = |name: &str| which(name).and_then(|path| path.canonicalize().ok());
        let mut default_path = Vec::new();
        for tool in ["bash", "ls", "find", "git", "rg"] {
            if let Some(dir) = find(tool).and_then(|path| path.parent().map(Path::to_owned))
                && !default_path.contains(&dir)
            {
                default_path.push(dir);
            }
        }
        let ca_bundle = [
            "/etc/ssl/certs/ca-certificates.crt",
            "/etc/ssl/certs/ca-bundle.crt",
        ]
        .iter()
        .find_map(|path| Path::new(path).canonicalize().ok())
        .filter(|path| path.starts_with("/nix/store"));
        Self {
            runner,
            bash: find("bash"),
            default_path,
            dev_shell: Vec::new(),
            ca_bundle,
        }
    }
}

/// The runner for a session in `workspace`: the unit template of the working
/// folder it is, or [`Runner::Direct`] when `ALYOSHA_COMMANDS=direct`.
pub fn runner_for(work_dir: &Path, workspace: &Path) -> Runner {
    if std::env::var(DIRECT_VARIABLE).as_deref() == Ok("direct") {
        return Runner::Direct;
    }
    match template_for(work_dir, workspace) {
        Some(template) => Runner::Units {
            template,
            runs_dir: PathBuf::from(RUNS_DIR),
        },
        None => Runner::Unavailable(format!(
            "commands run only in Alyosha's working folders under {}",
            work_dir.display()
        )),
    }
}

/// `alyosha-run-owner` for `<work>/owner`, `alyosha-run-group-<name>` for
/// `<work>/groups/<name>`, and nothing for any other folder.
pub fn template_for(work_dir: &Path, workspace: &Path) -> Option<String> {
    let relative = workspace.strip_prefix(work_dir).ok()?;
    let parts: Vec<&str> = relative.iter().filter_map(|part| part.to_str()).collect();
    match parts.as_slice() {
        ["owner"] => Some("alyosha-run-owner".into()),
        ["groups", name] if alyosha_config::is_group_name(name) => {
            Some(format!("alyosha-run-group-{name}"))
        }
        _ => None,
    }
}

fn which(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join(name))
            .find(|candidate| candidate.is_file())
    })
}

/// The variables a dev shell may not set.
const RESERVED: [&str; 9] = [
    "HOME",
    "PWD",
    "SHELL",
    "TMPDIR",
    "TMP",
    "TEMP",
    "TEMPDIR",
    "NIX_BUILD_TOP",
    "OLDPWD",
];

/// Loads the workspace's flake dev shell, if it has a `flake.nix`, outside
/// the sandbox. Returns its exported variables, or the error to warn about.
pub async fn load_dev_shell(
    workspace: &Path,
    limit: Duration,
) -> Result<Vec<(String, String)>, String> {
    let mut command = Command::new("nix");
    command
        .args([
            "--extra-experimental-features",
            "nix-command flakes",
            "print-dev-env",
            "--json",
        ])
        .arg(workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let output = tokio::time::timeout(limit, command.output())
        .await
        .map_err(|_| {
            format!(
                "the dev shell did not load within {} minutes",
                limit.as_secs() / 60
            )
        })?
        .map_err(|error| format!("could not run nix: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "nix print-dev-env failed: {}",
            nix_error(&String::from_utf8_lossy(&output.stderr))
        ));
    }
    parse_dev_env(&output.stdout)
}

/// The useful part of Nix's error output: the `error:` line, joined with the
/// lines that continue it, or else the last line.
pub fn nix_error(stderr: &str) -> String {
    let lines: Vec<&str> = stderr.lines().collect();
    let Some(start) = lines
        .iter()
        .rposition(|line| line.trim_start().starts_with("error:"))
    else {
        return lines
            .iter()
            .rev()
            .find(|line| !line.trim().is_empty())
            .unwrap_or(&"")
            .trim()
            .to_owned();
    };
    let mut message = lines[start]
        .trim()
        .trim_start_matches("error:")
        .trim()
        .to_owned();
    for line in &lines[start + 1..] {
        let line = line.trim();
        if !line.is_empty() {
            message.push(' ');
            message.push_str(line);
        }
    }
    message
}

/// Takes the `exported` variables of `nix print-dev-env --json`.
pub fn parse_dev_env(json: &[u8]) -> Result<Vec<(String, String)>, String> {
    let value: serde_json::Value =
        serde_json::from_slice(json).map_err(|error| format!("unreadable dev shell: {error}"))?;
    let mut variables = Vec::new();
    for (name, variable) in value["variables"].as_object().into_iter().flatten() {
        if variable["type"] == "exported"
            && !RESERVED.contains(&name.as_str())
            && let Some(text) = variable["value"].as_str()
        {
            variables.push((name.clone(), text.to_owned()));
        }
    }
    Ok(variables)
}

/// What one command needs from the session.
pub struct ShellLaunch<'a> {
    pub config: &'a SandboxConfig,
    pub workspace: &'a Path,
    /// The working directory, relative to the workspace.
    pub cwd: &'a Path,
    pub skills_root: Option<&'a Path>,
}

/// What a unit's `alyosha run-exec` reads from its run folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandSpec {
    pub command: String,
    /// The working directory inside the unit, under `/workspace`.
    pub cwd: PathBuf,
    pub env: BTreeMap<String, String>,
    pub bash: PathBuf,
}

impl ShellLaunch<'_> {
    /// The environment, built from nothing in the documented order.
    fn environment(&self) -> BTreeMap<String, String> {
        let config = self.config;
        let mut env: BTreeMap<String, String> = config.dev_shell.iter().cloned().collect();
        let defaults = config
            .default_path
            .iter()
            .map(|dir| dir.display().to_string())
            .collect::<Vec<_>>()
            .join(":");
        let path = match env.get("PATH") {
            Some(dev) if !dev.is_empty() && !defaults.is_empty() => format!("{dev}:{defaults}"),
            Some(dev) if !dev.is_empty() => dev.clone(),
            _ => defaults,
        };
        env.insert("PATH".into(), path);
        for (name, value) in [
            ("XDG_CACHE_HOME", "/home/alyosha/.cache"),
            ("CARGO_HOME", "/home/alyosha/.cache/cargo"),
            ("npm_config_cache", "/home/alyosha/.cache/npm"),
        ] {
            env.entry(name.into()).or_insert_with(|| value.into());
        }
        if let Some(bundle) = &config.ca_bundle {
            for name in ["SSL_CERT_FILE", "NIX_SSL_CERT_FILE"] {
                env.entry(name.into())
                    .or_insert_with(|| bundle.display().to_string());
            }
        }
        env.insert("HOME".into(), "/home/alyosha".into());
        env.insert("TMPDIR".into(), "/tmp".into());
        env
    }

    fn spec(&self, command: &str) -> Result<CommandSpec, LaunchFailure> {
        Ok(CommandSpec {
            command: command.to_owned(),
            cwd: Path::new("/workspace").join(self.cwd),
            env: self.environment(),
            bash: self
                .config
                .bash
                .clone()
                .ok_or_else(|| LaunchFailure("bash was not found on PATH".into()))?,
        })
    }
}

/// Where a command's output goes as it arrives.
pub trait OutputSink: Send {
    fn write(&mut self, bytes: &[u8]);
}

impl OutputSink for Vec<u8> {
    fn write(&mut self, bytes: &[u8]) {
        self.extend_from_slice(bytes);
    }
}

/// Runs one command in a fresh unit, with a deadline that covers its start.
/// Stop and the deadline stop the whole unit and return the output so far.
pub async fn run_shell(
    launch: &ShellLaunch<'_>,
    command: &str,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<CommandOutput, ToolError> {
    let sink = Arc::new(Mutex::new(Vec::new()));
    let status = run_shell_into(launch, command, timeout, cancel, sink.clone()).await;
    let output = std::mem::take(&mut *sink.lock().unwrap());
    match status {
        Ok(status) => Ok(CommandOutput { status, output }),
        Err(LaunchFailure(message)) => Err(ToolError::failed(format!(
            "The sandbox could not start: {message}"
        ))),
    }
}

/// The command did not start.
#[derive(Debug)]
pub struct LaunchFailure(pub String);

/// Like [`run_shell`], streaming output into `sink`.
pub async fn run_shell_into<S: OutputSink + ?Sized + 'static>(
    launch: &ShellLaunch<'_>,
    command: &str,
    timeout: Duration,
    cancel: &CancellationToken,
    sink: Arc<Mutex<S>>,
) -> Result<ShellStatus, LaunchFailure> {
    let deadline = tokio::time::Instant::now() + timeout;
    if !launch.workspace.is_dir() {
        return Err(LaunchFailure(format!(
            "the workspace {} does not exist",
            launch.workspace.display()
        )));
    }
    match &launch.config.runner {
        Runner::Unavailable(reason) => Err(LaunchFailure(reason.clone())),
        Runner::Units { template, runs_dir } => {
            let spec = launch.spec(command)?;
            run_unit(template, runs_dir, &spec, deadline, cancel, sink).await
        }
        Runner::Direct => run_direct(launch, command, deadline, cancel, sink).await,
    }
}

static RUNS: AtomicU64 = AtomicU64::new(0);

/// A unit's instance name: this process's ID and a counter.
fn next_instance() -> String {
    format!(
        "{}-{}",
        std::process::id(),
        RUNS.fetch_add(1, Ordering::Relaxed) + 1
    )
}

async fn systemctl(args: &[&str]) -> Result<String, String> {
    let output = Command::new("systemctl")
        .arg("--no-ask-password")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|error| format!("could not run systemctl: {error}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(stderr
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("systemctl failed")
            .trim()
            .to_owned())
    }
}

/// The unit's state, from `systemctl show`.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct UnitState {
    pub active: String,
    pub sub: String,
    pub result: String,
    /// `exited`, `killed`, or `dumped`, as numbers 1, 2, and 3.
    pub main_code: i32,
    pub main_status: i32,
}

impl UnitState {
    pub fn parse(text: &str) -> Self {
        let mut state = Self::default();
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            match key {
                "ActiveState" => state.active = value.to_owned(),
                "SubState" => state.sub = value.to_owned(),
                "Result" => state.result = value.to_owned(),
                "ExecMainCode" => state.main_code = value.parse().unwrap_or_default(),
                "ExecMainStatus" => state.main_status = value.parse().unwrap_or_default(),
                _ => {}
            }
        }
        state
    }

    /// The template keeps a finished unit as `active (exited)` or `failed`,
    /// so its result can be read.
    pub fn finished(&self) -> bool {
        self.active == "failed" || self.sub == "exited" || self.active == "inactive"
    }

    /// The command's status, as a shell reports it.
    pub fn status(&self) -> i32 {
        match self.main_code {
            2 | 3 => 128 + self.main_status,
            _ => self.main_status,
        }
    }
}

async fn unit_state(unit: &str) -> Result<UnitState, String> {
    systemctl(&[
        "show",
        "--property=ActiveState,SubState,Result,ExecMainCode,ExecMainStatus",
        unit,
    ])
    .await
    .map(|text| UnitState::parse(&text))
}

/// Copies what the unit added to its output file into the sink.
fn drain<S: OutputSink + ?Sized>(file: &mut File, sink: &Mutex<S>) {
    let mut buffer = Vec::new();
    if file.read_to_end(&mut buffer).is_ok() && !buffer.is_empty() {
        sink.lock().unwrap().write(&buffer);
    }
}

async fn run_unit<S: OutputSink + ?Sized + 'static>(
    template: &str,
    runs_dir: &Path,
    spec: &CommandSpec,
    deadline: tokio::time::Instant,
    cancel: &CancellationToken,
    sink: Arc<Mutex<S>>,
) -> Result<ShellStatus, LaunchFailure> {
    let instance = next_instance();
    let unit = format!("{template}@{instance}.service");
    let folder = runs_dir.join(&instance);
    let failure = |error: std::io::Error| LaunchFailure(error.to_string());
    DirBuilder::new()
        .mode(0o750)
        .create(&folder)
        .map_err(failure)?;
    let cleanup = || {
        let _ = fs::remove_dir_all(&folder);
    };
    let prepared = (|| {
        fs::write(
            folder.join("spec.json"),
            serde_json::to_vec(spec).expect("a spec serializes"),
        )?;
        OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o640)
            .open(folder.join("output"))
    })();
    if let Err(error) = prepared {
        cleanup();
        return Err(failure(error));
    }
    if let Err(message) = systemctl(&["start", &unit]).await {
        let _ = systemctl(&["reset-failed", &unit]).await;
        cleanup();
        return Err(LaunchFailure(message));
    }
    let mut output = match File::open(folder.join("output")) {
        Ok(file) => file,
        Err(error) => {
            let _ = systemctl(&["stop", &unit]).await;
            cleanup();
            return Err(failure(error));
        }
    };
    let mut interval = Duration::from_millis(10);
    let ended = loop {
        drain(&mut output, &sink);
        match unit_state(&unit).await {
            Ok(state) if state.finished() => break Ok(state),
            Ok(_) => {}
            Err(message) => break Err(message),
        }
        tokio::select! {
            () = tokio::time::sleep(interval) => {}
            () = tokio::time::sleep_until(deadline) => {
                let _ = systemctl(&["stop", &unit]).await;
                drain(&mut output, &sink);
                let _ = systemctl(&["reset-failed", &unit]).await;
                cleanup();
                return Ok(ShellStatus::TimedOut);
            }
            () = cancel.cancelled() => {
                let _ = systemctl(&["stop", &unit]).await;
                drain(&mut output, &sink);
                let _ = systemctl(&["reset-failed", &unit]).await;
                cleanup();
                return Ok(ShellStatus::Stopped);
            }
        }
        interval = (interval * 2).min(Duration::from_millis(200));
    };
    drain(&mut output, &sink);
    let _ = systemctl(&["stop", &unit]).await;
    let _ = systemctl(&["reset-failed", &unit]).await;
    cleanup();
    let state = ended.map_err(LaunchFailure)?;
    if state.result == "oom-kill" {
        sink.lock()
            .unwrap()
            .write(b"\n[The command was stopped: it passed its memory limit.]\n");
        return Ok(ShellStatus::Exited(137));
    }
    if state.main_code == 0 {
        return Err(LaunchFailure(format!(
            "the unit ended before the command started ({})",
            state.result
        )));
    }
    Ok(ShellStatus::Exited(state.status()))
}

/// Inside a unit: reads the run folder's description and replaces this process
/// with the command. Returns only on failure.
pub fn run_exec(runs_dir: &Path, instance: &str) -> std::io::Error {
    use std::os::unix::process::CommandExt;
    if instance.contains('/') || instance.starts_with('.') {
        return std::io::Error::other(format!("{instance:?} is not a run"));
    }
    let spec: CommandSpec = match fs::read(runs_dir.join(instance).join("spec.json"))
        .and_then(|bytes| serde_json::from_slice(&bytes).map_err(std::io::Error::other))
    {
        Ok(spec) => spec,
        Err(error) => return error,
    };
    std::process::Command::new(&spec.bash)
        .args(["--noprofile", "--norc", "-c", &spec.command])
        .current_dir(&spec.cwd)
        .env_clear()
        .envs(&spec.env)
        .exec()
}

/// Stops the units and removes the run folders of `alyosha acp` processes that
/// no longer run, such as one that crashed with a job running.
pub async fn stop_leftover_units(runs_dir: &Path) {
    let alive = |instance: &str| {
        instance
            .split('-')
            .next()
            .is_some_and(|pid| Path::new("/proc").join(pid).exists())
    };
    if let Ok(list) = systemctl(&[
        "list-units",
        "--all",
        "--plain",
        "--no-legend",
        "--no-pager",
        "alyosha-run-*@*.service",
    ])
    .await
    {
        for unit in list
            .lines()
            .filter_map(|line| line.split_whitespace().next())
        {
            let instance = unit
                .split_once('@')
                .map(|(_, rest)| rest.trim_end_matches(".service"))
                .unwrap_or_default();
            if !alive(instance) {
                tracing::info!(unit, "stopping a leftover command unit");
                let _ = systemctl(&["stop", unit]).await;
                let _ = systemctl(&["reset-failed", unit]).await;
            }
        }
    }
    for entry in fs::read_dir(runs_dir).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !alive(&name) {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

/// [`Runner::Direct`]: Bash as a plain child process in the workspace, in a
/// process group of its own that the deadline and Stop kill.
async fn run_direct<S: OutputSink + ?Sized + 'static>(
    launch: &ShellLaunch<'_>,
    command: &str,
    deadline: tokio::time::Instant,
    cancel: &CancellationToken,
    sink: Arc<Mutex<S>>,
) -> Result<ShellStatus, LaunchFailure> {
    let bash = launch
        .config
        .bash
        .clone()
        .ok_or_else(|| LaunchFailure("bash was not found on PATH".into()))?;
    let mut env = launch.environment();
    env.insert("HOME".into(), launch.workspace.display().to_string());
    env.insert("TMPDIR".into(), std::env::temp_dir().display().to_string());
    for name in ["XDG_CACHE_HOME", "CARGO_HOME", "npm_config_cache"] {
        env.remove(name);
    }
    let mut child = Command::new(bash)
        .args(["--noprofile", "--norc", "-c", command])
        .current_dir(launch.workspace.join(launch.cwd))
        .env_clear()
        .envs(&env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| LaunchFailure(error.to_string()))?;
    let mut readers = Vec::new();
    for mut stream in [
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Send + Unpin>),
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Send + Unpin>),
    ]
    .into_iter()
    .flatten()
    {
        let output = sink.clone();
        readers.push(tokio::spawn(async move {
            let mut buffer = [0u8; 8192];
            while let Ok(read) = stream.read(&mut buffer).await {
                if read == 0 {
                    break;
                }
                output.lock().unwrap().write(&buffer[..read]);
            }
        }));
    }
    let pid = child.id();
    let (status, ended) = tokio::select! {
        status = child.wait() => (Some(status.map_err(|error| LaunchFailure(error.to_string()))?), None),
        () = tokio::time::sleep_until(deadline) => (None, Some(ShellStatus::TimedOut)),
        () = cancel.cancelled() => (None, Some(ShellStatus::Stopped)),
    };
    if let Some(pid) = pid.and_then(|pid| rustix::process::Pid::from_raw(pid as i32)) {
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
    }
    if ended.is_some() {
        let _ = child.wait().await;
    }
    for reader in readers {
        let _ = tokio::time::timeout(Duration::from_secs(2), reader).await;
    }
    if let Some(ended) = ended {
        return Ok(ended);
    }
    use std::os::unix::process::ExitStatusExt;
    let status = status.expect("set when the child exited");
    Ok(ShellStatus::Exited(
        status
            .code()
            .or_else(|| status.signal().map(|signal| 128 + signal))
            .unwrap_or(1),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nix_errors_keep_the_error_line_and_its_hint() {
        let stderr = "warning: Git tree is dirty\nerror: Path 'flake.nix' in the repository \"/w\" is not tracked by Git.\n\n       To make it visible to Nix, run:\n\n       git -C \"/w\" add \"flake.nix\"\n";
        assert_eq!(
            nix_error(stderr),
            "Path 'flake.nix' in the repository \"/w\" is not tracked by Git. To make it visible to Nix, run: git -C \"/w\" add \"flake.nix\""
        );
        assert_eq!(nix_error("something odd\n"), "something odd");
    }

    #[test]
    fn dev_shell_keeps_exported_variables_except_reserved_ones() {
        let json = br#"{"variables": {
            "PATH": {"type": "exported", "value": "/nix/store/x-cargo/bin"},
            "HOME": {"type": "exported", "value": "/homeless-shelter"},
            "TMPDIR": {"type": "exported", "value": "/tmp/x"},
            "shellHook": {"type": "var", "value": "echo hi"},
            "RUSTFLAGS": {"type": "exported", "value": "-Dwarnings"}
        }}"#;
        let mut variables = parse_dev_env(json).unwrap();
        variables.sort();
        assert_eq!(
            variables,
            [
                ("PATH".to_owned(), "/nix/store/x-cargo/bin".to_owned()),
                ("RUSTFLAGS".to_owned(), "-Dwarnings".to_owned()),
            ]
        );
    }

    #[test]
    fn the_environment_follows_the_documented_order() {
        let config = SandboxConfig {
            default_path: vec!["/nix/store/a-coreutils/bin".into()],
            dev_shell: vec![
                ("PATH".into(), "/nix/store/x-cargo/bin".into()),
                ("CARGO_HOME".into(), "/custom".into()),
            ],
            ca_bundle: Some("/nix/store/c-ca/ca.crt".into()),
            ..SandboxConfig::default()
        };
        let launch = ShellLaunch {
            config: &config,
            workspace: Path::new("/w"),
            cwd: Path::new("sub"),
            skills_root: None,
        };
        let env = launch.environment();
        assert_eq!(
            env["PATH"],
            "/nix/store/x-cargo/bin:/nix/store/a-coreutils/bin"
        );
        assert_eq!(env["CARGO_HOME"], "/custom", "the dev shell's value wins");
        assert_eq!(env["XDG_CACHE_HOME"], "/home/alyosha/.cache");
        assert_eq!(env["SSL_CERT_FILE"], "/nix/store/c-ca/ca.crt");
        assert_eq!(env["HOME"], "/home/alyosha");
        assert_eq!(env["TMPDIR"], "/tmp");
        let with_bash = SandboxConfig {
            bash: Some("/nix/store/b-bash/bin/bash".into()),
            ..config.clone()
        };
        let launch = ShellLaunch {
            config: &with_bash,
            workspace: Path::new("/w"),
            cwd: Path::new("sub"),
            skills_root: None,
        };
        assert_eq!(launch.spec("ls").unwrap().cwd, Path::new("/workspace/sub"));
    }

    #[test]
    fn each_working_folder_has_its_template() {
        let work = Path::new("/var/lib/alyosha/work");
        assert_eq!(
            template_for(work, &work.join("owner")).as_deref(),
            Some("alyosha-run-owner")
        );
        assert_eq!(
            template_for(work, &work.join("groups/friends")).as_deref(),
            Some("alyosha-run-group-friends")
        );
        for other in ["guest", "groups/Bad Name", "owner/sub", "elsewhere"] {
            assert_eq!(template_for(work, &work.join(other)), None, "{other}");
        }
        assert_eq!(template_for(work, Path::new("/tmp/x")), None);
    }

    #[test]
    fn unit_states_give_the_shell_status() {
        let state = UnitState::parse(
            "ActiveState=failed\nSubState=failed\nResult=exit-code\nExecMainCode=1\nExecMainStatus=7\n",
        );
        assert!(state.finished());
        assert_eq!(state.status(), 7);
        let state = UnitState::parse(
            "ActiveState=failed\nSubState=failed\nResult=signal\nExecMainCode=2\nExecMainStatus=15\n",
        );
        assert_eq!(state.status(), 143);
        let running = UnitState::parse("ActiveState=active\nSubState=running\nExecMainCode=0\n");
        assert!(!running.finished());
        let done = UnitState::parse(
            "ActiveState=active\nSubState=exited\nResult=success\nExecMainCode=1\nExecMainStatus=0\n",
        );
        assert!(done.finished() && done.status() == 0);
    }
}
