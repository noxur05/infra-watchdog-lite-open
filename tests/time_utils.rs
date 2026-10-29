use chrono::{NaiveDateTime, TimeZone, Utc};
use vpn_watchdog::time_utils::{
    epoch_secs_from_sqlite_datetime, minutes_since, month_key, seconds_until_next_month_from,
};

#[test]
fn seconds_until_next_month_mid_month() {
    // April 15 2026 00:00 UTC → next month is May 1 2026 00:00 UTC
    let now = Utc.with_ymd_and_hms(2026, 4, 15, 0, 0, 0).unwrap();
    let secs = seconds_until_next_month_from(now);
    // 16 days × 86400s = 1,382,400
    assert_eq!(secs, 16 * 86400);
}

#[test]
fn seconds_until_next_month_last_day() {
    // Jan 31 2026 23:00 UTC → Feb 1 2026 00:00 UTC is 1h away, clamped to 1 day
    let now = Utc.with_ymd_and_hms(2026, 1, 31, 23, 0, 0).unwrap();
    let secs = seconds_until_next_month_from(now);
    assert_eq!(secs, 86400); // clamped minimum
}

#[test]
fn seconds_until_next_month_december_rolls_year() {
    // Dec 15 2026 → Jan 1 2027 — must not overflow month
    let now = Utc.with_ymd_and_hms(2026, 12, 15, 0, 0, 0).unwrap();
    let secs = seconds_until_next_month_from(now);
    // 17 days × 86400s
    assert_eq!(secs, 17 * 86400);
}

#[test]
fn month_key_formats_correctly() {
    let t = Utc.with_ymd_and_hms(2026, 4, 15, 12, 30, 45).unwrap();
    assert_eq!(month_key(t), "2026-04");

    let t2 = Utc.with_ymd_and_hms(2026, 12, 1, 0, 0, 0).unwrap();
    assert_eq!(month_key(t2), "2026-12");
}

#[test]
fn month_key_changes_on_first_of_month() {
    let last = Utc.with_ymd_and_hms(2026, 4, 30, 23, 59, 59).unwrap();
    let first = Utc.with_ymd_and_hms(2026, 5, 1, 0, 0, 0).unwrap();
    assert_ne!(month_key(last), month_key(first));
    assert_eq!(month_key(last), "2026-04");
    assert_eq!(month_key(first), "2026-05");
}

/// Trusted reference: parse the same string with chrono and take its epoch.
fn chrono_epoch(s: &str) -> u64 {
    NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
        .unwrap()
        .and_utc()
        .timestamp() as u64
}

#[test]
fn epoch_matches_chrono_across_leap_edges() {
    // Includes /4, /100, /400 leap rules, Feb 29, and a March 1 (which crosses
    // the leap-day boundary) so the hand-rolled offsets are validated end-to-end.
    let cases = [
        "1970-01-01 00:00:00",
        "1970-01-01 00:00:01",
        "2000-02-29 12:00:00", // /400 leap year
        "2024-02-29 23:59:59", // /4 leap year
        "2023-03-01 00:00:00", // non-leap March
        "2024-03-01 00:00:00", // leap-year March (offset + 1)
        "2026-06-19 14:30:45",
        "2099-12-31 23:59:59",
    ];
    for s in cases {
        assert_eq!(
            epoch_secs_from_sqlite_datetime(s),
            Some(chrono_epoch(s)),
            "epoch mismatch for {s}"
        );
    }
}

#[test]
fn epoch_of_unix_epoch_is_zero() {
    assert_eq!(epoch_secs_from_sqlite_datetime("1970-01-01 00:00:00"), Some(0));
}

#[test]
fn epoch_rejects_malformed_strings() {
    assert_eq!(epoch_secs_from_sqlite_datetime(""), None);
    assert_eq!(epoch_secs_from_sqlite_datetime("not a date"), None);
    assert_eq!(epoch_secs_from_sqlite_datetime("2026-06-19"), None); // no time part
    assert_eq!(epoch_secs_from_sqlite_datetime("2026-13-01 00:00:00"), None); // month 13
    assert_eq!(epoch_secs_from_sqlite_datetime("2026-00-01 00:00:00"), None); // month 0
    assert_eq!(epoch_secs_from_sqlite_datetime("2026-06-32 00:00:00"), None); // day 32
    assert_eq!(epoch_secs_from_sqlite_datetime("2026-06-00 00:00:00"), None); // day 0
    assert_eq!(epoch_secs_from_sqlite_datetime("abcd-06-19 00:00:00"), None); // non-numeric
}

#[test]
fn minutes_since_malformed_returns_zero_not_decades() {
    // Regression: a string that splits into 6+ fields but is otherwise invalid
    // used to fall through to ts=0 and report ~29 million minutes ("55 years
    // ago"). The defensive contract is 0.
    assert_eq!(minutes_since("2026-13-01 00:00:00"), 0);
    assert_eq!(minutes_since("abcd-06-19 00:00:00"), 0);
    assert_eq!(minutes_since("not a date"), 0);
    assert_eq!(minutes_since(""), 0);
}

#[test]
fn minutes_since_old_timestamp_is_large() {
    // A real epoch-era timestamp is decades in the past — must be a big number,
    // clearly distinct from the malformed-returns-0 case above.
    assert!(minutes_since("1970-01-01 00:00:00") > 1_000_000);
}
