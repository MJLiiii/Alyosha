//! The debug log every Alyosha program writes.
//!
//! One plain text file per program per day in
//! `$XDG_DATA_HOME/alyosha/logs/`. It records events, names, IDs, durations,
//! and errors, never the conversation. Each line is appended with one write
//! to a file opened with `O_APPEND`, so lines from several processes never
//! mix. Reference: `docs/reference/Logging.md`.

use std::{
    fmt::Write as _,
    fs::{self, DirBuilder, File, OpenOptions},
    io::{self, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, SystemTime},
};

use chrono::{DateTime, NaiveDate, Utc};
use chrono_tz::Tz;
use tracing::{Event, Subscriber};
use tracing_subscriber::{
    EnvFilter,
    fmt::{FmtContext, FormatEvent, FormatFields, MakeWriter, format::Writer},
    registry::LookupSpan,
};

/// The size at which a day's file stops growing.
pub const MAX_FILE_BYTES: u64 = 50 * 1024 * 1024;
/// The line written once when a file reaches its cap.
pub const LIMIT_LINE: &str = "Log limit reached; the rest of today is dropped";
/// Files older than this are deleted when a program starts.
pub const RETENTION: Duration = Duration::from_secs(14 * 24 * 60 * 60);

/// Where the current time comes from.
pub type Now = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;

/// Opens this program's log and installs it as the global `tracing`
/// subscriber. The level comes from `ALYOSHA_LOG`, `info` by default.
/// Returns the path of today's file, for startup warnings to name.
pub fn init(program: &str, timezone: Tz, paths: &crate::Paths) -> io::Result<PathBuf> {
    let dir = paths.data_dir.join("logs");
    let log = DailyLog::open(&dir, program, timezone, Arc::new(Utc::now))?;
    let path = log.current_path();
    let filter = EnvFilter::try_from_env("ALYOSHA_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .event_format(LineFormat {
            timezone,
            now: log.now.clone(),
        })
        .with_writer(log)
        .finish();
    tracing::subscriber::set_global_default(subscriber).map_err(io::Error::other)?;
    Ok(path)
}

/// A program's daily log files.
#[derive(Clone)]
pub struct DailyLog {
    dir: PathBuf,
    program: String,
    timezone: Tz,
    now: Now,
    current: Arc<Mutex<Option<Day>>>,
}

struct Day {
    date: NaiveDate,
    file: File,
    capped: bool,
}

impl DailyLog {
    /// Creates the folder with mode `0700` and deletes files older than 14
    /// days.
    pub fn open(dir: &Path, program: &str, timezone: Tz, now: Now) -> io::Result<Self> {
        DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        remove_old_files(dir, SystemTime::now())?;
        Ok(Self {
            dir: dir.to_owned(),
            program: program.to_owned(),
            timezone,
            now,
            current: Arc::new(Mutex::new(None)),
        })
    }

    /// Today's file.
    pub fn current_path(&self) -> PathBuf {
        self.path_for(self.today())
    }

    fn today(&self) -> NaiveDate {
        (self.now)().with_timezone(&self.timezone).date_naive()
    }

    fn path_for(&self, date: NaiveDate) -> PathBuf {
        self.dir.join(format!("{}.{date}.log", self.program))
    }

    /// Appends one whole line with a single write.
    pub fn write_line(&self, line: &[u8]) -> io::Result<()> {
        let today = self.today();
        let mut current = self.current.lock().unwrap_or_else(PoisonError::into_inner);
        if current.as_ref().is_none_or(|day| day.date != today) {
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .mode(0o600)
                .open(self.path_for(today))?;
            *current = Some(Day {
                date: today,
                file,
                capped: false,
            });
        }
        let day = current.as_mut().expect("opened above");
        if day.capped {
            return Ok(());
        }
        // Other processes may share the file, so read its real size.
        let size = day.file.metadata()?.len();
        if size + line.len() as u64 > MAX_FILE_BYTES {
            day.capped = true;
            return day.file.write_all(format!("{LIMIT_LINE}\n").as_bytes());
        }
        day.file.write_all(line)
    }
}

/// Deletes log files last changed more than 14 days ago.
fn remove_old_files(dir: &Path, now: SystemTime) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let is_log = entry.file_name().to_string_lossy().ends_with(".log");
        let modified = entry.metadata()?.modified()?;
        if is_log && now.duration_since(modified).unwrap_or_default() > RETENTION {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

/// Collects one event and writes it on drop, so each line is one write.
pub struct LineWriter {
    log: DailyLog,
    buffer: Vec<u8>,
}

impl Write for LineWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.buffer.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for LineWriter {
    fn drop(&mut self) {
        if !self.buffer.is_empty() {
            // A logging failure must never stop the program.
            let _ = self.log.write_line(&self.buffer);
        }
    }
}

impl<'a> MakeWriter<'a> for DailyLog {
    type Writer = LineWriter;

    fn make_writer(&'a self) -> LineWriter {
        LineWriter {
            log: self.clone(),
            buffer: Vec::new(),
        }
    }
}

/// `2026-11-25T10:02:03.123+01:00 INFO 4242 session=s1: message key=value`
struct LineFormat {
    timezone: Tz,
    now: Now,
}

impl<S, N> FormatEvent<S, N> for LineFormat
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> std::fmt::Result {
        let time = (self.now)().with_timezone(&self.timezone);
        let mut prefix = format!(
            "{} {} {}",
            time.format("%Y-%m-%dT%H:%M:%S%.3f%:z"),
            event.metadata().level(),
            std::process::id()
        );
        if let Some(scope) = ctx.event_scope() {
            for span in scope.from_root() {
                let extensions = span.extensions();
                if let Some(fields) =
                    extensions.get::<tracing_subscriber::fmt::FormattedFields<N>>()
                    && !fields.is_empty()
                {
                    write!(prefix, " {}{{{fields}}}", span.name())?;
                }
            }
        }
        write!(writer, "{prefix}: ")?;
        ctx.field_format().format_fields(writer.by_ref(), event)?;
        writeln!(writer)
    }
}

#[cfg(test)]
mod tests {
    use std::{os::unix::fs::MetadataExt, thread};

    use chrono::TimeZone;

    use super::*;

    fn at(hour: u32) -> Now {
        let time = Utc.with_ymd_and_hms(2026, 11, 25, hour, 0, 0).unwrap();
        Arc::new(move || time)
    }

    #[test]
    fn folder_and_file_are_private_and_named_by_program_and_local_date() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("logs");
        // 23:30 UTC on the 25th is already the 26th in Amsterdam.
        let late = Utc.with_ymd_and_hms(2026, 11, 25, 23, 30, 0).unwrap();
        let log = DailyLog::open(
            &dir,
            "alyosha-acp",
            chrono_tz::Europe::Amsterdam,
            Arc::new(move || late),
        )
        .unwrap();
        log.write_line(b"hello\n").unwrap();
        assert_eq!(fs::metadata(&dir).unwrap().mode() & 0o777, 0o700);
        let file = dir.join("alyosha-acp.2026-11-26.log");
        assert_eq!(log.current_path(), file);
        assert_eq!(fs::metadata(&file).unwrap().mode() & 0o777, 0o600);
        assert_eq!(fs::read_to_string(&file).unwrap(), "hello\n");
    }

    #[test]
    fn two_writers_never_mix_lines() {
        let root = tempfile::tempdir().unwrap();
        let threads: Vec<_> = (0..2)
            .map(|writer| {
                // Each writer opens the file on its own, like a separate process.
                let log =
                    DailyLog::open(root.path(), "alyosha-acp", chrono_tz::UTC, at(10)).unwrap();
                thread::spawn(move || {
                    let line = format!("{}\n", writer.to_string().repeat(3000));
                    for _ in 0..200 {
                        log.write_line(line.as_bytes()).unwrap();
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        let text = fs::read_to_string(root.path().join("alyosha-acp.2026-11-25.log")).unwrap();
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), 400);
        for line in lines {
            assert!(line == "0".repeat(3000) || line == "1".repeat(3000));
        }
    }

    #[test]
    fn the_cap_writes_one_limit_line_until_the_date_changes() {
        let root = tempfile::tempdir().unwrap();
        let hour = Arc::new(Mutex::new(
            Utc.with_ymd_and_hms(2026, 11, 25, 10, 0, 0).unwrap(),
        ));
        let clock = hour.clone();
        let log = DailyLog::open(
            root.path(),
            "alyosha-qq",
            chrono_tz::UTC,
            Arc::new(move || *clock.lock().unwrap()),
        )
        .unwrap();
        let mut big = vec![b'x'; 1024 * 1024 - 1];
        big.push(b'\n');
        for _ in 0..60 {
            log.write_line(&big).unwrap();
        }
        log.write_line(b"after the cap\n").unwrap();
        let text = fs::read_to_string(root.path().join("alyosha-qq.2026-11-25.log")).unwrap();
        assert!(text.len() as u64 <= MAX_FILE_BYTES + LIMIT_LINE.len() as u64 + 1);
        assert!(text.ends_with(&format!("{LIMIT_LINE}\n")));
        assert_eq!(text.matches(LIMIT_LINE).count(), 1);

        *hour.lock().unwrap() = Utc.with_ymd_and_hms(2026, 11, 26, 0, 30, 0).unwrap();
        log.write_line(b"a new day\n").unwrap();
        let next = fs::read_to_string(root.path().join("alyosha-qq.2026-11-26.log")).unwrap();
        assert_eq!(next, "a new day\n");
    }

    #[test]
    fn files_older_than_14_days_are_deleted_at_start() {
        let root = tempfile::tempdir().unwrap();
        let now = SystemTime::now();
        for (name, days) in [("old.2026-11-10.log", 15), ("recent.2026-11-12.log", 13)] {
            let path = root.path().join(name);
            let file = File::create(&path).unwrap();
            file.set_modified(now - Duration::from_secs(days * 24 * 60 * 60))
                .unwrap();
        }
        DailyLog::open(root.path(), "alyosha-acp", chrono_tz::UTC, at(10)).unwrap();
        assert!(!root.path().join("old.2026-11-10.log").exists());
        assert!(root.path().join("recent.2026-11-12.log").exists());
    }

    #[test]
    fn lines_carry_time_level_pid_and_session() {
        let root = tempfile::tempdir().unwrap();
        let log = DailyLog::open(
            root.path(),
            "alyosha-acp",
            chrono_tz::Europe::Amsterdam,
            at(9),
        )
        .unwrap();
        let subscriber = tracing_subscriber::fmt()
            .event_format(LineFormat {
                timezone: chrono_tz::Europe::Amsterdam,
                now: at(9),
            })
            .with_writer(log.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("session", id = "s1");
            let _entered = span.enter();
            tracing::info!(tool = "read_file", call_id = "call_a", "tool call finished");
        });
        let text = fs::read_to_string(log.current_path()).unwrap();
        let expected = format!(
            "2026-11-25T10:00:00.000+01:00 INFO {} session{{id=\"s1\"}}: tool call finished tool=\"read_file\" call_id=\"call_a\"\n",
            std::process::id()
        );
        assert_eq!(text, expected);
    }
}
