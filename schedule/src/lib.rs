//! Scheduled tasks: the task file shared by the schedule MCP server and the
//! bridges' runners, and the rules for when a task is due.
//!
//! Every reader and writer takes an exclusive `flock` on `schedule.lock`.
//! Writers replace the file atomically and refuse to touch a file that does
//! not parse, so a hand edit with a mistake loses no tasks. See
//! `docs/develop/Schedule.md`.

use std::{
    collections::HashSet,
    fs::{self, DirBuilder, File, OpenOptions},
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    str::FromStr,
};

use chrono::{DateTime, Duration, FixedOffset, TimeZone, Timelike};
use chrono_tz::Tz;
use croner::Cron;
use serde::{Deserialize, Serialize};

/// A task that is this late when it runs is marked late.
pub const LATE_AFTER: Duration = Duration::minutes(2);
pub const PROMPT_LIMIT: usize = 2000;

/// When a task runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum When {
    /// Once, at this time.
    At(DateTime<FixedOffset>),
    /// Repeating, by a five-field cron expression in the configured timezone.
    Cron(String),
}

/// One scheduled task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    pub id: String,
    pub channel: String,
    pub prompt: String,
    pub when: When,
    pub created: DateTime<FixedOffset>,
    pub last_run: Option<DateTime<FixedOffset>>,
}

#[derive(Debug, thiserror::Error)]
pub enum ScheduleError {
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("{path} does not parse, so no task can change until it is fixed: {message}")]
    Unreadable { path: PathBuf, message: String },
    #[error("{0}")]
    Invalid(String),
}

#[derive(Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct FileRecord {
    #[serde(default)]
    task: Vec<TaskRecord>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskRecord {
    id: String,
    channel: String,
    prompt: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    cron: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    at: Option<toml::value::Datetime>,
    created: toml::value::Datetime,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_run: Option<toml::value::Datetime>,
}

fn from_toml(value: &toml::value::Datetime, field: &str) -> Result<DateTime<FixedOffset>, String> {
    DateTime::parse_from_rfc3339(&value.to_string())
        .map_err(|_| format!("{field} = {value} needs a date, a time, and an offset"))
}

fn to_toml(value: &DateTime<FixedOffset>) -> toml::value::Datetime {
    toml::value::Datetime::from_str(&value.to_rfc3339()).expect("RFC 3339 is valid TOML")
}

impl TryFrom<TaskRecord> for Task {
    type Error = String;

    fn try_from(record: TaskRecord) -> Result<Self, String> {
        let when = match (record.at, record.cron) {
            (Some(at), None) => When::At(from_toml(&at, "at")?),
            (None, Some(cron)) => When::Cron(cron),
            _ => {
                return Err(format!(
                    "task {} needs exactly one of at and cron",
                    record.id
                ));
            }
        };
        Ok(Task {
            id: record.id,
            channel: record.channel,
            prompt: record.prompt,
            when,
            created: from_toml(&record.created, "created")?,
            last_run: record
                .last_run
                .as_ref()
                .map(|value| from_toml(value, "last_run"))
                .transpose()?,
        })
    }
}

impl From<&Task> for TaskRecord {
    fn from(task: &Task) -> Self {
        let (at, cron) = match &task.when {
            When::At(at) => (Some(to_toml(at)), None),
            When::Cron(cron) => (None, Some(cron.clone())),
        };
        TaskRecord {
            id: task.id.clone(),
            channel: task.channel.clone(),
            prompt: task.prompt.clone(),
            cron,
            at,
            created: to_toml(&task.created),
            last_run: task.last_run.as_ref().map(to_toml),
        }
    }
}

/// The task file in Alyosha's data folder.
#[derive(Debug, Clone)]
pub struct Schedule {
    dir: PathBuf,
}

/// Held while the file is read or changed.
pub struct Lock(#[allow(dead_code)] File);

impl Schedule {
    /// The file `schedule.toml` in `data_dir`, normally
    /// `~/.local/share/alyosha`.
    pub fn new(data_dir: &Path) -> Self {
        Self {
            dir: data_dir.to_owned(),
        }
    }

    pub fn path(&self) -> PathBuf {
        self.dir.join("schedule.toml")
    }

    fn lock(&self) -> Result<Lock, ScheduleError> {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.dir)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(self.dir.join("schedule.lock"))?;
        rustix::fs::flock(&file, rustix::fs::FlockOperation::LockExclusive)
            .map_err(std::io::Error::from)?;
        Ok(Lock(file))
    }

    fn read_unlocked(&self) -> Result<Vec<Task>, ScheduleError> {
        let text = match fs::read_to_string(self.path()) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let unreadable = |message: String| ScheduleError::Unreadable {
            path: self.path(),
            message,
        };
        let file: FileRecord =
            toml::from_str(&text).map_err(|error| unreadable(error.to_string()))?;
        file.task
            .into_iter()
            .map(|record| Task::try_from(record).map_err(unreadable))
            .collect()
    }

    /// Every task, read under the lock.
    pub fn read(&self) -> Result<Vec<Task>, ScheduleError> {
        let _lock = self.lock()?;
        self.read_unlocked()
    }

    /// Reads, changes, and atomically rewrites the file under the lock. The
    /// file is written only if `change` succeeds.
    pub fn update<R>(
        &self,
        change: impl FnOnce(&mut Vec<Task>) -> Result<R, ScheduleError>,
    ) -> Result<R, ScheduleError> {
        let _lock = self.lock()?;
        let mut tasks = self.read_unlocked()?;
        let before = tasks.clone();
        let result = change(&mut tasks)?;
        if tasks != before {
            let file = FileRecord {
                task: tasks.iter().map(TaskRecord::from).collect(),
            };
            let text = toml::to_string(&file)
                .map_err(|error| ScheduleError::Invalid(error.to_string()))?;
            let staging = self.dir.join(".schedule.toml.tmp");
            let mut out = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&staging)?;
            out.write_all(text.as_bytes())?;
            out.sync_all()?;
            fs::rename(&staging, self.path())?;
        }
        Ok(result)
    }
}

/// Parses a five-field cron expression.
pub fn parse_cron(expression: &str) -> Result<Cron, String> {
    if expression.split_whitespace().count() != 5 {
        return Err(format!(
            "{expression:?} is not a five-field cron expression, such as \"0 8 * * *\""
        ));
    }
    Cron::from_str(expression)
        .map_err(|error| format!("{expression:?} is not a valid cron expression: {error}"))
}

/// The task's next run after `now`, or its due time if it is overdue.
pub fn next_run(task: &Task, now: DateTime<Tz>) -> Option<DateTime<Tz>> {
    match &task.when {
        When::At(at) => Some(at.with_timezone(&now.timezone())),
        When::Cron(expression) => parse_cron(expression)
            .ok()?
            .find_next_occurrence(&now, false)
            .ok(),
    }
}

/// A new task ID: six random lowercase letters and digits.
pub fn new_id(taken: &[Task]) -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    loop {
        let mut bytes = [0u8; 6];
        let _ = getrandom::fill(&mut bytes);
        let id: String = bytes
            .iter()
            .map(|byte| ALPHABET[*byte as usize % ALPHABET.len()] as char)
            .collect();
        if !taken.iter().any(|task| task.id == id) {
            return id;
        }
    }
}

/// Checks a new task's fields and builds it.
pub fn new_task(
    tasks: &[Task],
    channel: &str,
    prompt: &str,
    at: Option<&str>,
    cron: Option<&str>,
    configured: &[&str],
    now: DateTime<Tz>,
) -> Result<Task, String> {
    if !configured.contains(&channel) {
        return Err(if configured.is_empty() {
            format!("The {channel} channel is not configured, and no chat channel is.")
        } else {
            format!(
                "The {channel} channel is not configured. Use one of: {}.",
                configured.join(", ")
            )
        });
    }
    let length = prompt.chars().count();
    if length == 0 || length > PROMPT_LIMIT {
        return Err(format!("prompt must be 1 to {PROMPT_LIMIT} characters"));
    }
    let when = match (at, cron) {
        (Some(_), Some(_)) => return Err("Give either at or cron, not both.".into()),
        (None, None) => {
            return Err("Give at for a one-time task or cron for a repeating one.".into());
        }
        (Some(at), None) => {
            let at = DateTime::parse_from_rfc3339(at)
                .map_err(|_| format!("at = {at:?} is not an RFC 3339 time with an offset, such as 2026-11-28T17:00:00+01:00"))?;
            if at < now {
                return Err(format!("at = {at} is in the past"));
            }
            When::At(at)
        }
        (None, Some(cron)) => {
            let parsed = parse_cron(cron)?;
            if parsed.find_next_occurrence(&now, false).is_err() {
                return Err(format!("{cron:?} never fires"));
            }
            When::Cron(cron.to_owned())
        }
    };
    Ok(Task {
        id: new_id(tasks),
        channel: channel.to_owned(),
        prompt: prompt.to_owned(),
        when,
        created: now
            .fixed_offset()
            .with_nanosecond(0)
            .unwrap_or(now.fixed_offset()),
        last_run: None,
    })
}

/// A task the runner should start now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DueTask {
    pub id: String,
    /// The prompt text sent to the session.
    pub prompt: String,
    /// What the chat shows first: **⏰ Scheduled:**, the prompt, and
    /// **(late)** when late.
    pub shown: String,
    pub late: bool,
}

/// Picks this channel's due tasks and updates the list: one-time tasks that
/// run leave it, repeating tasks record their latest due time. `open` holds
/// the IDs of tasks whose previous run is still open.
pub fn take_due(
    tasks: &mut Vec<Task>,
    channel: &str,
    now: DateTime<Tz>,
    open: &HashSet<String>,
) -> Vec<DueTask> {
    let mut due = Vec::new();
    let timezone = now.timezone();
    tasks.retain_mut(|task| {
        if task.channel != channel {
            return true;
        }
        match &task.when {
            When::At(at) => {
                if *at > now.fixed_offset() {
                    return true;
                }
                let late = now.fixed_offset() - *at > LATE_AFTER;
                due.push(due_task(task, at.with_timezone(&timezone), late));
                false
            }
            When::Cron(expression) => {
                let Ok(cron) = parse_cron(expression) else {
                    return true;
                };
                let Ok(latest) = cron.find_previous_occurrence(&now, true) else {
                    return true;
                };
                let base = task.last_run.unwrap_or(task.created);
                if latest.fixed_offset() <= base {
                    return true;
                }
                let recent = now.fixed_offset() - latest.fixed_offset() <= LATE_AFTER;
                if recent && !open.contains(&task.id) {
                    due.push(due_task(task, latest, false));
                }
                task.last_run = Some(latest.fixed_offset());
                true
            }
        }
    });
    due
}

fn due_task(task: &Task, due: DateTime<Tz>, late: bool) -> DueTask {
    DueTask {
        id: task.id.clone(),
        prompt: format!(
            "[Scheduled task {}, due {}]\n{}",
            task.id,
            due.format("%Y-%m-%d %H:%M"),
            task.prompt
        ),
        shown: format!(
            "⏰ Scheduled: {}{}",
            task.prompt,
            if late { " (late)" } else { "" }
        ),
        late,
    }
}

fn short(text: &str, limit: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= limit {
        flat
    } else {
        flat.chars().take(limit - 1).collect::<String>() + "…"
    }
}

/// The `/tasks` reply: one line per task, sorted by next run.
pub fn tasks_text(tasks: &[Task], now: DateTime<Tz>) -> String {
    if tasks.is_empty() {
        return "No scheduled tasks.".to_owned();
    }
    let mut lines: Vec<(Option<DateTime<Tz>>, String)> = tasks
        .iter()
        .map(|task| {
            let next = next_run(task, now);
            let shown = next.map_or_else(
                || "never".to_owned(),
                |next| next.format("%a %Y-%m-%d %H:%M").to_string(),
            );
            let repeats = if matches!(task.when, When::Cron(_)) {
                " repeats"
            } else {
                ""
            };
            (
                next,
                format!(
                    "{} {} {shown}{repeats} {}",
                    task.id,
                    task.channel,
                    short(&task.prompt, 80)
                ),
            )
        })
        .collect();
    lines.sort_by_key(|(next, _)| *next);
    lines
        .into_iter()
        .map(|(_, line)| line)
        .collect::<Vec<_>>()
        .join("\n")
}

/// A local time in the configured timezone.
pub fn local(timezone: Tz, time: DateTime<FixedOffset>) -> DateTime<Tz> {
    timezone.from_utc_datetime(&time.naive_utc())
}

#[cfg(test)]
mod tests {
    use super::*;

    const AMS: Tz = chrono_tz::Europe::Amsterdam;

    fn at(text: &str) -> DateTime<Tz> {
        DateTime::parse_from_rfc3339(text)
            .unwrap()
            .with_timezone(&AMS)
    }

    fn sample(now: DateTime<Tz>) -> Vec<Task> {
        vec![
            new_task(
                &[],
                "telegram",
                "Summarize my inbox.",
                None,
                Some("0 8 * * *"),
                &["telegram", "qq"],
                now,
            )
            .unwrap(),
            new_task(
                &[],
                "qq",
                "Call the bank.",
                Some("2026-11-28T17:00:00+01:00"),
                None,
                &["telegram", "qq"],
                now,
            )
            .unwrap(),
        ]
    }

    #[test]
    fn invalid_tasks_are_refused_with_the_reason() {
        let now = at("2026-11-25T10:02:00+01:00");
        let channels = ["telegram"];
        let error = |at: Option<&str>, cron: Option<&str>, channel: &str| {
            new_task(&[], channel, "x", at, cron, &channels, now).unwrap_err()
        };
        assert!(error(None, Some("0 8 * * *"), "qq").contains("qq channel is not configured"));
        assert!(error(Some("2026-11-24T10:00:00+01:00"), None, "telegram").contains("in the past"));
        assert!(error(None, Some("61 * * * *"), "telegram").contains("not a valid cron"));
        assert!(error(None, Some("0 8 * *"), "telegram").contains("five-field"));
        assert!(
            error(
                Some("2026-11-28T17:00:00+01:00"),
                Some("0 8 * * *"),
                "telegram"
            )
            .contains("not both")
        );
        assert!(error(None, None, "telegram").contains("Give at"));
        assert!(error(None, Some("0 0 30 2 *"), "telegram").contains("never fires"));
    }

    #[test]
    fn the_file_round_trips_with_mode_600() {
        let root = tempfile::tempdir().unwrap();
        let schedule = Schedule::new(root.path());
        let now = at("2026-11-25T10:02:00+01:00");
        let tasks = sample(now);
        schedule
            .update(|file| {
                file.extend(tasks.clone());
                Ok(())
            })
            .unwrap();
        assert_eq!(schedule.read().unwrap(), tasks);
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(schedule.path()).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let text = fs::read_to_string(schedule.path()).unwrap();
        assert!(
            text.contains("cron = \"0 8 * * *\"")
                && text.contains("at = 2026-11-28T17:00:00+01:00"),
            "{text}"
        );
    }

    #[test]
    fn a_broken_file_refuses_changes_until_fixed() {
        let root = tempfile::tempdir().unwrap();
        let schedule = Schedule::new(root.path());
        fs::write(schedule.path(), "[[task]\nbroken").unwrap();
        assert!(matches!(
            schedule.read(),
            Err(ScheduleError::Unreadable { .. })
        ));
        let result = schedule.update(|tasks| {
            tasks.clear();
            Ok(())
        });
        assert!(result.is_err());
        assert_eq!(
            fs::read_to_string(schedule.path()).unwrap(),
            "[[task]\nbroken"
        );
        fs::write(schedule.path(), "").unwrap();
        assert!(schedule.read().unwrap().is_empty());
    }

    #[test]
    fn two_writers_keep_both_changes() {
        let root = tempfile::tempdir().unwrap();
        let schedule = Schedule::new(root.path());
        let now = at("2026-11-25T10:02:00+01:00");
        let threads: Vec<_> = (0..2)
            .map(|n| {
                let schedule = schedule.clone();
                std::thread::spawn(move || {
                    for i in 0..20 {
                        schedule
                            .update(|tasks| {
                                let task = new_task(
                                    tasks,
                                    "telegram",
                                    &format!("w{n} {i}"),
                                    None,
                                    Some("0 8 * * *"),
                                    &["telegram"],
                                    now,
                                )
                                .unwrap();
                                tasks.push(task);
                                Ok(())
                            })
                            .unwrap();
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(schedule.read().unwrap().len(), 40);
    }

    #[test]
    fn a_one_time_task_runs_once_and_late_when_missed() {
        let created = at("2026-11-25T10:02:00+01:00");
        let mut tasks = sample(created);
        let open = HashSet::new();
        assert!(take_due(&mut tasks, "qq", at("2026-11-28T16:59:00+01:00"), &open).is_empty());
        let due = take_due(&mut tasks, "qq", at("2026-11-28T17:00:30+01:00"), &open);
        assert_eq!(due.len(), 1);
        assert!(!due[0].late);
        assert_eq!(
            due[0].prompt,
            format!(
                "[Scheduled task {}, due 2026-11-28 17:00]\nCall the bank.",
                due[0].id
            )
        );
        assert_eq!(due[0].shown, "⏰ Scheduled: Call the bank.");
        assert_eq!(tasks.len(), 1, "it left the file");

        let mut tasks = sample(created);
        let due = take_due(&mut tasks, "qq", at("2026-11-28T20:00:00+01:00"), &open);
        assert!(due[0].late && due[0].shown.ends_with("(late)"));
    }

    #[test]
    fn a_missed_repeat_is_skipped_and_runs_the_next_day() {
        let created = at("2026-11-25T10:02:00+01:00");
        let mut tasks = sample(created);
        let open = HashSet::new();
        // Three hours after 08:00 on the 26th: skipped, but recorded.
        assert!(
            take_due(
                &mut tasks,
                "telegram",
                at("2026-11-26T11:00:00+01:00"),
                &open
            )
            .is_empty()
        );
        assert_eq!(
            tasks[0].last_run.unwrap().to_rfc3339(),
            "2026-11-26T08:00:00+01:00"
        );
        let due = take_due(
            &mut tasks,
            "telegram",
            at("2026-11-27T08:00:40+01:00"),
            &open,
        );
        assert_eq!(due.len(), 1);
        assert!(
            take_due(
                &mut tasks,
                "telegram",
                at("2026-11-27T08:01:40+01:00"),
                &open
            )
            .is_empty(),
            "once per time"
        );
    }

    #[test]
    fn a_repeat_still_open_is_skipped_and_channels_stay_apart() {
        let created = at("2026-11-25T10:02:00+01:00");
        let mut tasks = sample(created);
        let id = tasks[0].id.clone();
        let open: HashSet<String> = [id].into();
        assert!(
            take_due(
                &mut tasks,
                "telegram",
                at("2026-11-26T08:00:10+01:00"),
                &open
            )
            .is_empty()
        );
        assert!(tasks[0].last_run.is_some());
        // The QQ runner never takes the Telegram task.
        let mut tasks = sample(created);
        assert!(
            take_due(
                &mut tasks,
                "qq",
                at("2026-11-26T08:00:10+01:00"),
                &HashSet::new()
            )
            .is_empty()
        );
    }

    #[test]
    fn the_tasks_reply_lists_by_next_run() {
        let now = at("2026-11-25T10:02:00+01:00");
        assert_eq!(tasks_text(&[], now), "No scheduled tasks.");
        let tasks = sample(now);
        let text = tasks_text(&tasks, now);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].contains("telegram Thu 2026-11-26 08:00 repeats Summarize my inbox."));
        assert!(lines[1].contains("qq Sat 2026-11-28 17:00 Call the bank."));
    }
}
