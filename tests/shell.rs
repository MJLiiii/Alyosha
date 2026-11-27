//! Shell command checks. The first group runs commands as plain child
//! processes (`Runner::Direct`) and checks statuses, output, deadlines, and
//! Stop anywhere `bash` is on `PATH`. The second starts real command units and
//! runs only in the container, as `alyosha`, with `ALYOSHA_TEST_UNITS` naming
//! a template such as `alyosha-run-owner` and `ALYOSHA_TEST_WORKSPACE` its
//! working folder. See `docs/EnvironmentInterface.md#verify-on-the-host`.

use std::{fs, path::Path, time::Duration};

use alyosha::environment::shell::{
    RUNS_DIR, Runner, SandboxConfig, ShellLaunch, ShellStatus, run_shell,
};
use tokio_util::sync::CancellationToken;

struct Sandbox {
    _root: Option<tempfile::TempDir>,
    workspace: std::path::PathBuf,
    config: SandboxConfig,
}

/// Commands as plain child processes in a temporary workspace.
fn direct() -> Option<Sandbox> {
    let config = SandboxConfig::discover(Runner::Direct);
    if config.bash.is_none() {
        eprintln!("skipped: bash is not on PATH");
        return None;
    }
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("ws");
    fs::create_dir_all(&workspace).unwrap();
    Some(Sandbox {
        workspace: workspace.canonicalize().unwrap(),
        config,
        _root: Some(root),
    })
}

/// Real command units, in the container only.
fn units() -> Option<Sandbox> {
    let (Ok(template), Ok(workspace)) = (
        std::env::var("ALYOSHA_TEST_UNITS"),
        std::env::var("ALYOSHA_TEST_WORKSPACE"),
    ) else {
        eprintln!("skipped: ALYOSHA_TEST_UNITS and ALYOSHA_TEST_WORKSPACE are not set");
        return None;
    };
    Some(Sandbox {
        workspace: workspace.into(),
        config: SandboxConfig::discover(Runner::Units {
            template,
            runs_dir: RUNS_DIR.into(),
        }),
        _root: None,
    })
}

async fn run(
    sandbox: &Sandbox,
    command: &str,
    timeout: Duration,
    cancel: &CancellationToken,
) -> (ShellStatus, String) {
    let launch = ShellLaunch {
        config: &sandbox.config,
        workspace: &sandbox.workspace,
        cwd: Path::new(""),
        skills_root: None,
    };
    let output = run_shell(&launch, command, timeout, cancel).await.unwrap();
    (
        output.status,
        String::from_utf8_lossy(&output.output).into_owned(),
    )
}

async fn quick(sandbox: &Sandbox, command: &str) -> (ShellStatus, String) {
    run(
        sandbox,
        command,
        Duration::from_secs(60),
        &CancellationToken::new(),
    )
    .await
}

async fn reports_status_and_output(sandbox: &Sandbox) {
    let (status, output) = quick(sandbox, "echo out; echo err >&2; exit 3").await;
    assert_eq!(status, ShellStatus::Exited(3));
    assert_eq!(output, "out\nerr\n");
    let (status, output) = quick(sandbox, "kill -TERM $$").await;
    assert_eq!(status, ShellStatus::Exited(128 + 15), "{output}");
}

async fn a_timeout_kills_every_process(sandbox: &Sandbox, marker: &Path) {
    let started = std::time::Instant::now();
    let (status, output) = run(
        sandbox,
        &format!(
            "(sleep 3; touch {}) & echo started; sleep 60",
            marker.display()
        ),
        Duration::from_secs(1),
        &CancellationToken::new(),
    )
    .await;
    assert_eq!(status, ShellStatus::TimedOut);
    assert_eq!(output, "started\n");
    assert!(started.elapsed() < Duration::from_secs(5));
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(!marker.exists(), "the background child was killed");
}

async fn stop_keeps_the_output(sandbox: &Sandbox) {
    let cancel = CancellationToken::new();
    let stopper = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(700)).await;
        stopper.cancel();
    });
    let started = std::time::Instant::now();
    let (status, output) = run(
        sandbox,
        "echo before; sleep 600",
        Duration::from_secs(900),
        &cancel,
    )
    .await;
    assert_eq!(status, ShellStatus::Stopped);
    assert_eq!(output, "before\n");
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn direct_commands_report_status_and_output() {
    let Some(sandbox) = direct() else { return };
    reports_status_and_output(&sandbox).await;
    let (_, output) = quick(&sandbox, "pwd").await;
    assert_eq!(output.trim(), sandbox.workspace.display().to_string());
}

#[tokio::test]
async fn a_direct_timeout_kills_the_background_child() {
    let Some(sandbox) = direct() else { return };
    let marker = sandbox.workspace.join("late");
    a_timeout_kills_every_process(&sandbox, &marker).await;
}

#[tokio::test]
async fn direct_stop_keeps_the_output() {
    let Some(sandbox) = direct() else { return };
    stop_keeps_the_output(&sandbox).await;
}

#[tokio::test]
async fn unavailable_commands_are_a_tool_error() {
    let mut sandbox = SandboxConfig::discover(Runner::Unavailable("no units here".into()));
    sandbox.runner = Runner::Unavailable("no units here".into());
    let root = tempfile::tempdir().unwrap();
    let launch = ShellLaunch {
        config: &sandbox,
        workspace: root.path(),
        cwd: Path::new(""),
        skills_root: None,
    };
    let error = run_shell(
        &launch,
        "echo hi",
        Duration::from_secs(5),
        &CancellationToken::new(),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("no units here"), "{error}");
}

#[tokio::test]
async fn units_report_status_and_output() {
    let Some(sandbox) = units() else { return };
    reports_status_and_output(&sandbox).await;
}

#[tokio::test]
async fn a_unit_has_its_documented_shape() {
    let Some(sandbox) = units() else { return };
    let (status, output) = quick(
        &sandbox,
        "pwd; nice; id -un; echo HOME=$HOME; ls /proc | grep -c '^[0-9]'; \
         test -e /var/lib/alyosha-secrets && echo secrets; test -e /run/postgresql && echo db; \
         test -e /nix/var/nix/daemon-socket && echo socket; touch /skills/x 2>/dev/null && echo skills-writable; \
         echo made > made.txt && stat -c %U made.txt && rm made.txt",
    )
    .await;
    assert_eq!(status, ShellStatus::Exited(0), "{output}");
    let lines: Vec<&str> = output.lines().collect();
    assert_eq!(lines[0], "/workspace");
    assert_eq!(lines[1], "10");
    assert_eq!(lines[2], "alyosha-run");
    assert_eq!(lines[3], "HOME=/home/alyosha");
    assert!(
        lines[4].parse::<u32>().unwrap() <= 4,
        "only the unit's processes: {output}"
    );
    assert_eq!(lines[5..], ["alyosha-run"], "{output}");
}

#[tokio::test]
async fn a_unit_reaches_the_internet_but_not_the_loopback() {
    let Some(sandbox) = units() else { return };
    let (_, output) = quick(
        &sandbox,
        "(exec 3<>/dev/tcp/127.0.0.1/18787) 2>/dev/null && echo loopback-open || echo loopback-closed; \
         (exec 3<>/dev/tcp/1.1.1.1/443) 2>/dev/null && echo internet || echo offline",
    )
    .await;
    assert_eq!(output, "loopback-closed\ninternet\n");
}

#[tokio::test]
async fn a_unit_timeout_kills_every_process() {
    let Some(sandbox) = units() else { return };
    let marker = sandbox.workspace.join("late");
    let _ = fs::remove_file(&marker);
    a_timeout_kills_every_process(&sandbox, Path::new("/workspace/late")).await;
    assert!(!marker.exists());
}

#[tokio::test]
async fn unit_stop_keeps_the_output() {
    let Some(sandbox) = units() else { return };
    stop_keeps_the_output(&sandbox).await;
}
