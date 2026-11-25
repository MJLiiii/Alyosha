//! The `alyosha` program: the TUI, `alyosha acp`, and the gateway commands.

use std::process::ExitCode;

use alyosha::gateway_admin::{self, GatewayFiles, StdTerminal};
use alyosha_config::Paths;

const USAGE: &str = "\
Usage:
  alyosha [path]          Start the TUI in a folder (default: here)
  alyosha acp             Serve one ACP agent connection on stdin and stdout
  alyosha acp --guest     The same, as a group chat's guest session
  alyosha gateway init    Create the GPROXY secrets file and data folder
  alyosha gateway login   Set up the model provider and sign in";

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
        ["acp"] => ExitCode::from(alyosha::acp::run(false)),
        ["acp", "--guest"] => ExitCode::from(alyosha::acp::run(true)),
        [] => ExitCode::from(alyosha::tui::run(None)),
        [path] if !path.starts_with('-') => {
            ExitCode::from(alyosha::tui::run(Some((*path).to_owned())))
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
    let config = match alyosha_config::load_from(&paths.config_file()) {
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
