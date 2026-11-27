//! The debug log every Alyosha program writes.
//!
//! Each program logs to the systemd journal under an identifier of its own,
//! such as `alyosha-acp-guest`, with its level as the priority. Outside a
//! systemd service, as in tests and development, lines go to standard error
//! instead. The log records events, names, IDs, durations, and errors, never
//! the conversation. Reference: `docs/reference/Logging.md`.

use std::{fmt::Write as _, io, sync::Arc};

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use tracing::{Event, Subscriber};
use tracing_subscriber::{
    EnvFilter,
    fmt::{FmtContext, FormatEvent, FormatFields, format::Writer},
    layer::SubscriberExt,
    registry::LookupSpan,
};

/// Where the current time comes from.
pub type Now = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;

/// Where a program's log went, for startup warnings to name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogTarget {
    Journal,
    Stderr,
}

impl LogTarget {
    /// The words a warning uses to point at the details.
    pub fn hint(self) -> &'static str {
        match self {
            Self::Journal => "the log: alyosha logs",
            Self::Stderr => "standard error",
        }
    }
}

/// Installs this program's log as the global `tracing` subscriber. The level
/// comes from `ALYOSHA_LOG`, `info` by default. A program started by systemd
/// with its output in the journal, which systemd marks with
/// `JOURNAL_STREAM`, logs to the journal; any other logs to standard error.
pub fn init(program: &str, timezone: Tz) -> io::Result<LogTarget> {
    let filter =
        || EnvFilter::try_from_env("ALYOSHA_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    if std::env::var_os("JOURNAL_STREAM").is_some()
        && let Ok(journal) = tracing_journald::layer()
    {
        let subscriber = tracing_subscriber::registry()
            .with(filter())
            .with(journal.with_syslog_identifier(program.to_owned()));
        tracing::subscriber::set_global_default(subscriber).map_err(io::Error::other)?;
        return Ok(LogTarget::Journal);
    }
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(filter())
        .event_format(LineFormat {
            program: program.to_owned(),
            timezone,
            now: Arc::new(Utc::now),
        })
        .with_writer(io::stderr)
        .finish();
    tracing::subscriber::set_global_default(subscriber).map_err(io::Error::other)?;
    Ok(LogTarget::Stderr)
}

/// `2026-11-25T10:02:03.123+01:00 INFO alyosha-acp 4242 session{id="s1"}: message key=value`
struct LineFormat {
    program: String,
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
            "{} {} {} {}",
            time.format("%Y-%m-%dT%H:%M:%S%.3f%:z"),
            event.metadata().level(),
            self.program,
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
    use std::sync::Mutex;

    use chrono::TimeZone;
    use tracing_subscriber::fmt::MakeWriter;

    use super::*;

    /// Collects what the subscriber writes.
    #[derive(Clone, Default)]
    struct Collect(Arc<Mutex<Vec<u8>>>);

    impl io::Write for Collect {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for Collect {
        type Writer = Collect;
        fn make_writer(&'a self) -> Collect {
            self.clone()
        }
    }

    #[test]
    fn lines_carry_time_level_program_pid_and_session() {
        let out = Collect::default();
        let subscriber = tracing_subscriber::fmt()
            .event_format(LineFormat {
                program: "alyosha-acp".to_owned(),
                timezone: chrono_tz::Europe::Amsterdam,
                now: Arc::new(|| Utc.with_ymd_and_hms(2026, 11, 25, 9, 0, 0).unwrap()),
            })
            .with_writer(out.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("session", id = "s1");
            let _entered = span.enter();
            tracing::info!(tool = "read_file", call_id = "call_a", "tool call finished");
        });
        let text = String::from_utf8(out.0.lock().unwrap().clone()).unwrap();
        let expected = format!(
            "2026-11-25T10:00:00.000+01:00 INFO alyosha-acp {} session{{id=\"s1\"}}: tool call finished tool=\"read_file\" call_id=\"call_a\"\n",
            std::process::id()
        );
        assert_eq!(text, expected);
    }
}
