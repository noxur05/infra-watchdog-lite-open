use vpn_watchdog::utils::escape_md;

#[test]
fn escape_md_escapes_all_reserved_chars() {
    assert_eq!(escape_md("hello"), "hello");
    assert_eq!(escape_md("a.b"), "a\\.b");
    assert_eq!(escape_md("*bold*"), "\\*bold\\*");
    assert_eq!(escape_md("_under_"), "\\_under\\_");
    assert_eq!(escape_md("(x)"), "\\(x\\)");
    assert_eq!(escape_md("!"), "\\!");
    assert_eq!(escape_md("|"), "\\|");
}

#[test]
fn escape_md_preserves_non_special() {
    assert_eq!(escape_md("abc 123 XYZ"), "abc 123 XYZ");
    assert_eq!(escape_md(""), "");
}

#[test]
fn escape_md_handles_unicode() {
    assert_eq!(escape_md("☁️ Selectel"), "☁️ Selectel");
    assert_eq!(escape_md("🇺🇿"), "🇺🇿");
    // Emoji followed by dot needs the dot escaped
    assert_eq!(escape_md("✅."), "✅\\.");
}
