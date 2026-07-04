//! Per-check scheduling for daemon mode. All functions are pure (no IO/async).
//!
//! `every` is an interval string (`"30s"`, `"5m"`, `"2h"`, `"1d"`) or a 5-field
//! cron expression. A cron check is due when the next occurrence after `last_run`
//! is `<= now`; a never-run check is due immediately (fire at startup).

use chrono::{DateTime, TimeZone, Utc};
use std::str::FromStr as _;

/// A parsed schedule — either a fixed interval in seconds or a cron expression.
#[derive(Debug, Clone)]
pub enum Schedule {
    /// Run every N seconds.
    Interval(u64),
    /// Run according to a cron expression (UTC).
    Cron(Box<croner::Cron>),
}

/// Parse `every` into a [`Schedule`] — interval first, then cron, else `None`.
pub fn parse_schedule(every: &str) -> Option<Schedule> {
    if let Some(secs) = interval_secs(every) {
        return Some(Schedule::Interval(secs));
    }
    if let Ok(cron) = croner::Cron::from_str(every) {
        return Some(Schedule::Cron(Box::new(cron)));
    }
    None
}

/// Is a check with this schedule due now?
pub fn schedule_is_due(schedule: &Schedule, last_run: Option<i64>, now: i64) -> bool {
    match schedule {
        Schedule::Interval(secs) => is_due(*secs, last_run, now),
        Schedule::Cron(cron) => cron_is_due(cron.as_ref(), last_run, now),
    }
}

fn cron_is_due(cron: &croner::Cron, last_run: Option<i64>, now: i64) -> bool {
    let Some(last) = last_run else {
        return true; // never run → fire at startup
    };
    let last_dt: DateTime<Utc> = match Utc.timestamp_opt(last, 0) {
        chrono::LocalResult::Single(dt) => dt,
        _ => return false,
    };
    let now_dt: DateTime<Utc> = match Utc.timestamp_opt(now, 0) {
        chrono::LocalResult::Single(dt) => dt,
        _ => return false,
    };
    match cron.find_next_occurrence(&last_dt, false) {
        Ok(next) => next <= now_dt,
        Err(_) => false,
    }
}

/// Parse `"30s"`/`"5m"`/`"2h"`/`"1d"` into seconds; `None` on unsupported suffix.
pub fn interval_secs(s: &str) -> Option<u64> {
    let s = s.trim();
    if let Some(n) = s.strip_suffix('s') {
        return n.trim().parse::<u64>().ok();
    }
    if let Some(n) = s.strip_suffix('m') {
        return n.trim().parse::<u64>().ok().map(|v| v * 60);
    }
    if let Some(n) = s.strip_suffix('h') {
        return n.trim().parse::<u64>().ok().map(|v| v * 3600);
    }
    if let Some(n) = s.strip_suffix('d') {
        return n.trim().parse::<u64>().ok().map(|v| v * 86400);
    }
    None
}

/// Due when never run, or when `now - last_run >= every_secs`.
pub fn is_due(every_secs: u64, last_run: Option<i64>, now: i64) -> bool {
    match last_run {
        None => true,
        Some(last) => {
            let elapsed = now.saturating_sub(last);
            elapsed >= 0 && (elapsed as u64) >= every_secs
        }
    }
}

/// Should this outcome page?
///
/// Not failing → no. Failing without `sustained_secs` → yes. With it, only once
/// `now - failing_since >= sustained_secs` (and not until `failing_since` is known).
pub fn should_notify(
    is_failing: bool,
    sustained_secs: Option<u64>,
    failing_since: Option<i64>,
    now: i64,
) -> bool {
    if !is_failing {
        return false;
    }
    match sustained_secs {
        None => true,
        Some(window) => match failing_since {
            None => false,
            Some(since) => {
                let elapsed = now.saturating_sub(since);
                elapsed >= 0 && (elapsed as u64) >= window
            }
        },
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    // --- interval_secs ---

    #[test]
    fn interval_seconds() {
        assert_eq!(interval_secs("30s"), Some(30));
        assert_eq!(interval_secs("1s"), Some(1));
        assert_eq!(interval_secs("0s"), Some(0));
    }

    #[test]
    fn interval_minutes() {
        assert_eq!(interval_secs("5m"), Some(300));
        assert_eq!(interval_secs("1m"), Some(60));
        assert_eq!(interval_secs("60m"), Some(3600));
    }

    #[test]
    fn interval_hours() {
        assert_eq!(interval_secs("2h"), Some(7200));
        assert_eq!(interval_secs("1h"), Some(3600));
        assert_eq!(interval_secs("24h"), Some(86400));
    }

    #[test]
    fn interval_days() {
        assert_eq!(interval_secs("1d"), Some(86400));
        assert_eq!(interval_secs("7d"), Some(604800));
    }

    #[test]
    fn interval_bad_input_returns_none() {
        assert_eq!(interval_secs(""), None);
        assert_eq!(interval_secs("5"), None); // no suffix
        assert_eq!(interval_secs("abc"), None);
        assert_eq!(interval_secs("5x"), None); // unknown suffix
        assert_eq!(interval_secs("ms"), None); // no number
        assert_eq!(interval_secs("-1s"), None); // negative
        assert_eq!(interval_secs("1.5m"), None); // fractional
    }

    // --- is_due ---

    #[test]
    fn never_run_is_always_due() {
        assert!(is_due(60, None, 0));
        assert!(is_due(60, None, 1_000_000));
    }

    #[test]
    fn just_ran_is_not_due() {
        // 59s elapsed < 60s interval.
        assert!(!is_due(60, Some(1000), 1059));
    }

    #[test]
    fn exactly_at_boundary_is_due() {
        assert!(is_due(60, Some(1000), 1060));
    }

    #[test]
    fn past_boundary_is_due() {
        assert!(is_due(60, Some(1000), 1200));
    }

    #[test]
    fn zero_interval_always_due_after_first_run() {
        assert!(is_due(0, Some(1000), 1000));
        assert!(is_due(0, Some(1000), 1001));
    }

    // --- should_notify ---

    #[test]
    fn not_failing_never_notifies() {
        assert!(!should_notify(false, None, None, 0));
        assert!(!should_notify(false, Some(0), Some(0), 9999));
        assert!(!should_notify(false, None, Some(0), 9999));
    }

    #[test]
    fn failing_no_sustained_notifies_immediately() {
        assert!(should_notify(true, None, None, 0));
        assert!(should_notify(true, None, Some(100), 200));
    }

    #[test]
    fn failing_sustained_unknown_since_does_not_notify() {
        assert!(!should_notify(true, Some(300), None, 9999));
    }

    #[test]
    fn failing_within_sustained_window_does_not_notify() {
        // 299s elapsed < 300s window.
        assert!(!should_notify(true, Some(300), Some(1000), 1299));
    }

    #[test]
    fn failing_at_sustained_boundary_notifies() {
        assert!(should_notify(true, Some(300), Some(1000), 1300));
    }

    #[test]
    fn failing_past_sustained_window_notifies() {
        assert!(should_notify(true, Some(300), Some(1000), 1600));
    }

    // --- parse_schedule ---

    #[test]
    fn parse_schedule_recognizes_interval() {
        match parse_schedule("5m") {
            Some(Schedule::Interval(300)) => {}
            other => panic!("expected Interval(300), got {other:?}"),
        }
        match parse_schedule("30s") {
            Some(Schedule::Interval(30)) => {}
            other => panic!("expected Interval(30), got {other:?}"),
        }
        match parse_schedule("2h") {
            Some(Schedule::Interval(7200)) => {}
            other => panic!("expected Interval(7200), got {other:?}"),
        }
        match parse_schedule("1d") {
            Some(Schedule::Interval(86400)) => {}
            other => panic!("expected Interval(86400), got {other:?}"),
        }
    }

    #[test]
    fn parse_schedule_recognizes_cron() {
        match parse_schedule("0 9 * * *") {
            Some(Schedule::Cron(_)) => {}
            other => panic!("expected Cron(_), got {other:?}"),
        }
        match parse_schedule("*/5 * * * *") {
            Some(Schedule::Cron(_)) => {}
            other => panic!("expected Cron(_), got {other:?}"),
        }
    }

    #[test]
    fn parse_schedule_garbage_returns_none() {
        assert!(parse_schedule("").is_none());
        assert!(parse_schedule("garbage").is_none());
        assert!(parse_schedule("5").is_none());
        assert!(parse_schedule("5x").is_none());
        assert!(parse_schedule("* * *").is_none()); // too few fields for cron
    }

    // --- schedule_is_due: interval arm ---

    #[test]
    fn schedule_is_due_interval_matches_is_due() {
        let sched = parse_schedule("60s").unwrap();
        assert!(schedule_is_due(&sched, None, 0));
        assert!(!schedule_is_due(&sched, Some(1000), 1059));
        assert!(schedule_is_due(&sched, Some(1000), 1060));
        assert!(schedule_is_due(&sched, Some(1000), 1200));
    }

    // Cron arm uses "*/5 * * * *" (fires at :00, :05, :10 ...) on 2024-01-01 UTC.

    #[test]
    fn schedule_is_due_cron_due_when_boundary_crossed() {
        let sched = parse_schedule("*/5 * * * *").unwrap();
        // 00:05:00 boundary lies in (last_run, now], so due.
        let last = Utc
            .with_ymd_and_hms(2024, 1, 1, 0, 4, 59)
            .unwrap()
            .timestamp();
        let now = Utc
            .with_ymd_and_hms(2024, 1, 1, 0, 5, 1)
            .unwrap()
            .timestamp();
        assert!(
            schedule_is_due(&sched, Some(last), now),
            "expected due because 00:05:00 boundary is in (last_run, now]"
        );
    }

    #[test]
    fn schedule_is_due_cron_not_due_before_next_boundary() {
        let sched = parse_schedule("*/5 * * * *").unwrap();
        // Next boundary 00:10:00 is still future, so not due.
        let last = Utc
            .with_ymd_and_hms(2024, 1, 1, 0, 5, 0)
            .unwrap()
            .timestamp();
        let now = Utc
            .with_ymd_and_hms(2024, 1, 1, 0, 9, 59)
            .unwrap()
            .timestamp();
        assert!(
            !schedule_is_due(&sched, Some(last), now),
            "expected not due because next boundary 00:10:00 is still in the future"
        );
    }

    #[test]
    fn schedule_is_due_cron_never_run_is_immediately_due() {
        let sched = parse_schedule("0 9 * * *").unwrap(); // daily at 09:00
        assert!(schedule_is_due(&sched, None, 0));
        assert!(schedule_is_due(&sched, None, 1_000_000));
    }
}
