//! Background jobs: long shell commands that run outside a turn and start a
//! turn when they end.
//!
//! A job uses the normal sandbox launch under its own Stop signal, which lives
//! for the session, so Stop on a turn never reaches it. Its output streams to
//! a session log from the start. See `docs/Jobs.md#implementation`.

use std::{
    collections::VecDeque,
    fs::File,
    io::Write,
    path::PathBuf,
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, Instant},
};

use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::{
    environment::{
        SessionLogs,
        shell::{OutputSink, SandboxConfig, ShellLaunch, ShellStatus, run_shell_into},
    },
    tools::{RegisteredTool, Scope, ToolContext, ToolDefinition, ToolError, ToolOutput, handler},
};

/// At most this many jobs run at once in a session.
pub const MAX_RUNNING: usize = 2;
/// A job's deadline unless it sets its own.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(4 * 60 * 60);
pub const MAX_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);
const TAIL_BYTES: usize = 2 * 1024;
const KEPT_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    Running,
    Finished(ShellStatus),
}

/// Who started a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobOrigin {
    Model,
    /// The owner, with `/job`.
    Owner,
}

/// What the session knows about one job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobInfo {
    pub id: String,
    pub command: String,
    pub started: DateTime<Utc>,
    pub state: JobState,
    pub runtime: Duration,
    /// The last 2 KiB of output, on a character boundary.
    pub tail: String,
    pub log_path: String,
    /// Earlier output was dropped when the log budget ran out.
    pub output_dropped: bool,
}

/// The message a job that ended on its own sends to the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobFinished {
    pub id: String,
    pub text: String,
}

/// Where finished-job messages go: the session driver.
pub type OnFinished = Arc<dyn Fn(JobFinished) + Send + Sync>;

/// What every job's sandbox needs.
#[derive(Debug, Clone)]
pub struct JobEnvironment {
    pub sandbox: SandboxConfig,
    pub workspace: PathBuf,
    pub cwd: PathBuf,
    pub skills_root: Option<PathBuf>,
    pub logs: Arc<SessionLogs>,
}

struct Job {
    info: JobInfo,
    origin: JobOrigin,
    output: Arc<Mutex<JobOutput>>,
    stop: CancellationToken,
    started_at: Instant,
    task: Option<tokio::task::JoinHandle<()>>,
}

/// The session's jobs.
pub struct JobManager {
    environment: Arc<JobEnvironment>,
    jobs: Arc<Mutex<Vec<Job>>>,
    on_finished: OnFinished,
    next: Mutex<u64>,
    on_change: Option<Arc<dyn Fn(usize) + Send + Sync>>,
}

impl std::fmt::Debug for JobManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobManager").finish_non_exhaustive()
    }
}

/// Streams output to the job's log while the budget allows, and keeps the
/// latest 64 KiB in memory.
struct JobOutput {
    file: Option<File>,
    logs: Arc<SessionLogs>,
    recent: VecDeque<u8>,
    dropped: bool,
}

impl OutputSink for JobOutput {
    fn write(&mut self, bytes: &[u8]) {
        if self.file.is_some() {
            let saved = self.logs.reserve(bytes.len() as u64)
                && self
                    .file
                    .as_mut()
                    .is_some_and(|file| file.write_all(bytes).is_ok());
            if !saved {
                self.file = None;
                self.dropped = true;
            }
        }
        self.recent.extend(bytes);
        let excess = self.recent.len().saturating_sub(KEPT_BYTES);
        self.recent.drain(..excess);
    }
}

impl JobOutput {
    fn tail(&self) -> String {
        let bytes: Vec<u8> = self.recent.iter().copied().collect();
        let start = bytes.len().saturating_sub(TAIL_BYTES);
        let text = String::from_utf8_lossy(&bytes[start..]).into_owned();
        // A cut in the middle of a character shows as a replacement mark.
        text.trim_start_matches('\u{FFFD}').to_owned()
    }
}

impl JobManager {
    pub fn new(environment: JobEnvironment, on_finished: OnFinished) -> Self {
        Self {
            environment: Arc::new(environment),
            jobs: Arc::new(Mutex::new(Vec::new())),
            on_finished,
            next: Mutex::new(1),
            on_change: None,
        }
    }

    /// Called with the number of running jobs whenever it changes.
    pub fn on_change(mut self, callback: Arc<dyn Fn(usize) + Send + Sync>) -> Self {
        self.on_change = Some(callback);
        self
    }

    fn running(jobs: &[Job]) -> usize {
        jobs.iter()
            .filter(|job| job.info.state == JobState::Running)
            .count()
    }

    fn changed(&self) {
        if let Some(callback) = &self.on_change {
            let count = Self::running(&self.jobs.lock().unwrap_or_else(PoisonError::into_inner));
            callback(count);
        }
    }

    /// Starts a job, or refuses when two are running.
    pub fn start(
        &self,
        command: &str,
        timeout: Duration,
        origin: JobOrigin,
    ) -> Result<JobInfo, ToolError> {
        let mut jobs = self.jobs.lock().unwrap_or_else(PoisonError::into_inner);
        if Self::running(&jobs) >= MAX_RUNNING {
            return Err(ToolError::failed(format!(
                "{MAX_RUNNING} jobs are already running. Wait for one to finish or stop one first."
            )));
        }
        let id = {
            let mut next = self.next.lock().unwrap_or_else(PoisonError::into_inner);
            let id = format!("j{next}");
            *next += 1;
            id
        };
        let (file, log_path) =
            self.environment
                .logs
                .create(&format!("job-{id}"))
                .map_err(|error| {
                    ToolError::failed(format!("Could not create the job's log: {error}"))
                })?;
        let output = Arc::new(Mutex::new(JobOutput {
            file: Some(file),
            logs: self.environment.logs.clone(),
            recent: VecDeque::new(),
            dropped: false,
        }));
        let stop = CancellationToken::new();
        let info = JobInfo {
            id: id.clone(),
            command: command.to_owned(),
            started: Utc::now(),
            state: JobState::Running,
            runtime: Duration::ZERO,
            tail: String::new(),
            log_path,
            output_dropped: false,
        };
        let task = {
            let environment = self.environment.clone();
            let command = command.to_owned();
            let stop = stop.clone();
            let output = output.clone();
            let jobs = self.jobs.clone();
            let on_finished = self.on_finished.clone();
            let on_change = self.on_change.clone();
            let id = id.clone();
            tokio::spawn(async move {
                let launch = ShellLaunch {
                    config: &environment.sandbox,
                    workspace: &environment.workspace,
                    cwd: &environment.cwd,
                    skills_root: environment.skills_root.as_deref(),
                };
                let status =
                    match run_shell_into(&launch, &command, timeout, &stop, output.clone()).await {
                        Ok(status) => status,
                        Err(failure) => {
                            output.lock().unwrap_or_else(PoisonError::into_inner).write(
                                format!("The sandbox could not start: {}\n", failure.0).as_bytes(),
                            );
                            ShellStatus::Exited(127)
                        }
                    };
                let finished = {
                    let mut jobs = jobs.lock().unwrap_or_else(PoisonError::into_inner);
                    let Some(job) = jobs.iter_mut().find(|job| job.info.id == id) else {
                        return;
                    };
                    job.info.state = JobState::Finished(status);
                    job.info.runtime = job.started_at.elapsed();
                    {
                        let output = job.output.lock().unwrap_or_else(PoisonError::into_inner);
                        job.info.tail = output.tail();
                        job.info.output_dropped = output.dropped;
                    }
                    // A job ended by stop or shutdown sends nothing.
                    let message = (status != ShellStatus::Stopped)
                        .then(|| finished_message(&job.info, job.origin));
                    if let Some(callback) = &on_change {
                        callback(Self::running(&jobs));
                    }
                    message
                };
                if let Some(message) = finished {
                    on_finished(message);
                }
            })
        };
        jobs.push(Job {
            info: info.clone(),
            origin,
            output,
            stop,
            started_at: Instant::now(),
            task: Some(task),
        });
        drop(jobs);
        self.changed();
        Ok(info)
    }

    fn snapshot(job: &Job) -> JobInfo {
        let mut info = job.info.clone();
        if info.state == JobState::Running {
            info.runtime = job.started_at.elapsed();
            let output = job.output.lock().unwrap_or_else(PoisonError::into_inner);
            info.tail = output.tail();
            info.output_dropped = output.dropped;
        }
        info
    }

    pub fn list(&self) -> Vec<JobInfo> {
        let jobs = self.jobs.lock().unwrap_or_else(PoisonError::into_inner);
        jobs.iter().map(Self::snapshot).collect()
    }

    pub fn running_count(&self) -> usize {
        Self::running(&self.jobs.lock().unwrap_or_else(PoisonError::into_inner))
    }

    pub fn status(&self, id: &str) -> Result<JobInfo, ToolError> {
        let jobs = self.jobs.lock().unwrap_or_else(PoisonError::into_inner);
        jobs.iter()
            .find(|job| job.info.id == id)
            .map(Self::snapshot)
            .ok_or_else(|| ToolError::failed(format!("There is no job {id} in this session.")))
    }

    /// Stops a running job and returns its final state. A finished job is
    /// returned unchanged.
    pub async fn stop(&self, id: &str) -> Result<JobInfo, ToolError> {
        let task = {
            let mut jobs = self.jobs.lock().unwrap_or_else(PoisonError::into_inner);
            let job = jobs
                .iter_mut()
                .find(|job| job.info.id == id)
                .ok_or_else(|| {
                    ToolError::failed(format!("There is no job {id} in this session."))
                })?;
            job.stop.cancel();
            job.task.take()
        };
        if let Some(task) = task {
            let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
        }
        self.status(id)
    }

    /// Kills every running job and waits up to 5 seconds for each.
    pub async fn shutdown(&self) {
        let tasks: Vec<_> = {
            let mut jobs = self.jobs.lock().unwrap_or_else(PoisonError::into_inner);
            jobs.iter_mut()
                .filter_map(|job| {
                    job.stop.cancel();
                    job.task.take()
                })
                .collect()
        };
        for task in tasks {
            let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
        }
    }
}

/// `58m`, `3s`, `1h 2m`.
pub fn format_runtime(runtime: Duration) -> String {
    let seconds = runtime.as_secs();
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m", seconds / 60),
        _ => format!("{}h {}m", seconds / 3600, seconds % 3600 / 60),
    }
}

/// A command cut to 80 characters for one-line displays.
pub fn short_command(command: &str) -> String {
    let flat = command.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= 80 {
        flat
    } else {
        flat.chars().take(79).collect::<String>() + "…"
    }
}

fn finished_message(info: &JobInfo, origin: JobOrigin) -> JobFinished {
    let status = match info.state {
        JobState::Finished(ShellStatus::TimedOut) => "TimedOut".to_owned(),
        JobState::Finished(status) => format!("status {status}"),
        JobState::Running => "running".to_owned(),
    };
    let mut text = format!(
        "[Job {} finished: {status} after {} · {}",
        info.id,
        format_runtime(info.runtime),
        short_command(&info.command)
    );
    if origin == JobOrigin::Owner {
        text.push_str(" · started by the owner with /job");
    }
    text.push_str("]\nLast output:\n");
    text.push_str(&info.tail);
    if info.output_dropped {
        if !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str("Earlier output was dropped.");
    }
    JobFinished {
        id: info.id.clone(),
        text,
    }
}

fn state_text(state: JobState) -> String {
    match state {
        JobState::Running => "Running".into(),
        JobState::Finished(status) => format!("Finished({status})"),
    }
}

/// Every field, with the tail last.
pub fn describe(info: &JobInfo) -> String {
    format!(
        "id: {}\ncommand: {}\nstarted: {}\nstate: {}\nruntime: {}\nlog_path: {}\noutput_dropped: {}\ntail:\n{}",
        info.id,
        info.command,
        info.started.to_rfc3339(),
        state_text(info.state),
        format_runtime(info.runtime),
        info.log_path,
        info.output_dropped,
        info.tail
    )
}

/// The built-in `job` tool.
pub fn job_tool() -> RegisteredTool {
    RegisteredTool {
        definition: ToolDefinition {
            name: "job".into(),
            description: "List, check, or stop this session's background jobs, started with run_command and background: true. `status` returns a job's state and latest output; read its whole output with read_file on its log_path.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "enum": ["list", "status", "stop"]},
                    "id": {"type": "string"}
                },
                "required": ["command"],
                "additionalProperties": false
            }),
            requires_approval: false,
        },
        handler: handler(|arguments: Value, context: ToolContext| async move {
            if context.scope != Scope::Direct {
                return Err(ToolError::failed("job is for direct calls only."));
            }
            let jobs = context
                .session
                .jobs
                .as_ref()
                .ok_or_else(|| ToolError::failed("This session cannot run jobs."))?;
            let id = || {
                arguments["id"]
                    .as_str()
                    .ok_or_else(|| ToolError::failed("This command needs a job id."))
            };
            match arguments["command"].as_str().unwrap_or_default() {
                "list" => {
                    let lines: Vec<String> = jobs
                        .list()
                        .iter()
                        .map(|info| {
                            format!(
                                "{} {} {} {}",
                                info.id,
                                state_text(info.state),
                                format_runtime(info.runtime),
                                short_command(&info.command)
                            )
                        })
                        .collect();
                    Ok(ToolOutput::text(if lines.is_empty() { "No jobs in this session.".into() } else { lines.join("\n") }))
                }
                "status" => Ok(ToolOutput::text(describe(&jobs.status(id()?)?))),
                "stop" => Ok(ToolOutput::text(describe(&jobs.stop(id()?).await?))),
                other => Err(ToolError::failed(format!("Unknown job command {other:?}."))),
            }
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtimes_and_messages_read_naturally() {
        assert_eq!(format_runtime(Duration::from_secs(3)), "3s");
        assert_eq!(format_runtime(Duration::from_secs(58 * 60 + 5)), "58m");
        assert_eq!(format_runtime(Duration::from_secs(3720)), "1h 2m");
        let info = JobInfo {
            id: "j1".into(),
            command: "cargo test --workspace".into(),
            started: Utc::now(),
            state: JobState::Finished(ShellStatus::Exited(101)),
            runtime: Duration::from_secs(58 * 60),
            tail: "test result: FAILED\n".into(),
            log_path: "/session-logs/job-j1-1.log".into(),
            output_dropped: false,
        };
        assert_eq!(
            finished_message(&info, JobOrigin::Model).text,
            "[Job j1 finished: status 101 after 58m · cargo test --workspace]\nLast output:\ntest result: FAILED\n"
        );
        let timed_out = JobInfo {
            state: JobState::Finished(ShellStatus::TimedOut),
            output_dropped: true,
            ..info
        };
        assert_eq!(
            finished_message(&timed_out, JobOrigin::Owner).text,
            "[Job j1 finished: TimedOut after 58m · cargo test --workspace · started by the owner with /job]\nLast output:\ntest result: FAILED\nEarlier output was dropped."
        );
    }

    #[test]
    fn the_sink_drops_to_memory_when_the_budget_runs_out() {
        let root = tempfile::tempdir().unwrap();
        let logs = Arc::new(SessionLogs::with_budget(root.path().join("logs"), 10));
        let (file, _path) = logs.create("job").unwrap();
        let mut output = JobOutput {
            file: Some(file),
            logs,
            recent: VecDeque::new(),
            dropped: false,
        };
        output.write(b"12345678");
        assert!(!output.dropped);
        output.write(b"abcdef");
        assert!(output.dropped);
        assert_eq!(output.tail(), "12345678abcdef");
    }
}
