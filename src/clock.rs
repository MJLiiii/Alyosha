//! The time source and the clock line on each message.
//!
//! Each message enters history with a clock line in front, such as
//! `[Wed 2026-11-25 10:02, Europe/Amsterdam]`, giving the time it was submitted
//! in the owner's timezone. The system instruction stays fixed for the session,
//! so the time cannot go there. See `docs/design/AgentLoop.md`.

use chrono::{DateTime, Utc};
use chrono_tz::Tz;

/// Where the session driver reads the current time.
pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

/// The system's clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// A clock stopped at one instant, for tests.
#[derive(Debug, Clone, Copy)]
pub struct FixedClock(pub DateTime<Utc>);

impl Clock for FixedClock {
    fn now(&self) -> DateTime<Utc> {
        self.0
    }
}

/// Formats `at` in `timezone` as `[Wed 2026-11-25 10:02, Europe/Amsterdam]`:
/// the weekday as three letters, the date, the 24-hour time, and the
/// timezone's name.
pub fn clock_line(at: DateTime<Utc>, timezone: Tz) -> String {
    let local = at.with_timezone(&timezone);
    format!(
        "[{}, {}]",
        local.format("%a %Y-%m-%d %H:%M"),
        timezone.name()
    )
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    #[test]
    fn clock_line_uses_the_local_time_and_zone_name() {
        let winter = Utc.with_ymd_and_hms(2026, 11, 25, 9, 2, 0).unwrap();
        assert_eq!(
            clock_line(winter, chrono_tz::Europe::Amsterdam),
            "[Wed 2026-11-25 10:02, Europe/Amsterdam]"
        );
        let summer = Utc.with_ymd_and_hms(2026, 7, 1, 8, 30, 0).unwrap();
        assert_eq!(
            clock_line(summer, chrono_tz::Europe::Amsterdam),
            "[Wed 2026-07-01 10:30, Europe/Amsterdam]"
        );
    }
}
