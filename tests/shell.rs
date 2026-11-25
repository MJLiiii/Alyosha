//! Shell sandbox checks that start the real Bubblewrap and pasta. They need
//! `bwrap`, `pasta`, `bash`, and the default tools on `PATH`, unprivileged user
//! namespaces, and the Nix store; without them each check is skipped with a
//! message. See `docs/develop/EnvironmentInterface.md#verify-on-the-host`.

use std::{fs, path::Path, time::Duration};

use alyosha::environment::shell::{SandboxConfig, ShellLaunch, ShellStatus, run_shell};
use tokio_util::sync::CancellationToken;

struct Sandbox {
    _root: tempfile::TempDir,
    workspace: std::path::PathBuf,
    skills: std::path::PathBuf,
    config: SandboxConfig,
}

fn sandbox() -> Option<Sandbox> {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("ws");
    let skills = root.path().join("skills");
    fs::create_dir_all(&workspace).unwrap();
    fs::create_dir_all(skills.join("tool")).unwrap();
    fs::write(skills.join("tool/run.sh"), "echo skill ran\n").unwrap();
    let config = SandboxConfig::discover(root.path().join("cache"), root.path().join("private"));
    if config.programs.is_none() {
        eprintln!("skipped: bwrap, pasta, or bash is not on PATH");
        return None;
    }
    Some(Sandbox {
        workspace: workspace.canonicalize().unwrap(),
        skills,
        config,
        _root: root,
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
        skills_root: Some(&sandbox.skills),
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

#[tokio::test]
async fn commands_see_the_workspace_and_report_their_status() {
    let Some(sandbox) = sandbox() else { return };
    fs::write(sandbox.workspace.join("notes.md"), "inside\n").unwrap();
    let (status, output) = quick(&sandbox, "pwd; cat notes.md; echo made > new.txt; exit 3").await;
    assert_eq!(status, ShellStatus::Exited(3));
    assert_eq!(output, "/workspace\ninside\n");
    assert_eq!(
        fs::read_to_string(sandbox.workspace.join("new.txt")).unwrap(),
        "made\n"
    );

    let (status, output) = quick(&sandbox, "kill -TERM $$").await;
    assert_eq!(status, ShellStatus::Exited(128 + 15), "{output}");
}

#[tokio::test]
async fn the_sandbox_has_its_documented_shape() {
    let Some(sandbox) = sandbox() else { return };
    let (status, output) = quick(
        &sandbox,
        "nice; echo HOME=$HOME TMPDIR=$TMPDIR; ls -A /home/alyosha; \
         test -e /nix/var && echo nix-var; test -e /nix/var/nix/daemon-socket && echo socket; \
         bash /skills/tool/run.sh; touch /skills/x 2>/dev/null || echo skills-read-only; \
         git --version >/dev/null && rg --version >/dev/null && echo tools",
    )
    .await;
    assert_eq!(status, ShellStatus::Exited(0), "{output}");
    assert_eq!(
        output,
        "10\nHOME=/home/alyosha TMPDIR=/tmp\n.cache\nskill ran\nskills-read-only\ntools\n"
    );
}

#[tokio::test]
async fn the_cache_persists_between_commands() {
    let Some(sandbox) = sandbox() else { return };
    quick(
        &sandbox,
        "mkdir -p $CARGO_HOME && echo kept > $CARGO_HOME/marker",
    )
    .await;
    let (_, output) = quick(
        &sandbox,
        "cat /home/alyosha/.cache/cargo/marker; ls /tmp | wc -l",
    )
    .await;
    assert_eq!(output, "kept\n0\n");
}

#[tokio::test]
async fn the_network_reaches_out_but_not_the_loopback() {
    let Some(sandbox) = sandbox() else { return };
    // A listener on the host's loopback stands in for GPROXY.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (_, output) = quick(
        &sandbox,
        &format!(
            "(exec 3<>/dev/tcp/127.0.0.1/{port}) 2>/dev/null && echo loopback-open || echo loopback-closed; \
             getent hosts example.com >/dev/null 2>&1 || (exec 3<>/dev/tcp/example.com/443) 2>/dev/null && echo internet || echo offline"
        ),
    )
    .await;
    assert!(output.starts_with("loopback-closed\n"), "{output}");
    if output.contains("offline") {
        eprintln!("note: no internet from the sandbox on this machine");
    }
}

#[tokio::test]
async fn a_timeout_kills_the_whole_sandbox() {
    let Some(sandbox) = sandbox() else { return };
    let marker = sandbox.workspace.join("late");
    let started = std::time::Instant::now();
    let (status, output) = run(
        &sandbox,
        "(sleep 3; touch late) & echo started; sleep 60",
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

#[tokio::test]
async fn stop_kills_the_command_and_keeps_its_output() {
    let Some(sandbox) = sandbox() else { return };
    let cancel = CancellationToken::new();
    let stopper = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(700)).await;
        stopper.cancel();
    });
    let started = std::time::Instant::now();
    let (status, output) = run(
        &sandbox,
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
async fn a_failed_setup_is_a_tool_error() {
    let Some(mut sandbox) = sandbox() else { return };
    sandbox.workspace = sandbox.workspace.join("missing");
    let launch = ShellLaunch {
        config: &sandbox.config,
        workspace: &sandbox.workspace,
        cwd: Path::new(""),
        skills_root: None,
    };
    let error = run_shell(
        &launch,
        "echo hi",
        Duration::from_secs(30),
        &CancellationToken::new(),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("could not start"), "{error}");
}
