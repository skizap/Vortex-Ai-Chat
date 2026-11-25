//! Pure time math for job schedules: cron parsing, next-tick computation,
//! missed-tick enumeration, DST rules, and overlap decisions. No I/O, no
//! store access; the injectable [`Clock`] trait lets tests run without
//! wall-clock sleeps. SA2 owns the implementation (Phase 0 freeze: surface).
//!
//! DST contract: nonexistent local times (spring forward) are skipped;
//! ambiguous local times (fall back) run exactly once, at the first/earliest
//! instant. The `cron` crate's own iterator fires ambiguous times twice, so
//! the core math enumerates wall-clock candidates in a fixed-offset (UTC)
//! interpretation and resolves each candidate through
//! `TimeZone::from_local_datetime` under the contract above. All `after` /
//! `now` comparisons happen on absolute instants, never on wall-clock fields.
//!
//! User schedules are standard 5-field cron (minute hour day-of-month month
//! day-of-week). The `cron` crate parses 6/7 fields with seconds first, so a
//! validated 5-field expression is prefixed with `"0 "` before parsing;
//! 6/7-field inputs are rejected as invalid user schedules.

use chrono::{DateTime, Local, TimeZone, Utc};
use cron::Schedule;
use std::collections::VecDeque;
use std::str::FromStr;
use vortex_types::{OverlapPolicy, ScheduleSpec};

/// Upper bound on the number of ticks returned by [`missed_ticks`]. When more
/// ticks fell due inside the window, the MOST RECENT 512 are kept and older
/// ones are dropped, so a long downtime cannot produce an unbounded list.
const MAX_MISSED_TICKS: usize = 512;

/// Injectable time source. Tests use a fake; production uses [`SystemClock`].
pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Local>;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Local> {
        Local::now()
    }
}

/// Validate a schedule (5-field cron expression + timezone). Returns an
/// actionable message on failure.
pub fn validate_schedule(spec: &ScheduleSpec) -> Result<(), String> {
    validate_tz(&spec.tz)?;
    parse_user_cron(&spec.cron).map(|_| ())
}

/// The next fire time strictly after `after`, in the schedule's timezone
/// (converted to local for storage/display). `Ok(None)` when the expression
/// can never fire again (e.g. Feb 30).
pub fn next_tick(
    spec: &ScheduleSpec,
    after: &DateTime<Local>,
) -> Result<Option<DateTime<Local>>, String> {
    let schedule = parse_schedule(spec)?;
    Ok(next_tick_in(&schedule, &Local, after))
}

/// Missed fire times in `(last_fired, now]`, ascending, bounded (the caller
/// applies the missed-tick policy; enumeration itself is capped so a long
/// downtime cannot produce an unbounded list). At most [`MAX_MISSED_TICKS`]
/// (512) entries are returned; when more ticks fell due in the window, the
/// MOST RECENT 512 are kept. Empty when `last_fired >= now`.
pub fn missed_ticks(
    spec: &ScheduleSpec,
    last_fired: &DateTime<Local>,
    now: &DateTime<Local>,
) -> Result<Vec<DateTime<Local>>, String> {
    let schedule = parse_schedule(spec)?;
    Ok(missed_ticks_in(&schedule, &Local, last_fired, now))
}

/// Pure single-flight/overlap decision. `live` is the number of non-terminal
/// occurrences of the job right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlapDecision {
    Fire,
    Skip,
    QueueOne,
}

pub fn overlap_decision(policy: OverlapPolicy, live: usize) -> OverlapDecision {
    if live == 0 {
        return OverlapDecision::Fire;
    }
    match policy {
        OverlapPolicy::Skip => OverlapDecision::Skip,
        OverlapPolicy::QueueOne => {
            if live == 1 {
                OverlapDecision::QueueOne
            } else {
                OverlapDecision::Skip
            }
        }
    }
}

fn validate_tz(tz: &str) -> Result<(), String> {
    match tz.trim() {
        "" | "local" => Ok(()),
        other => Err(format!(
            "unsupported timezone '{other}': only the server's local timezone is supported \
             (set tz to \"local\" or leave it empty)"
        )),
    }
}

fn parse_user_cron(expr: &str) -> Result<Schedule, String> {
    let trimmed = expr.trim();
    if trimmed.is_empty() {
        return Err(
            "cron expression is empty: expected 5 fields (minute hour day-of-month month \
             day-of-week), e.g. \"0 9 * * *\""
                .into(),
        );
    }
    let fields: Vec<&str> = trimmed.split_whitespace().collect();
    if fields.len() != 5 {
        return Err(format!(
            "invalid cron expression '{trimmed}': expected 5 fields (minute hour day-of-month \
             month day-of-week), got {} (seconds and years fields are not accepted)",
            fields.len()
        ));
    }
    let normalized = format!("0 {}", fields.join(" "));
    Schedule::from_str(&normalized).map_err(|e| format!("invalid cron expression '{trimmed}': {e}"))
}

fn parse_schedule(spec: &ScheduleSpec) -> Result<Schedule, String> {
    validate_tz(&spec.tz)?;
    parse_user_cron(&spec.cron)
}

/// Wall-clock candidates from the cron schedule, resolved to absolute
/// instants in `tz` under the DST contract: nonexistent local times are
/// skipped (`LocalResult::None`); ambiguous local times resolve to their
/// earliest instant (`LocalResult::Ambiguous` / `Single` via `earliest()`).
///
/// Candidates are enumerated by interpreting the wall clock as UTC, so the
/// cron crate's own DST handling (which emits ambiguous times twice) never
/// applies. The first instant at which a wall-clock reading occurs is
/// strictly increasing in that reading, so the resolved sequence is
/// strictly increasing in absolute time.
fn resolved_ticks<'a, Tz: TimeZone>(
    schedule: &'a Schedule,
    tz: &'a Tz,
    after: &DateTime<Tz>,
) -> impl Iterator<Item = DateTime<Tz>> + 'a {
    let start = Utc.from_utc_datetime(&after.naive_local());
    schedule
        .after(&start)
        .filter_map(move |candidate| tz.from_local_datetime(&candidate.naive_utc()).earliest())
}

fn next_tick_in<Tz: TimeZone>(
    schedule: &Schedule,
    tz: &Tz,
    after: &DateTime<Tz>,
) -> Option<DateTime<Tz>> {
    resolved_ticks(schedule, tz, after).find(|tick| tick > after)
}

fn missed_ticks_in<Tz: TimeZone>(
    schedule: &Schedule,
    tz: &Tz,
    last_fired: &DateTime<Tz>,
    now: &DateTime<Tz>,
) -> Vec<DateTime<Tz>> {
    if last_fired >= now {
        return Vec::new();
    }
    let mut ticks: VecDeque<DateTime<Tz>> = VecDeque::new();
    for tick in resolved_ticks(schedule, tz, last_fired) {
        if tick > *now {
            break;
        }
        if tick > *last_fired {
            ticks.push_back(tick);
            if ticks.len() > MAX_MISSED_TICKS {
                ticks.pop_front();
            }
        }
    }
    ticks.into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::offset::LocalResult;
    use chrono::{Duration, NaiveDate, NaiveDateTime, Timelike};
    use chrono_tz::America::New_York as NY;
    use chrono_tz::Tz;
    use std::sync::{Arc, Mutex};

    fn spec(cron: &str) -> ScheduleSpec {
        ScheduleSpec {
            cron: cron.into(),
            ..Default::default()
        }
    }

    fn spec_tz(cron: &str, tz: &str) -> ScheduleSpec {
        ScheduleSpec {
            cron: cron.into(),
            tz: tz.into(),
            ..Default::default()
        }
    }

    fn sched(cron: &str) -> Schedule {
        parse_user_cron(cron).unwrap()
    }

    fn naive(y: i32, m: u32, d: u32, h: u32, min: u32, s: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, m, d)
            .unwrap()
            .and_hms_opt(h, min, s)
            .unwrap()
    }

    /// Unambiguous New York wall time (panics on nonexistent/ambiguous input,
    /// which keeps fixtures honest).
    fn ny(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Tz> {
        NY.with_ymd_and_hms(y, m, d, h, min, 0).unwrap()
    }

    /// Exact New York instant via RFC 3339 offset (works for ambiguous and
    /// post-transition times too).
    fn ny_rfc(s: &str) -> DateTime<Tz> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&NY)
    }

    struct FakeClock {
        now: Mutex<DateTime<Local>>,
    }

    impl FakeClock {
        fn new(t: DateTime<Local>) -> Self {
            Self { now: Mutex::new(t) }
        }

        fn set(&self, t: DateTime<Local>) {
            *self.now.lock().unwrap() = t;
        }
    }

    impl Clock for FakeClock {
        fn now(&self) -> DateTime<Local> {
            *self.now.lock().unwrap()
        }
    }

    fn read_clock(clock: &dyn Clock) -> DateTime<Local> {
        clock.now()
    }

    #[test]
    fn five_field_every_five_minutes() {
        let s = sched("*/5 * * * *");
        let tick = next_tick_in(&s, &NY, &ny(2026, 6, 15, 12, 3)).unwrap();
        assert_eq!(tick, ny(2026, 6, 15, 12, 5));
        assert_eq!(tick.second(), 0, "5-field cron fires at second 0");
    }

    #[test]
    fn daily_nine_am() {
        let s = sched("0 9 * * *");
        assert_eq!(
            next_tick_in(&s, &NY, &ny(2026, 6, 15, 8, 0)).unwrap(),
            ny(2026, 6, 15, 9, 0)
        );
        assert_eq!(
            next_tick_in(&s, &NY, &ny(2026, 6, 15, 10, 0)).unwrap(),
            ny(2026, 6, 16, 9, 0)
        );
    }

    #[test]
    fn wrong_field_counts_rejected() {
        for expr in [
            "0 */5 * * * *",
            "0 0 9 * * * 2026",
            "0 9 * *",
            "* * *",
            "@daily",
        ] {
            let err = validate_schedule(&spec(expr)).unwrap_err();
            assert!(err.contains("5 fields"), "{expr}: {err}");
        }
    }

    #[test]
    fn invalid_expressions_rejected() {
        for expr in ["60 * * * *", "* 25 * * *", "0 0 32 * *", "a b c d e", ""] {
            assert!(validate_schedule(&spec(expr)).is_err(), "{expr}");
        }
        let err = validate_schedule(&spec("")).unwrap_err();
        assert!(err.contains("empty"), "{err}");
    }

    #[test]
    fn timezone_validation() {
        assert!(validate_schedule(&spec_tz("0 9 * * *", "local")).is_ok());
        assert!(validate_schedule(&spec_tz("0 9 * * *", "")).is_ok());
        for tz in ["UTC", "America/New_York", "Europe/Berlin", "localtime"] {
            let err = validate_schedule(&spec_tz("0 9 * * *", tz)).unwrap_err();
            assert!(
                err.contains("only the server's local timezone is supported"),
                "{tz}: {err}"
            );
        }
    }

    #[test]
    fn valid_expressions_accepted() {
        for expr in [
            "*/5 * * * *",
            "0 9 * * *",
            "0 9 * * MON-FRI",
            "*/15 9-17 * * 1-5",
            "0 0 29 2 *",
            "0 0 30 2 *",
        ] {
            assert!(validate_schedule(&spec(expr)).is_ok(), "{expr}");
        }
    }

    #[test]
    fn month_boundary() {
        let s = sched("0 0 1 * *");
        assert_eq!(
            next_tick_in(&s, &NY, &ny(2026, 1, 31, 12, 0)).unwrap(),
            ny(2026, 2, 1, 0, 0)
        );
    }

    #[test]
    fn year_boundary() {
        let s = sched("0 0 * * *");
        assert_eq!(
            next_tick_in(&s, &NY, &ny(2026, 12, 31, 12, 0)).unwrap(),
            ny(2027, 1, 1, 0, 0)
        );
    }

    #[test]
    fn leap_day() {
        let s = sched("0 0 29 2 *");
        assert_eq!(
            next_tick_in(&s, &NY, &ny(2026, 3, 1, 0, 0)).unwrap(),
            ny(2028, 2, 29, 0, 0),
            "2027 is not a leap year"
        );
        assert_eq!(
            next_tick_in(&s, &NY, &ny(2028, 2, 29, 0, 0)).unwrap(),
            ny(2032, 2, 29, 0, 0)
        );
    }

    #[test]
    fn never_fires() {
        let s = sched("0 0 30 2 *");
        assert!(next_tick_in(&s, &NY, &ny(2026, 1, 1, 0, 0)).is_none());
        assert!(missed_ticks_in(&s, &NY, &ny(2026, 1, 1, 0, 0), &ny(2027, 1, 1, 0, 0)).is_empty());
        let after = Local.from_utc_datetime(&naive(2026, 6, 15, 12, 0, 0));
        assert_eq!(next_tick(&spec("0 0 30 2 *"), &after).unwrap(), None);
    }

    #[test]
    fn spring_forward_daily_skips_nonexistent() {
        assert!(matches!(
            NY.with_ymd_and_hms(2026, 3, 8, 2, 30, 0),
            LocalResult::None
        ));
        let s = sched("30 2 * * *");
        let tick = next_tick_in(&s, &NY, &ny(2026, 3, 8, 0, 0)).unwrap();
        assert_eq!(tick, ny_rfc("2026-03-09T02:30:00-04:00"));
        let missed = missed_ticks_in(&s, &NY, &ny(2026, 3, 7, 3, 0), &ny(2026, 3, 9, 3, 0));
        assert_eq!(missed, vec![ny_rfc("2026-03-09T02:30:00-04:00")]);
    }

    #[test]
    fn spring_forward_hourly() {
        let s = sched("0 * * * *");
        let tick = next_tick_in(&s, &NY, &ny(2026, 3, 8, 1, 0)).unwrap();
        assert_eq!(tick, ny_rfc("2026-03-08T03:00:00-04:00"));
        let missed = missed_ticks_in(&s, &NY, &ny(2026, 3, 8, 0, 30), &ny(2026, 3, 8, 4, 0));
        assert_eq!(
            missed,
            vec![
                ny(2026, 3, 8, 1, 0),
                ny_rfc("2026-03-08T03:00:00-04:00"),
                ny_rfc("2026-03-08T04:00:00-04:00"),
            ],
            "no 02:00 fire inside the spring-forward gap"
        );
    }

    #[test]
    fn fall_back_daily_fires_once() {
        assert!(matches!(
            NY.with_ymd_and_hms(2026, 11, 1, 1, 30, 0),
            LocalResult::Ambiguous(_, _)
        ));
        let s = sched("30 1 * * *");
        let tick = next_tick_in(&s, &NY, &ny(2026, 11, 1, 0, 0)).unwrap();
        assert_eq!(
            tick,
            ny_rfc("2026-11-01T01:30:00-04:00"),
            "ambiguous time fires at the first (EDT) instant"
        );
        let next = next_tick_in(&s, &NY, &tick).unwrap();
        assert_eq!(
            next,
            ny_rfc("2026-11-02T01:30:00-05:00"),
            "the second (EST) 01:30 is not a separate fire"
        );
        let missed = missed_ticks_in(&s, &NY, &ny(2026, 10, 31, 12, 0), &ny(2026, 11, 1, 12, 0));
        assert_eq!(missed, vec![ny_rfc("2026-11-01T01:30:00-04:00")]);
    }

    #[test]
    fn fall_back_hourly_fires_once() {
        let s = sched("0 * * * *");
        let missed = missed_ticks_in(&s, &NY, &ny(2026, 11, 1, 0, 30), &ny(2026, 11, 1, 3, 0));
        assert_eq!(
            missed,
            vec![
                ny_rfc("2026-11-01T01:00:00-04:00"),
                ny_rfc("2026-11-01T02:00:00-05:00"),
                ny_rfc("2026-11-01T03:00:00-05:00"),
            ],
            "the repeated 01:00 hour fires exactly once"
        );
    }

    #[test]
    fn fall_back_window_with_second_pass_now() {
        let s = sched("30 1 * * *");
        let last = ny_rfc("2026-11-01T01:00:00-04:00");
        let now = ny_rfc("2026-11-01T01:15:00-05:00");
        let missed = missed_ticks_in(&s, &NY, &last, &now);
        assert_eq!(
            missed,
            vec![ny_rfc("2026-11-01T01:30:00-04:00")],
            "01:30 EDT is absolutely inside (last, now] even though its wall \
             clock is ahead of now's second-pass wall clock"
        );
    }

    #[test]
    fn next_tick_after_second_pass() {
        let s = sched("30 1 * * *");
        let after = ny_rfc("2026-11-01T01:29:59-05:00");
        let tick = next_tick_in(&s, &NY, &after).unwrap();
        assert_eq!(
            tick,
            ny_rfc("2026-11-02T01:30:00-05:00"),
            "the designated (earliest) 01:30 instant already passed; the \
             second-pass 01:30 is never a fire time"
        );
    }

    #[test]
    fn missed_ticks_ascending_and_boundaries() {
        let s = sched("0 9 * * *");
        let last = ny(2026, 6, 1, 9, 0);
        let now = ny(2026, 6, 4, 9, 0);
        let missed = missed_ticks_in(&s, &NY, &last, &now);
        assert_eq!(
            missed,
            vec![
                ny(2026, 6, 2, 9, 0),
                ny(2026, 6, 3, 9, 0),
                ny(2026, 6, 4, 9, 0)
            ],
            "tick at last_fired excluded, tick at now included, ascending"
        );
    }

    #[test]
    fn missed_ticks_cap_keeps_most_recent() {
        let s = sched("* * * * *");
        let last = ny(2026, 6, 15, 0, 0);
        let now = ny(2026, 6, 15, 10, 0);
        let missed = missed_ticks_in(&s, &NY, &last, &now);
        assert_eq!(
            missed.len(),
            512,
            "600 ticks fell due; keep most recent 512"
        );
        assert_eq!(missed.first().unwrap(), &ny(2026, 6, 15, 1, 29));
        assert_eq!(missed.last().unwrap(), &now);
        assert!(missed.windows(2).all(|w| w[0] < w[1]));

        let exact = missed_ticks_in(&s, &NY, &last, &ny(2026, 6, 15, 8, 32));
        assert_eq!(exact.len(), 512, "exactly 512 ticks are all kept");
        assert_eq!(exact.first().unwrap(), &ny(2026, 6, 15, 0, 1));
    }

    #[test]
    fn missed_ticks_empty_windows() {
        let s = sched("0 9 * * *");
        assert!(
            missed_ticks_in(&s, &NY, &ny(2026, 6, 15, 9, 0), &ny(2026, 6, 15, 9, 0)).is_empty()
        );
        assert!(
            missed_ticks_in(&s, &NY, &ny(2026, 6, 15, 10, 0), &ny(2026, 6, 15, 9, 0)).is_empty()
        );
        assert!(
            missed_ticks_in(&s, &NY, &ny(2026, 6, 15, 10, 0), &ny(2026, 6, 15, 11, 0)).is_empty()
        );
    }

    #[test]
    fn overlap_decision_truth_table() {
        let cases = [
            (OverlapPolicy::Skip, 0, OverlapDecision::Fire),
            (OverlapPolicy::Skip, 1, OverlapDecision::Skip),
            (OverlapPolicy::Skip, 2, OverlapDecision::Skip),
            (OverlapPolicy::QueueOne, 0, OverlapDecision::Fire),
            (OverlapPolicy::QueueOne, 1, OverlapDecision::QueueOne),
            (OverlapPolicy::QueueOne, 2, OverlapDecision::Skip),
            (OverlapPolicy::QueueOne, 7, OverlapDecision::Skip),
        ];
        for (policy, live, want) in cases {
            assert_eq!(
                overlap_decision(policy, live),
                want,
                "{policy:?} live={live}"
            );
        }
    }

    #[test]
    fn fake_clock_is_injectable() {
        let t0 = Local.from_utc_datetime(&naive(2026, 6, 15, 12, 0, 0));
        let clock = FakeClock::new(t0);
        assert_eq!(read_clock(&clock), t0);
        let t1 = t0 + Duration::hours(3);
        clock.set(t1);
        assert_eq!(read_clock(&clock), t1);
        let shared: Arc<dyn Clock> = Arc::new(FakeClock::new(t1));
        assert_eq!(shared.now(), t1);
    }

    #[test]
    fn system_clock_tracks_now() {
        let before = Local::now();
        let tick = SystemClock.now();
        let after = Local::now();
        assert!(tick >= before && tick <= after);
    }

    #[test]
    fn strictly_after_semantics() {
        let s = sched("0 9 * * *");
        assert_eq!(
            next_tick_in(&s, &NY, &ny(2026, 6, 15, 9, 0)).unwrap(),
            ny(2026, 6, 16, 9, 0),
            "a tick exactly at `after` is not returned"
        );
        let s5 = sched("*/5 * * * *");
        assert_eq!(
            next_tick_in(&s5, &NY, &ny(2026, 6, 15, 12, 5)).unwrap(),
            ny(2026, 6, 15, 12, 10)
        );
    }

    #[test]
    fn public_local_api() {
        let after = Local.from_utc_datetime(&naive(2026, 6, 15, 12, 3, 0));
        let tick = next_tick(&spec("*/5 * * * *"), &after).unwrap().unwrap();
        assert!(tick > after);
        assert!(tick.minute().is_multiple_of(5));
        assert_eq!(tick.second(), 0);
        assert!(tick - after <= Duration::minutes(5));

        let last = Local.from_utc_datetime(&naive(2026, 6, 1, 9, 0, 0));
        let now = Local.from_utc_datetime(&naive(2026, 6, 3, 9, 0, 0));
        let missed = missed_ticks(&spec("0 9 * * *"), &last, &now).unwrap();
        assert_eq!(missed.len(), 2);
        assert!(missed.windows(2).all(|w| w[0] < w[1]));
        assert!(missed.iter().all(|t| *t > last && *t <= now));

        assert!(next_tick(&spec("not a cron"), &after).is_err());
        assert!(missed_ticks(&spec_tz("0 9 * * *", "UTC"), &last, &now).is_err());
        assert!(next_tick(&spec_tz("0 9 * * *", "UTC"), &after).is_err());
    }
}
