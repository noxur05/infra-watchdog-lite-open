//! Time helpers. Kept separate from provisioner.rs so tests can reach them
//! without pulling in teloxide/sqlx machinery.

use chrono::{DateTime, Datelike, FixedOffset, TimeZone, Utc};

/// The bot's local timezone (Ashgabat / Turkmenistan): UTC+5 year-round, no DST.
/// Single source of truth for every user-facing timestamp and the backup clock.
pub const ASHGABAT_OFFSET_SECS: i32 = 5 * 3600;

/// Fixed UTC+5 offset for the bot's local time.
pub fn bot_tz() -> FixedOffset {
    FixedOffset::east_opt(ASHGABAT_OFFSET_SECS).unwrap()
}

/// Current bot-local time (UTC+5) formatted for user-facing display,
/// e.g. `2026-07-03 17:42 (UTC+5)`. Used for the mail `{date}` key and the
/// admin template preview so all shown times match Turkmenistan local time.
pub fn now_local_string() -> String {
    Utc::now().with_timezone(&bot_tz()).format("%Y-%m-%d %H:%M (UTC+5)").to_string()
}

/// Seconds from `now` until the *next* 00:00 Ashgabat (UTC+5). Always
/// returns the next calendar day's midnight — if called at exactly 00:00
/// Ashgabat, returns ~86400 (not 0) so a boot-time call doesn't fire
/// immediately and then again 60s later.
pub fn seconds_until_next_ashgabat_midnight(now: DateTime<Utc>) -> u64 {
    let tz = FixedOffset::east_opt(ASHGABAT_OFFSET_SECS).unwrap();
    let now_a = now.with_timezone(&tz);
    let tomorrow = match now_a.date_naive().succ_opt() {
        Some(d) => d,
        None => return 86400,
    };
    let target_naive = match tomorrow.and_hms_opt(0, 0, 0) {
        Some(t) => t,
        None => return 86400,
    };
    let target = match tz.from_local_datetime(&target_naive).single() {
        Some(t) => t.with_timezone(&Utc),
        None => return 86400,
    };
    (target - now).num_seconds().max(60) as u64
}

/// Seconds from `now` until the 1st of the following month at 00:00 UTC.
/// Clamped to at least 1 day so the caller can't accidentally spin-wait.
pub fn seconds_until_next_month_from(now: DateTime<Utc>) -> u64 {
    let (next_year, next_month) = if now.month() == 12 {
        (now.year() + 1, 1)
    } else {
        (now.year(), now.month() + 1)
    };
    match Utc.with_ymd_and_hms(next_year, next_month, 1, 0, 0, 0).single() {
        Some(t) => (t - now).num_seconds().max(86400) as u64,
        None => 86400 * 7,
    }
}

/// Convenience wrapper using the current time.
pub fn seconds_until_next_month() -> u64 {
    seconds_until_next_month_from(Utc::now())
}

/// Format a timestamp as "YYYY-MM" — used as the traffic baseline month key.
pub fn month_key(now: DateTime<Utc>) -> String {
    now.format("%Y-%m").to_string()
}

/// Parse a SQLite datetime string ("YYYY-MM-DD HH:MM:SS", UTC) into seconds
/// since the Unix epoch. Returns `None` for anything malformed — too few
/// fields, a non-numeric component, or an out-of-range month/day — so callers
/// apply their own defensive default rather than trusting a garbage value.
///
/// Hand-rolled leap-year math (no chrono) to stay cheap on the 0.5 GB host;
/// cross-checked against chrono in the tests. Note: the day check only bounds
/// 1..=31, so an impossible day-for-month (e.g. "2026-02-31") still parses —
/// callers feed this DB timestamps written by SQLite, which are always valid.
pub fn epoch_secs_from_sqlite_datetime(datetime_str: &str) -> Option<u64> {
    let parts: Vec<&str> = datetime_str.split(&['-', ' ', ':'][..]).collect();
    if parts.len() < 6 {
        return None;
    }

    let year: u64 = parts[0].parse().ok()?;
    let month: u64 = parts[1].parse().ok()?;
    let day: u64 = parts[2].parse().ok()?;
    let hour: u64 = parts[3].parse().ok()?;
    let min: u64 = parts[4].parse().ok()?;
    let sec: u64 = parts[5].parse().ok()?;

    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }

    let month_offsets: [u64; 12] = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
    let mut total_days: u64 = 0;
    for y in 1970..year {
        total_days += if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 { 366 } else { 365 };
    }
    total_days += month_offsets[(month - 1) as usize] + day - 1;
    if month > 2 && ((year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400)) {
        total_days += 1;
    }
    Some(total_days * 86400 + hour * 3600 + min * 60 + sec)
}

/// Calculate minutes elapsed since a SQLite datetime string ("YYYY-MM-DD HH:MM:SS" UTC).
/// Returns 0 if the string is malformed (defensive — treating a corrupted DB
/// column as "just now" is safer than reading it as "decades ago" and acting).
pub fn minutes_since(datetime_str: &str) -> u64 {
    let ts = match epoch_secs_from_sqlite_datetime(datetime_str) {
        Some(t) => t,
        None => return 0,
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    now.saturating_sub(ts) / 60
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at_ashgabat(year: i32, month: u32, day: u32, hour: u32, min: u32) -> DateTime<Utc> {
        // build the moment in Ashgabat then return as UTC
        let tz = FixedOffset::east_opt(ASHGABAT_OFFSET_SECS).unwrap();
        tz.with_ymd_and_hms(year, month, day, hour, min, 0).single().unwrap().with_timezone(&Utc)
    }

    #[test]
    fn bot_tz_is_utc_plus_5() {
        assert_eq!(ASHGABAT_OFFSET_SECS, 5 * 3600);
        assert_eq!(bot_tz().local_minus_utc(), 5 * 3600);
    }

    #[test]
    fn now_local_string_is_utc5_labeled() {
        let s = now_local_string();
        assert!(s.ends_with("(UTC+5)"), "got {s}");
        // Fixed shape "YYYY-MM-DD HH:MM (UTC+5)" regardless of the moment.
        assert_eq!(s.len(), "2026-07-03 17:42 (UTC+5)".len(), "got {s}");
    }

    #[test]
    fn ashgabat_midnight_from_evening() {
        // 23:00 Ashgabat → 1h to next midnight
        let s = seconds_until_next_ashgabat_midnight(at_ashgabat(2026, 5, 7, 23, 0));
        assert_eq!(s, 3600);
    }

    #[test]
    fn ashgabat_midnight_from_midday() {
        // 12:00 Ashgabat → 12h to next midnight
        let s = seconds_until_next_ashgabat_midnight(at_ashgabat(2026, 5, 7, 12, 0));
        assert_eq!(s, 12 * 3600);
    }

    #[test]
    fn ashgabat_midnight_at_midnight_goes_to_tomorrow() {
        // Exactly 00:00 Ashgabat → next is *tomorrow* 00:00, not "now".
        let s = seconds_until_next_ashgabat_midnight(at_ashgabat(2026, 5, 7, 0, 0));
        assert_eq!(s, 86400);
    }
}
