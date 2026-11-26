//! The schedule MCP server: `add`, `list`, and `remove` over stdio JSON-RPC.
//!
//! It finds `config.toml` and the task file through `HOME` and the XDG
//! variables, like every Alyosha program. See
//! `docs/Schedule.md#the-mcp-server`.

use std::io::{BufRead, Write};

use alyosha_config::{Config, Paths};
use alyosha_schedule::{Schedule, ScheduleError, When, local, new_task, next_run};
use chrono::Utc;
use serde_json::{Value, json};

fn main() {
    let paths = match Paths::from_env() {
        Ok(paths) => paths,
        Err(error) => {
            eprintln!("alyosha-schedule: {error}");
            std::process::exit(2);
        }
    };
    let config = match alyosha_config::load_from(&paths.config_file()) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("alyosha-schedule: {error}");
            std::process::exit(2);
        }
    };
    let _ = alyosha_config::logging::init("alyosha-schedule", config.timezone, &paths);
    let schedule = Schedule::new(&paths.data_dir);
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(id) = message.get("id").cloned() else {
            continue;
        };
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let result = match message["method"].as_str().unwrap_or_default() {
            "initialize" => json!({
                "protocolVersion": params["protocolVersion"].as_str().unwrap_or("2025-06-18"),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "alyosha-schedule", "version": env!("CARGO_PKG_VERSION")},
            }),
            "ping" => json!({}),
            "tools/list" => json!({"tools": tools()}),
            "tools/call" => call(&schedule, &config, &params),
            other => {
                let reply = json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": format!("method not found: {other}")}});
                let _ = writeln!(stdout, "{reply}");
                let _ = stdout.flush();
                continue;
            }
        };
        let _ = writeln!(
            stdout,
            "{}",
            json!({"jsonrpc": "2.0", "id": id, "result": result})
        );
        let _ = stdout.flush();
    }
}

fn tools() -> Value {
    json!([
        {
            "name": "add",
            "description": "Schedule a task: a prompt Alyosha runs at a set time, delivering the result in a chat app. A reminder is a task too, such as \"Remind the owner to call the bank.\" Give at for one time or cron for a repeating task. Times use the owner's timezone.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "channel": {"type": "string", "enum": ["telegram", "qq"], "description": "The chat app the owner should receive the result in: the one they are writing from, unless they said otherwise."},
                    "prompt": {"type": "string", "minLength": 1, "maxLength": 2000, "description": "What Alyosha should do when the task runs, written as an instruction."},
                    "at": {"type": "string", "description": "For a one-time task: an RFC 3339 date and time with offset, such as 2026-11-28T17:00:00+01:00."},
                    "cron": {"type": "string", "description": "For a repeating task: a five-field cron expression in the owner's timezone, such as \"0 8 * * *\" for every day at 8:00."}
                },
                "required": ["channel", "prompt"],
                "additionalProperties": false
            }
        },
        {
            "name": "list",
            "description": "List every scheduled task with its ID, channel, time or cron, next run, last run, and prompt.",
            "inputSchema": {"type": "object", "additionalProperties": false}
        },
        {
            "name": "remove",
            "description": "Remove a scheduled task by its ID.",
            "inputSchema": {
                "type": "object",
                "properties": {"id": {"type": "string", "description": "The task's six-character ID, from list or add."}},
                "required": ["id"],
                "additionalProperties": false
            }
        }
    ])
}

fn text(content: String, is_error: bool) -> Value {
    json!({"content": [{"type": "text", "text": content}], "isError": is_error})
}

fn channels(config: &Config) -> Vec<&'static str> {
    let mut channels = Vec::new();
    if config.telegram.is_some() {
        channels.push("telegram");
    }
    if config.qq.is_some() {
        channels.push("qq");
    }
    channels
}

fn call(schedule: &Schedule, config: &Config, params: &Value) -> Value {
    let arguments = &params["arguments"];
    let now = Utc::now().with_timezone(&config.timezone);
    let shown = |time: chrono::DateTime<chrono_tz::Tz>| {
        format!(
            "{} ({})",
            time.format("%a %Y-%m-%d %H:%M"),
            config.timezone.name()
        )
    };
    let result: Result<String, ScheduleError> = match params["name"].as_str().unwrap_or_default() {
        "add" => schedule.update(|tasks| {
            let task = new_task(
                tasks,
                arguments["channel"].as_str().unwrap_or_default(),
                arguments["prompt"].as_str().unwrap_or_default(),
                arguments["at"].as_str(),
                arguments["cron"].as_str(),
                &channels(config),
                now,
            )
            .map_err(ScheduleError::Invalid)?;
            let next = next_run(&task, now).map_or_else(|| "never".into(), shown);
            let reply = format!(
                "Added task {} for {}. Next run: {next}.",
                task.id, task.channel
            );
            tasks.push(task);
            Ok(reply)
        }),
        "list" => schedule.read().map(|tasks| {
            if tasks.is_empty() {
                return "No scheduled tasks.".to_owned();
            }
            tasks
                .iter()
                .map(|task| {
                    let time = match &task.when {
                        When::At(at) => format!("at {}", shown(local(config.timezone, *at))),
                        When::Cron(cron) => format!("cron {cron}"),
                    };
                    let next = next_run(task, now).map_or_else(|| "never".into(), shown);
                    let last = task.last_run.map_or_else(
                        || "never".into(),
                        |last| shown(local(config.timezone, last)),
                    );
                    format!(
                        "{} · {} · {time} · next {next} · last {last} · {}",
                        task.id, task.channel, task.prompt
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        }),
        "remove" => {
            let id = arguments["id"].as_str().unwrap_or_default().to_owned();
            schedule.update(|tasks| {
                let before = tasks.len();
                tasks.retain(|task| task.id != id);
                if tasks.len() == before {
                    Err(ScheduleError::Invalid(format!("There is no task {id}.")))
                } else {
                    Ok(format!("Removed task {id}."))
                }
            })
        }
        other => Err(ScheduleError::Invalid(format!("Unknown tool {other}."))),
    };
    match result {
        Ok(reply) => text(reply, false),
        Err(error) => text(error.to_string(), true),
    }
}
