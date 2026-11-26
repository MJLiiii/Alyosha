//! The task runner each bridge starts for its own channel.
//!
//! Once a minute, just after the minute turns, it takes the lock, reads the
//! task file, and starts this channel's due tasks as prompts in the bridge's
//! session. See `docs/Schedule.md#the-runner`.

use std::{collections::HashSet, sync::Arc, time::Duration};

use alyosha_schedule::{DueTask, Schedule, take_due};
use chrono::{DateTime, Timelike, Utc};
use chrono_tz::Tz;

use crate::Bridge;

/// The prompt key of a scheduled task.
pub fn task_key(id: &str) -> String {
    format!("task:{id}")
}

/// Runs one check: starts due tasks, showing each first. Returns the tasks
/// started.
pub fn check(
    schedule: &Schedule,
    channel: &str,
    now: DateTime<Tz>,
    bridge: &Bridge,
    show: &(dyn Fn(&DueTask) + Send + Sync),
) -> Vec<DueTask> {
    let open: HashSet<String> = bridge
        .open_keys()
        .into_iter()
        .filter_map(|key| key.strip_prefix("task:").map(str::to_owned))
        .collect();
    let due = match schedule.update(|tasks| Ok(take_due(tasks, channel, now, &open))) {
        Ok(due) => due,
        Err(error) => {
            tracing::warn!(error = %error, "the task file could not be read; running nothing");
            return Vec::new();
        }
    };
    for task in &due {
        show(task);
        bridge.prompt(&task_key(&task.id), &task.prompt);
    }
    due
}

/// Checks every minute, just after it turns, until the program exits.
pub async fn run(
    schedule: Schedule,
    channel: String,
    timezone: Tz,
    bridge: Bridge,
    show: Arc<dyn Fn(&DueTask) + Send + Sync>,
) {
    loop {
        let now = Utc::now();
        let wait = 61 - u64::from(now.second());
        tokio::time::sleep(Duration::from_secs(wait)).await;
        check(
            &schedule,
            &channel,
            Utc::now().with_timezone(&timezone),
            &bridge,
            show.as_ref(),
        );
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use alyosha_schedule::new_task;
    use tokio::io::AsyncBufReadExt;

    use super::*;

    #[tokio::test]
    async fn a_due_task_is_shown_then_prompted() {
        let root = tempfile::tempdir().unwrap();
        let schedule = Schedule::new(root.path());
        let tz = chrono_tz::Europe::Amsterdam;
        let created = DateTime::parse_from_rfc3339("2026-11-25T10:02:00+01:00")
            .unwrap()
            .with_timezone(&tz);
        schedule
            .update(|tasks| {
                let task = new_task(
                    tasks,
                    "telegram",
                    "Stretch.",
                    Some("2026-11-25T10:12:00+01:00"),
                    None,
                    &["telegram"],
                    created,
                )
                .unwrap();
                tasks.push(task);
                Ok(())
            })
            .unwrap();
        let (ours, theirs) = tokio::io::duplex(1 << 16);
        let (read, write) = tokio::io::split(theirs);
        let (bridge, _events) = Bridge::connect(read, write);
        let shown = Arc::new(Mutex::new(Vec::new()));
        let record = shown.clone();
        let show = move |task: &DueTask| record.lock().unwrap().push(task.shown.clone());
        let now = DateTime::parse_from_rfc3339("2026-11-25T10:12:30+01:00")
            .unwrap()
            .with_timezone(&tz);
        let started = check(&schedule, "telegram", now, &bridge, &show);
        assert_eq!(started.len(), 1);
        assert_eq!(*shown.lock().unwrap(), ["⏰ Scheduled: Stretch."]);
        let (agent_read, _agent_write) = tokio::io::split(ours);
        let line = tokio::io::BufReader::new(agent_read)
            .lines()
            .next_line()
            .await
            .unwrap()
            .unwrap();
        assert!(
            line.contains("[Scheduled task") && line.contains("due 2026-11-25 10:12]\\nStretch."),
            "{line}"
        );
        assert!(schedule.read().unwrap().is_empty());
        assert_eq!(bridge.open_keys(), [task_key(&started[0].id)]);
    }
}
