//! The `alyosha` program in the container: `alyosha acp`, the gateway
//! commands, the configuration check, and `run-exec`, which a command's unit
//! runs. You talk to Alyosha through the Telegram and QQ bridges, which run
//! `alyosha acp`. The host's own `alyosha` command is a different program, in
//! the container module.

use std::{path::Path, process::ExitCode};

use alyosha::{
    acp::Mode,
    gateway_admin::{self, GatewayFiles, StdTerminal},
};
use alyosha_config::Paths;

const USAGE: &str = "\
Usage:
  alyosha acp             Serve one ACP agent connection on stdin and stdout
  alyosha acp --guest     The same, as a group chat's guest session
  alyosha acp --group     The same, as a trusted group chat's full session
  alyosha gateway init    Write GPROXY's secrets file
  alyosha gateway login   Set up the model provider and sign in
  alyosha config check FILE
                          Check a configuration file, as the container module does
  alyosha run-exec RUN    Inside a command's unit: run the command its run folder holds
  alyosha check-units     As `alyosha` in the container: check the command units";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        ["-h" | "--help"] => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        ["gateway", "init"] => gateway_init(),
        ["gateway", "login"] => gateway_login(),
        ["gateway", ..] => usage_error(),
        ["acp"] => ExitCode::from(alyosha::acp::run(Mode::Owner)),
        ["acp", "--guest"] => ExitCode::from(alyosha::acp::run(Mode::Guest)),
        ["acp", "--group"] => ExitCode::from(alyosha::acp::run(Mode::Group)),
        ["config", "check", file] => config_check(Path::new(file)),
        ["check-units"] => check_units(),
        ["run-exec", instance] => {
            let error = alyosha::environment::shell::run_exec(
                Path::new(alyosha::environment::shell::RUNS_DIR),
                instance,
            );
            fail(&format!("could not start the command: {error}"))
        }
        _ => usage_error(),
    }
}

fn usage_error() -> ExitCode {
    eprintln!("{USAGE}");
    ExitCode::from(2)
}

fn fail(message: &str) -> ExitCode {
    eprintln!("alyosha: {message}");
    ExitCode::from(2)
}

fn paths() -> Result<Paths, ExitCode> {
    Paths::from_env().map_err(|error| fail(&error.to_string()))
}

fn gateway_init() -> ExitCode {
    let paths = match paths() {
        Ok(paths) => paths,
        Err(code) => return code,
    };
    match gateway_admin::init(&GatewayFiles::new(&paths)) {
        Ok(message) => {
            println!("{message}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("alyosha: {error}");
            ExitCode::FAILURE
        }
    }
}

fn gateway_login() -> ExitCode {
    let paths = match paths() {
        Ok(paths) => paths,
        Err(code) => return code,
    };
    let config = match alyosha_config::load_from(&paths.config_file) {
        Ok(config) => config,
        Err(error) => return fail(&error.to_string()),
    };
    let runtime = tokio::runtime::Runtime::new().expect("a Tokio runtime starts");
    let result = runtime.block_on(gateway_admin::login(
        &config,
        &GatewayFiles::new(&paths),
        &mut StdTerminal,
    ));
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("alyosha: {error}");
            ExitCode::FAILURE
        }
    }
}

fn config_check(file: &Path) -> ExitCode {
    if !file.is_file() {
        return fail(&format!("{} does not exist", file.display()));
    }
    match alyosha_config::load_from(file) {
        Ok(_) => ExitCode::SUCCESS,
        Err(error) => fail(&error.to_string()),
    }
}

/// The container checks that need no model account: runs probes in the
/// owner's command units and prints one line per check.
/// See `docs/Deployment.md#verification`.
fn check_units() -> ExitCode {
    use alyosha::environment::shell::{
        RUNS_DIR, Runner, SandboxConfig, ShellLaunch, ShellStatus, run_shell,
    };
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    let paths = match paths() {
        Ok(paths) => paths,
        Err(code) => return code,
    };
    let workspace = paths.owner_work();
    let config = SandboxConfig::discover(Runner::Units {
        template: "alyosha-run-owner".into(),
        runs_dir: RUNS_DIR.into(),
    });
    let runtime = tokio::runtime::Runtime::new().expect("a Tokio runtime starts");
    let run = |command: &str, seconds: u64| {
        let launch = ShellLaunch {
            config: &config,
            workspace: &workspace,
            cwd: Path::new(""),
            skills_root: None,
        };
        runtime.block_on(run_shell(
            &launch,
            command,
            Duration::from_secs(seconds),
            &CancellationToken::new(),
        ))
    };
    let mut failed = 0;
    let mut check = |name: &str, ok: bool, detail: String| {
        if ok {
            println!("ok    {name}");
        } else {
            failed += 1;
            println!("FAIL  {name}: {detail}");
        }
    };

    match run("echo out; echo err >&2; exit 7", 60) {
        Ok(output) => check(
            "a command's status and output come back",
            output.status == ShellStatus::Exited(7) && output.output == b"out\nerr\n",
            format!(
                "{:?} {:?}",
                output.status,
                String::from_utf8_lossy(&output.output)
            ),
        ),
        Err(error) => check("a command's unit starts", false, error.to_string()),
    }
    let probe = "id -un; pwd; ls /proc | grep -c '^[0-9]'; \
                 for path in /var/lib/alyosha-secrets /var/lib/gproxy /run/postgresql \
                   /nix/var/nix/daemon-socket /var/lib/alyosha/work/groups; do \
                   test -e $path && echo visible:$path; done; \
                 (exec 3<>/dev/tcp/127.0.0.1/18787) 2>/dev/null && echo reached:gproxy; \
                 (exec 3<>/dev/tcp/10.233.1.1/22) 2>/dev/null && echo reached:host; \
                 (timeout 5 bash -c 'exec 3<>/dev/tcp/1.1.1.1/443') 2>/dev/null || echo offline; \
                 echo written > check-units.txt";
    match run(probe, 60) {
        Ok(output) => {
            let text = String::from_utf8_lossy(&output.output).into_owned();
            let lines: Vec<&str> = text.lines().collect();
            check(
                "a command runs as alyosha-run in /workspace",
                lines.first() == Some(&"alyosha-run") && lines.get(1) == Some(&"/workspace"),
                text.clone(),
            );
            check(
                "a command sees only its own processes",
                lines
                    .get(2)
                    .and_then(|count| count.parse::<u32>().ok())
                    .is_some_and(|count| count <= 6),
                text.clone(),
            );
            let leaks: Vec<&&str> = lines
                .iter()
                .filter(|line| line.starts_with("visible:") || line.starts_with("reached:"))
                .collect();
            check(
                "secrets, GPROXY, the database, and the host are out of reach",
                leaks.is_empty(),
                format!("{leaks:?}"),
            );
            check(
                "a command reaches the internet",
                !lines.contains(&"offline"),
                text.clone(),
            );
        }
        Err(error) => check("the probe runs", false, error.to_string()),
    }
    let shared = workspace.join("check-units.txt");
    let agent_side = std::fs::read_to_string(&shared)
        .and_then(|text| std::fs::write(&shared, format!("{text}edited\n")));
    let back = run("cat check-units.txt; rm check-units.txt", 30)
        .map(|output| String::from_utf8_lossy(&output.output).into_owned());
    check(
        "the agent and commands share the working folder's files",
        agent_side.is_ok()
            && back
                .as_deref()
                .is_ok_and(|text| text == "written\nedited\n"),
        format!("{agent_side:?} {back:?}"),
    );
    let marker = workspace.join("check-units-late");
    let _ = std::fs::remove_file(&marker);
    let timed = run("(sleep 2; touch check-units-late) & sleep 60", 1);
    std::thread::sleep(Duration::from_secs(3));
    check(
        "a timeout stops the command and its children",
        matches!(&timed, Ok(output) if output.status == ShellStatus::TimedOut) && !marker.exists(),
        format!("{:?}", timed.map(|output| output.status)),
    );
    let refused = std::process::Command::new("systemctl")
        .args([
            "--no-ask-password",
            "start",
            "systemd-tmpfiles-clean.service",
        ])
        .output()
        .map(|output| !output.status.success())
        .unwrap_or(false);
    check(
        "the agent cannot start any other unit",
        refused,
        "systemctl start systemd-tmpfiles-clean.service succeeded".into(),
    );
    if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
