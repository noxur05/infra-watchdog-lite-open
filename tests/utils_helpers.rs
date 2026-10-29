//! Unit tests for the pure helper tables in `src/utils.rs`.
//!
//! These functions have no I/O but a typo in any arm is a silent product bug:
//! a mislabeled host (`region_to_country`) or a failed SSH login
//! (`ssh_user_for`). The tests pin the current behavior so those tables can't
//! drift unnoticed.

use vpn_watchdog::utils::{flag_emoji_from_cc, region_to_country, ssh_user_for};

const UNKNOWN: (&str, &str) = ("🌐", "Unknown");

// ---------------------------------------------------------------------------
// region_to_country (Selectel-only bot)
// ---------------------------------------------------------------------------

#[test]
fn region_selectel_prefix_includes_gis_as_russia() {
    assert_eq!(region_to_country("selectel", "ru-7"), ("🇷🇺", "Russia"));
    assert_eq!(region_to_country("selectel", "gis-1"), ("🇷🇺", "Russia"));
    assert_eq!(region_to_country("selectel", "uz-1"), ("🇺🇿", "Uzbekistan"));
    assert_eq!(region_to_country("selectel", "kz-1"), ("🇰🇿", "Kazakhstan"));
    assert_eq!(region_to_country("selectel", "ke-1"), ("🇰🇪", "Kenya"));
    assert_eq!(region_to_country("selectel", "xx-1"), UNKNOWN);
}

#[test]
fn region_unknown_provider_falls_back() {
    assert_eq!(region_to_country("nonexistent", "whatever"), UNKNOWN);
    assert_eq!(region_to_country("", ""), UNKNOWN);
}

// ---------------------------------------------------------------------------
// ssh_user_for — Selectel logs in as root.
// ---------------------------------------------------------------------------

#[test]
fn ssh_user_is_always_root() {
    for p in ["selectel", "unknown", ""] {
        assert_eq!(ssh_user_for(p), "root", "{p} should log in as root");
    }
}

// ---------------------------------------------------------------------------
// flag_emoji_from_cc — ISO alpha-2 → regional-indicator flag.
// ---------------------------------------------------------------------------

#[test]
fn flag_from_cc_maps_uppercase_pairs() {
    assert_eq!(flag_emoji_from_cc("DE"), "🇩🇪");
    assert_eq!(flag_emoji_from_cc("NL"), "🇳🇱");
    assert_eq!(flag_emoji_from_cc("US"), "🇺🇸");
}

#[test]
fn flag_from_cc_is_case_insensitive() {
    assert_eq!(flag_emoji_from_cc("de"), flag_emoji_from_cc("DE"));
}

#[test]
fn flag_from_cc_drops_non_alphabetic() {
    assert_eq!(flag_emoji_from_cc("1!"), "");
    assert_eq!(flag_emoji_from_cc(""), "");
    // Non-letters are filtered, letters still map.
    assert_eq!(flag_emoji_from_cc("D-E"), "🇩🇪");
}
