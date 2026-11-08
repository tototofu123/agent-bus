//! Scheduled jobs: a cron expression that posts a `task.offer` into a room.
//!
//! The mechanism only — what an offer says is up to the schedule (the
//! daily digest is a schedule, not part of this feature). See
//! docs/scheduling.md.
//!
//! The tick is minute-granularity and driven by an external timer
//! (`bus schedule tick`, once a minute). A schedule fires when its next
//! occurrence after the last fire (or its creation, if never fired) is due
//! within the last minute. There is no catch-up: if the broker was down at
//! 08:00, the 08:00 digest does not fire at 10:00.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::str::FromStr;

/// The tick's firing window: an occurrence older than this is missed, not
/// caught up on.
pub const TICK_WINDOW_MS: i64 = 60_000;

/// One stored schedule. The `task.offer` payload is the full JSON object
/// (it must contain `files` as an array — see `validate_offer_payload`);
/// `text` is the envelope text.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ScheduleEntry {
    pub name: String,
    pub room: String,
    pub cron: String,
    #[serde(default = "default_timezone")]
    pub timezone: String,
    #[serde(default)]
    pub text: String,
    pub payload: Value,
    pub enabled: bool,
    pub created_at_ms: i64,
    pub last_fired_ms: Option<i64>,
}

fn default_timezone() -> String {
    "local".into()
}

/// Parse a cron expression. Accepts the 5-field crontab form
/// (`min hour dom mon dow`, seconds assumed 0), the 6-field form
/// (`sec min hour dom mon dow`), the 7-field form (+ year), and the
/// `@daily` / `@weekly` / `@monthly` / `@yearly` shorthands the `cron`
/// crate understands. Anything else is an error.
pub fn parse_cron(expr: &str) -> Result<cron::Schedule, String> {
    let trimmed = expr.trim();
    if trimmed.is_empty() {
        return Err("cron expression is empty".into());
    }
    if trimmed.starts_with('@') {
        return cron::Schedule::from_str(trimmed)
            .map_err(|e| format!("invalid cron expression {expr:?}: {e}"));
    }
    let fields = trimmed.split_whitespace().count();
    let normalized = match fields {
        5 => format!("0 {trimmed}"),
        6 | 7 => trimmed.to_string(),
        n => {
            return Err(format!(
                "cron expression has {n} fields, expected 5, 6 or 7 (or @daily/@weekly/@monthly/@yearly)"
            ))
        }
    };
    cron::Schedule::from_str(&normalized)
        .map_err(|e| format!("invalid cron expression {expr:?}: {e}"))
}

/// A schedule timezone: `local` (the broker machine's local time),
/// `UTC`, or an IANA name such as `Asia/Hong_Kong`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScheduleTz {
    Local,
    Utc,
    Named(chrono_tz::Tz),
}

pub fn resolve_tz(name: &str) -> Result<ScheduleTz, String> {
    let n = name.trim();
    if n.eq_ignore_ascii_case("local") {
        return Ok(ScheduleTz::Local);
    }
    if n.eq_ignore_ascii_case("utc") {
        return Ok(ScheduleTz::Utc);
    }
    n.parse::<chrono_tz::Tz>()
        .map(ScheduleTz::Named)
        .map_err(|_| {
            format!(
                "unknown timezone {name:?} (use local, UTC, or an IANA name like Asia/Hong_Kong)"
            )
        })
}

/// The next cron occurrence strictly after `from_ms` (epoch ms), in the
/// schedule's timezone. `Ok(None)` only when the expression has no future
/// occurrence (e.g. a year in the past).
pub fn next_occurrence_after(
    expr: &str,
    tz_name: &str,
    from_ms: i64,
) -> Result<Option<i64>, String> {
    let sched = parse_cron(expr)?;
    let tz = resolve_tz(tz_name)?;
    let from = DateTime::from_timestamp_millis(from_ms)
        .ok_or_else(|| format!("bad timestamp {from_ms}"))?;
    let next = match tz {
        ScheduleTz::Local => sched
            .after(&from.with_timezone(&chrono::Local))
            .next()
            .map(|d| d.timestamp_millis()),
        ScheduleTz::Utc => sched
            .after(&from.with_timezone(&Utc))
            .next()
            .map(|d| d.timestamp_millis()),
        ScheduleTz::Named(t) => sched
            .after(&from.with_timezone(&t))
            .next()
            .map(|d| d.timestamp_millis()),
    };
    Ok(next)
}

/// The occurrence this schedule should fire at `now_ms`, or `None` when
/// nothing is due. `Err` when the cron expression or timezone is bad.
///
/// The rule is what makes a tick idempotent within a minute: the fire
/// candidate is the first occurrence after the last fire (or the
/// creation time, if never fired). It fires only if that occurrence is
/// already due and no older than the tick window. Recording the
/// *occurrence* time (not the wall clock) as `last_fired` means a second
/// tick in the same minute finds its next occurrence in the future and
/// does nothing; a tick that runs more than a minute late finds the
/// occurrence stale and does not catch up.
pub fn due_occurrence(entry: &ScheduleEntry, now_ms: i64) -> Result<Option<i64>, String> {
    let since = entry
        .last_fired_ms
        .unwrap_or(entry.created_at_ms)
        .max(entry.created_at_ms);
    let Some(next) = next_occurrence_after(&entry.cron, &entry.timezone, since)? else {
        return Ok(None);
    };
    if next <= now_ms && now_ms - next <= TICK_WINDOW_MS {
        Ok(Some(next))
    } else {
        Ok(None)
    }
}

/// The stored payload must satisfy the `task.offer` contract.
pub fn validate_offer_payload(payload: &Value) -> Result<(), String> {
    super::Kind::TaskOffer.validate_payload(Some(payload), None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone as _;

    fn ms(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> i64 {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, s)
            .unwrap()
            .timestamp_millis()
    }

    fn entry(cron: &str, tz: &str, created_ms: i64) -> ScheduleEntry {
        ScheduleEntry {
            name: "t".into(),
            room: "#test".into(),
            cron: cron.into(),
            timezone: tz.into(),
            text: "do the thing".into(),
            payload: serde_json::json!({"files": []}),
            enabled: true,
            created_at_ms: created_ms,
            last_fired_ms: None,
        }
    }

    #[test]
    fn cron_parses_five_six_seven_fields_and_shorthands() {
        assert!(parse_cron("0 8 * * *").is_ok()); // 5-field crontab
        assert!(parse_cron("0 0 8 * * *").is_ok()); // 6-field, seconds first
        assert!(parse_cron("0 0 8 * * * 2027").is_ok()); // 7-field + year
        assert!(parse_cron("@daily").is_ok());
        assert!(parse_cron("@weekly").is_ok());
        assert!(parse_cron("  0 8 * * *  ").is_ok()); // surrounding space

        assert!(parse_cron("").is_err());
        assert!(parse_cron("0 8 * *").is_err()); // 4 fields
        assert!(parse_cron("0 8 * * * * * *").is_err()); // 8 fields
        assert!(parse_cron("not a cron").is_err());
        assert!(parse_cron("0 0 25 * * *").is_err()); // hour 25
        assert!(parse_cron("0 0 8 * * * 1999").is_ok()); // parses; never fires again
    }

    #[test]
    fn next_occurrence_utc() {
        // 08:00 UTC daily; from midnight the next one is 08:00 the same day.
        let next = next_occurrence_after("0 8 * * *", "UTC", ms(2026, 11, 8, 0, 0, 0))
            .unwrap()
            .unwrap();
        assert_eq!(next, ms(2026, 11, 8, 8, 0, 0));

        // From just after 08:00, the next one is tomorrow.
        let next = next_occurrence_after("0 8 * * *", "UTC", ms(2026, 11, 8, 8, 0, 1))
            .unwrap()
            .unwrap();
        assert_eq!(next, ms(2026, 11, 9, 8, 0, 0));

        // A year in the past has no future occurrence.
        assert_eq!(
            next_occurrence_after("0 0 8 * * * 1999", "UTC", ms(2026, 11, 8, 0, 0, 0)).unwrap(),
            None
        );
    }

    #[test]
    fn next_occurrence_named_timezone() {
        // 08:00 Asia/Hong_Kong daily. From 2026-11-08 00:00 HKT (= 16:00Z
        // on the 7th), the next 08:00 HKT is 2026-11-08 08:00 HKT,
        // i.e. 2026-11-08 00:00 UTC.
        let from = chrono_tz::Tz::Asia__Hong_Kong
            .with_ymd_and_hms(2026, 11, 8, 0, 0, 0)
            .unwrap()
            .timestamp_millis();
        let next = next_occurrence_after("0 8 * * *", "Asia/Hong_Kong", from)
            .unwrap()
            .unwrap();
        assert_eq!(next, ms(2026, 11, 8, 0, 0, 0));

        // The same cron in UTC gives a different instant — the timezone
        // actually moves the firing time.
        let next_utc = next_occurrence_after("0 8 * * *", "UTC", from)
            .unwrap()
            .unwrap();
        assert_eq!(next_utc, ms(2026, 11, 8, 8, 0, 0));
        assert_ne!(next, next_utc);

        assert!(resolve_tz("Asia/Hong_Kong").is_ok());
        assert!(resolve_tz("local").is_ok());
        assert!(resolve_tz("UTC").is_ok());
        assert!(resolve_tz("Mars/Olympus").is_err());
    }

    #[test]
    fn due_occurrence_fires_once_per_minute_and_never_catches_up() {
        // Daily 08:00 UTC, created 07:59, tick at 08:00:30 -> fires 08:00.
        let e = entry("0 8 * * *", "UTC", ms(2026, 11, 8, 7, 59, 0));
        assert_eq!(
            due_occurrence(&e, ms(2026, 11, 8, 8, 0, 30)).unwrap(),
            Some(ms(2026, 11, 8, 8, 0, 0))
        );

        // Tick 91 s after the occurrence -> missed, no catch-up.
        assert_eq!(due_occurrence(&e, ms(2026, 11, 8, 8, 1, 31)).unwrap(), None);

        // Already fired this occurrence -> a second tick in the same minute
        // does nothing.
        let mut fired = e.clone();
        fired.last_fired_ms = Some(ms(2026, 11, 8, 8, 0, 0));
        assert_eq!(
            due_occurrence(&fired, ms(2026, 11, 8, 8, 0, 45)).unwrap(),
            None
        );

        // Fired yesterday -> today's occurrence fires again.
        let mut yesterday = e.clone();
        yesterday.last_fired_ms = Some(ms(2026, 11, 7, 8, 0, 0));
        assert_eq!(
            due_occurrence(&yesterday, ms(2026, 11, 8, 8, 0, 30)).unwrap(),
            Some(ms(2026, 11, 8, 8, 0, 0))
        );

        // Before the occurrence -> nothing due.
        assert_eq!(
            due_occurrence(&e, ms(2026, 11, 8, 7, 59, 30)).unwrap(),
            None
        );

        // Every-minute cron, created 09:59:30, tick 10:00:30 -> fires 10:00.
        let every = entry("* * * * *", "UTC", ms(2026, 11, 8, 9, 59, 30));
        assert_eq!(
            due_occurrence(&every, ms(2026, 11, 8, 10, 0, 30)).unwrap(),
            Some(ms(2026, 11, 8, 10, 0, 0))
        );

        // Bad cron / bad timezone surface as errors, not as "not due".
        let bad = entry("not a cron", "UTC", ms(2026, 11, 8, 7, 59, 0));
        assert!(due_occurrence(&bad, ms(2026, 11, 8, 8, 0, 30)).is_err());
        let bad_tz = entry("0 8 * * *", "Mars/Olympus", ms(2026, 11, 8, 7, 59, 0));
        assert!(due_occurrence(&bad_tz, ms(2026, 11, 8, 8, 0, 30)).is_err());
    }

    #[test]
    fn offer_payload_contract() {
        assert!(validate_offer_payload(&serde_json::json!({"files": []})).is_ok());
        assert!(validate_offer_payload(
            &serde_json::json!({"files": ["src/**"], "claim_ttl_s": 300})
        )
        .is_ok());
        assert!(validate_offer_payload(&serde_json::json!({})).is_err());
        assert!(validate_offer_payload(&serde_json::json!({"files": "src/**"})).is_err());
        assert!(validate_offer_payload(&serde_json::json!(null)).is_err());
    }
}
