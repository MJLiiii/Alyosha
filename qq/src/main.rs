//! The `alyosha-qq` program.

use std::{process::ExitCode, sync::Arc};

use alyosha_bridge::Bridge;
use alyosha_config::Paths;
use alyosha_qq::gateway::{QqApi, Settings, listen, run};
use alyosha_schedule::Schedule;

fn main() -> ExitCode {
    let paths = match Paths::from_env() {
        Ok(paths) => paths,
        Err(error) => {
            eprintln!("alyosha-qq: {error}");
            return ExitCode::from(2);
        }
    };
    let config = match alyosha_config::load_from(&paths.config_file()) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("alyosha-qq: {error}");
            return ExitCode::from(2);
        }
    };
    let Some(qq) = config.qq.clone() else {
        eprintln!(
            "alyosha-qq: {} has no [qq] section with app_id, secret, and workspace.",
            paths.config_file().display()
        );
        return ExitCode::from(2);
    };
    let _ = alyosha_config::logging::init("alyosha-qq", config.timezone, &paths);
    if qq.owner_openid.is_empty() {
        tracing_note();
    }
    let api_base =
        std::env::var("ALYOSHA_QQ_API").unwrap_or_else(|_| "https://api.bot.qq.com".into());
    let token_url = std::env::var("ALYOSHA_QQ_TOKEN_URL")
        .unwrap_or_else(|_| "https://api.bot.qq.com/app/getAppAccessToken".into());
    let runtime = tokio::runtime::Runtime::new().expect("a Tokio runtime starts");
    let code = runtime.block_on(async {
        let api = Arc::new(QqApi::new(
            &api_base,
            &token_url,
            &qq.app_id,
            qq.secret.expose(),
        ));
        if let Err(error) = api.token().await {
            eprintln!("alyosha-qq: {error}");
            return 1;
        }
        let (bridge, events) = match Bridge::spawn(std::path::Path::new("alyosha")) {
            Ok(started) => started,
            Err(error) => {
                eprintln!("alyosha-qq: could not start `alyosha acp`: {error}");
                return 1;
            }
        };
        let (messages_sender, messages) = tokio::sync::mpsc::unbounded_channel();
        let gateway_api = api.clone();
        let gateway = tokio::spawn(async move { listen(gateway_api, messages_sender).await });
        let settings = Settings {
            owner_openid: qq.owner_openid,
            workspace: qq.workspace,
            reply_chunk: qq.reply_chunk as usize,
            schedule: Schedule::new(&paths.data_dir),
            timezone: config.timezone,
        };
        tokio::select! {
            code = run(api, bridge, events, messages, settings) => code,
            error = gateway => {
                if let Ok(error) = error {
                    eprintln!("alyosha-qq: {error}");
                }
                1
            }
        }
    });
    ExitCode::from(code)
}

fn tracing_note() {
    eprintln!(
        "alyosha-qq: owner_openid is empty, so the bridge answers nobody. Send the bot a message and copy your user_openid from the log."
    );
}
