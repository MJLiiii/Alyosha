//! The `alyosha-telegram` program.

use std::process::ExitCode;

use alyosha_bridge::{Bridge, guests::GroupSpawners};
use alyosha_config::Paths;
use alyosha_schedule::Schedule;
use alyosha_telegram::{Api, Settings, run};

fn main() -> ExitCode {
    let paths = match Paths::from_env() {
        Ok(paths) => paths,
        Err(error) => {
            eprintln!("alyosha-telegram: {error}");
            return ExitCode::from(2);
        }
    };
    let config = match alyosha_config::load_from(&paths.config_file()) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("alyosha-telegram: {error}");
            return ExitCode::from(2);
        }
    };
    let Some(telegram) = config.telegram.clone() else {
        eprintln!(
            "alyosha-telegram: {} has no [telegram] section with owner_id, workspace, and token.",
            paths.config_file().display()
        );
        return ExitCode::from(2);
    };
    let _ = alyosha_config::logging::init("alyosha-telegram", config.timezone, &paths);
    let base =
        std::env::var("ALYOSHA_TELEGRAM_API").unwrap_or_else(|_| "https://api.telegram.org".into());
    let runtime = tokio::runtime::Runtime::new().expect("a Tokio runtime starts");
    let code = runtime.block_on(async {
        let (bridge, events) = match Bridge::spawn(std::path::Path::new("alyosha")) {
            Ok(started) => started,
            Err(error) => {
                eprintln!("alyosha-telegram: could not start `alyosha acp`: {error}");
                return 1;
            }
        };
        let settings = Settings {
            owner_id: telegram.owner_id,
            workspace: telegram.workspace,
            schedule: Schedule::new(&paths.data_dir),
            timezone: config.timezone,
            groups: GroupSpawners {
                guest: Some(std::sync::Arc::new(|| {
                    Bridge::spawn_with(std::path::Path::new("alyosha"), &["acp", "--guest"])
                })),
                trusted: Some(std::sync::Arc::new(|| {
                    Bridge::spawn_with(std::path::Path::new("alyosha"), &["acp", "--group"])
                })),
                trusted_groups: telegram.trusted_groups.iter().map(i64::to_string).collect(),
            },
        };
        run(
            Api::new(&base, telegram.token.expose()),
            bridge,
            events,
            settings,
        )
        .await
    });
    ExitCode::from(code)
}
