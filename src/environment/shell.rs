//! The shell sandbox: each command runs in a fresh Bubblewrap sandbox inside
//! its own network namespace from `pasta`.
//!
//! `pasta` gives the command outgoing internet without the container's
//! loopback, where GPROXY listens. It closes inherited descriptors, so a small
//! trusted wrapper inside its namespace opens Bubblewrap's status file before
//! starting `bwrap`. See
//! `docs/develop/EnvironmentInterface.md#bubblewrap-launch-contract`.

use std::{
    collections::BTreeMap,
    fs::{self, DirBuilder},
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};

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

/// The programs the sandbox needs, as Nix store paths.
#[derive(Debug, Clone)]
pub struct SandboxPrograms {
    pub bwrap: PathBuf,
    pub pasta: PathBuf,
    pub bash: PathBuf,
}

/// How commands reach DNS.
#[derive(Debug, Clone, Default)]
pub enum Dns {
    /// The host's `resolv.conf` names a non-loopback server; bind it as is.
    #[default]
    Host,
    /// The host resolves through a loopback server such as systemd-resolved:
    /// forward only DNS queries from a fixed address to it.
    Forward { host: String },
}

/// The address inside the sandbox that `pasta` forwards to the host's DNS.
pub const DNS_FORWARD_ADDRESS: &str = "169.254.0.53";

/// The session's sandbox settings.
#[derive(Debug, Clone, Default)]
pub struct SandboxConfig {
    /// `None` when the sandbox programs were not found: commands fail with a
    /// tool error.
    pub programs: Option<SandboxPrograms>,
    /// The directories of the default tools: Bash, coreutils, findutils, Git,
    /// and ripgrep.
    pub default_path: Vec<PathBuf>,
    /// Exported variables of the workspace's dev shell.
    pub dev_shell: Vec<(String, String)>,
    /// The persistent cache bound at `/home/alyosha/.cache`.
    pub cache_dir: PathBuf,
    /// A private folder for status files and the generated `resolv.conf`.
    pub private_dir: PathBuf,
    pub dns: Dns,
    /// The CA bundle's real path in the Nix store, if found.
    pub ca_bundle: Option<PathBuf>,
}

impl SandboxConfig {
    /// Finds the sandbox programs and default tools on `PATH`, which Alyosha's
    /// package sets to their store paths.
    pub fn discover(cache_dir: PathBuf, private_dir: PathBuf) -> Self {
        let find = |name: &str| which(name).and_then(|path| path.canonicalize().ok());
        // pasta picks its mode from the name it runs under, so keep its link.
        let programs = match (find("bwrap"), which("pasta"), find("bash")) {
            (Some(bwrap), Some(pasta), Some(bash)) => Some(SandboxPrograms { bwrap, pasta, bash }),
            _ => None,
        };
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
            programs,
            default_path,
            dev_shell: Vec::new(),
            cache_dir,
            private_dir,
            dns: host_dns(&fs::read_to_string("/etc/resolv.conf").unwrap_or_default()),
            ca_bundle,
        }
    }
}

fn which(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join(name))
            .find(|candidate| candidate.is_file())
    })
}

/// Reads the host's nameservers. Loopback servers cannot be reached from the
/// sandbox's own loopback, so DNS is forwarded to them.
pub fn host_dns(resolv_conf: &str) -> Dns {
    let servers: Vec<&str> = resolv_conf
        .lines()
        .filter_map(|line| line.trim().strip_prefix("nameserver"))
        .map(str::trim)
        .collect();
    let loopback = |server: &&str| server.starts_with("127.") || *server == "::1";
    if servers.iter().any(|server| !loopback(server)) || servers.is_empty() {
        Dns::Host
    } else {
        Dns::Forward {
            host: servers[0].to_owned(),
        }
    }
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

/// What one sandboxed command needs from the session.
pub struct ShellLaunch<'a> {
    pub config: &'a SandboxConfig,
    pub workspace: &'a Path,
    /// The working directory, relative to the workspace.
    pub cwd: &'a Path,
    pub skills_root: Option<&'a Path>,
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

    /// The full launch: `pasta`, the status wrapper, then `bwrap`.
    fn command(
        &self,
        programs: &SandboxPrograms,
        status_file: &Path,
        shell_command: &str,
    ) -> std::io::Result<Command> {
        let config = self.config;
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&config.cache_dir)?;
        let mut pasta_args: Vec<String> = [
            "--config-net",
            "--quiet",
            "--no-map-gw",
            "-t",
            "none",
            "-u",
            "none",
            "-T",
            "none",
            "-U",
            "none",
        ]
        .map(String::from)
        .to_vec();
        let mut bwrap_args: Vec<String> = Vec::new();
        let mut push = |args: &[&str]| bwrap_args.extend(args.iter().map(|arg| (*arg).to_owned()));
        let uid = rustix::process::getuid().as_raw().to_string();
        let gid = rustix::process::getgid().as_raw().to_string();
        push(&[
            "--unshare-user",
            "--unshare-pid",
            "--unshare-ipc",
            "--new-session",
            "--die-with-parent",
        ]);
        push(&["--uid", &uid, "--gid", &gid]);
        push(&["--ro-bind", "/nix/store", "/nix/store"]);
        push(&[
            "--bind",
            &self.workspace.display().to_string(),
            "/workspace",
        ]);
        if let Some(skills) = self.skills_root {
            push(&["--ro-bind", &skills.display().to_string(), "/skills"]);
        }
        push(&["--tmpfs", "/tmp", "--dir", "/home/alyosha"]);
        push(&[
            "--bind",
            &config.cache_dir.display().to_string(),
            "/home/alyosha/.cache",
        ]);
        push(&["--dev", "/dev", "--proc", "/proc"]);
        match &config.dns {
            Dns::Host => push(&["--ro-bind-try", "/etc/resolv.conf", "/etc/resolv.conf"]),
            Dns::Forward { host } => {
                let resolv = config.private_dir.join("resolv.conf");
                fs::write(&resolv, format!("nameserver {DNS_FORWARD_ADDRESS}\n"))?;
                pasta_args.extend([
                    "--dns-forward".into(),
                    DNS_FORWARD_ADDRESS.into(),
                    "--dns-host".into(),
                    host.clone(),
                ]);
                push(&[
                    "--ro-bind",
                    &resolv.display().to_string(),
                    "/etc/resolv.conf",
                ]);
            }
        }
        if let Some(bundle) = &config.ca_bundle {
            push(&[
                "--ro-bind",
                &bundle.display().to_string(),
                "/etc/ssl/certs/ca-certificates.crt",
            ]);
        }
        let cwd = Path::new("/workspace").join(self.cwd);
        push(&["--chdir", &cwd.display().to_string(), "--clearenv"]);
        for (name, value) in self.environment() {
            push(&["--setenv", &name, &value]);
        }
        push(&["--json-status-fd", "3", "--"]);
        push(&[
            &programs.bash.display().to_string(),
            "--noprofile",
            "--norc",
            "-c",
            shell_command,
        ]);

        let mut command = Command::new(&programs.pasta);
        command
            .args(&pasta_args)
            .arg("--")
            .arg(&programs.bash)
            .args([
                "--noprofile",
                "--norc",
                "-c",
                "exec 3>\"$0\" && exec \"$@\"",
            ])
            .arg(status_file)
            .arg(&programs.bwrap)
            .args(&bwrap_args)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true);
        // SAFETY: setpriority is async-signal-safe and touches no memory.
        unsafe {
            command.pre_exec(|| {
                rustix::process::setpriority_process(None, 10)?;
                Ok(())
            });
        }
        Ok(command)
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

/// Runs one command in a fresh sandbox, with a deadline that covers setup.
/// Stop and the deadline kill the whole sandbox and return the output so far.
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
        Err(LaunchFailure(message)) => {
            let diagnostic = String::from_utf8_lossy(&output);
            let last = diagnostic
                .lines()
                .rev()
                .find(|line| !line.trim().is_empty())
                .unwrap_or(&message);
            Err(ToolError::failed(format!(
                "The sandbox could not start: {last}"
            )))
        }
    }
}

/// The sandbox did not run the command.
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
    let Some(programs) = &launch.config.programs else {
        return Err(LaunchFailure(
            "the shell sandbox is unavailable: bwrap, pasta, or bash was not found".into(),
        ));
    };
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&launch.config.private_dir)
        .map_err(|error| LaunchFailure(error.to_string()))?;
    let status_file = launch
        .config
        .private_dir
        .join(format!("status-{}.json", super::random_suffix()));
    let mut child = launch
        .command(programs, &status_file, command)
        .and_then(|mut command| command.spawn())
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
    let ended = tokio::select! {
        status = child.wait() => {
            status.map_err(|error| LaunchFailure(error.to_string()))?;
            None
        }
        () = tokio::time::sleep_until(deadline) => Some(ShellStatus::TimedOut),
        () = cancel.cancelled() => Some(ShellStatus::Stopped),
    };
    if ended.is_some() {
        if let Some(pid) = pid.and_then(|pid| rustix::process::Pid::from_raw(pid as i32)) {
            let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
        }
        let _ = child.wait().await;
    }
    // The sandbox's processes are gone, so the pipes close; don't wait forever
    // on a descriptor something else still holds.
    for reader in readers {
        let _ = tokio::time::timeout(Duration::from_secs(2), reader).await;
    }
    let record = fs::read_to_string(&status_file).unwrap_or_default();
    let _ = fs::remove_file(&status_file);
    if let Some(status) = ended {
        return Ok(status);
    }
    exit_code(&record)
        .map(ShellStatus::Exited)
        .ok_or_else(|| LaunchFailure("no message".into()))
}

/// The command's exit code from Bubblewrap's status records.
fn exit_code(records: &str) -> Option<i32> {
    records.lines().find_map(|line| {
        let value: serde_json::Value = serde_json::from_str(line).ok()?;
        value["exit-code"].as_i64().map(|code| code as i32)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dns_is_forwarded_only_for_loopback_resolvers() {
        assert!(
            matches!(host_dns("nameserver 127.0.0.53\noptions edns0\n"), Dns::Forward { host } if host == "127.0.0.53")
        );
        assert!(matches!(
            host_dns("nameserver 10.0.0.1\nnameserver 127.0.0.1\n"),
            Dns::Host
        ));
        assert!(matches!(host_dns(""), Dns::Host));
    }

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
            ..SandboxConfig::default()
        };
        let launch = ShellLaunch {
            config: &config,
            workspace: Path::new("/w"),
            cwd: Path::new(""),
            skills_root: None,
        };
        let env = launch.environment();
        assert_eq!(
            env["PATH"],
            "/nix/store/x-cargo/bin:/nix/store/a-coreutils/bin"
        );
        assert_eq!(env["CARGO_HOME"], "/custom", "the dev shell's value wins");
        assert_eq!(env["XDG_CACHE_HOME"], "/home/alyosha/.cache");
        assert_eq!(env["HOME"], "/home/alyosha");
        assert_eq!(env["TMPDIR"], "/tmp");
    }

    #[test]
    fn exit_codes_come_from_the_status_records() {
        assert_eq!(
            exit_code("{\"child-pid\": 2}\n{ \"exit-code\": 7 }\n"),
            Some(7)
        );
        assert_eq!(exit_code("{\"child-pid\": 2}\n"), None);
    }
}
