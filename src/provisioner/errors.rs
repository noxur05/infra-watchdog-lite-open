//! Provisioner-cycle error categorization + the "🔄 Retry now" inline keyboard.
//!
//! `categorize_provisioning_error` maps a freeform error string to a stable
//! category id + human label. The `AccountProvisioner` uses the category to
//! dedupe admin notifications (same category re-notifies only after 24h);
//! the label is the user-facing text in the alert.

use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup};

pub(super) fn retry_keyboard(provider_type: &str, account_id: i64) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![
        vec![InlineKeyboardButton::callback(
            "🔄 Retry now",
            format!("adm_retry:{}:{}", provider_type, account_id),
        )],
    ])
}

/// Map a `cycle error` string to a stable category id + human-readable label.
/// Returns None for errors we explicitly don't want to spam the admin about
/// (auth errors are notified separately on the Err(is_auth) branch; generic
/// errors aren't actionable enough to ping for).
pub(super) fn categorize_provisioning_error(msg: &str) -> Option<(&'static str, &'static str)> {
    // Account balance / suspension — TimeWeb's `no_paid` / `blocked` server
    // statuses, or any create error that mentions an unpaid/blocked account.
    // Admin-actionable: top up the balance, then "🔄 Retry now".
    if msg.contains("account_blocked") || msg.contains("no_paid")
        || msg.contains("not_paid") || msg.contains("insufficient")
    {
        return Some(("account_blocked",
            "provider account is out of balance or blocked — top up, then tap Retry now"));
    }
    // Selectel-style quota messages
    if msg.contains("Quota exceeded for ram") {
        return Some(("quota_ram",
            "RAM quota exhausted on the provider account (project may have 0 RAM allocation)"));
    }
    if msg.contains("Quota exceeded for local_gb") || msg.contains("Quota exceeded for disk") {
        return Some(("quota_disk",
            "disk quota exhausted on the provider account (project may have 0 disk allocation)"));
    }
    if msg.contains("Quota exceeded") {
        return Some(("quota", "provider quota exhausted"));
    }
    // TimeWeb "no inventory in selected region"
    if msg.contains("no_free_node") || msg.contains("No free node") {
        return Some(("no_free_node", "no inventory available in the selected region"));
    }
    // Selectel "no available resources" — capacity in this AZ
    if msg.contains("no available resources") {
        return Some(("no_capacity", "no capacity in this availability zone"));
    }
    // 4xx/5xx (skip auth, which has its own dedicated notification)
    if msg.contains("auth error:") {
        return None;
    }
    if let Some(idx) = msg.find("API error: ") {
        let rest = &msg[idx + "API error: ".len()..];
        if rest.starts_with('5') {
            return Some(("server_5xx", "provider returned a 5xx server error"));
        }
        if rest.starts_with("404") {
            return Some(("not_found_404",
                "provider returned 404 (account warm-up or wrong endpoint)"));
        }
    }
    if msg.contains("network error") || msg.contains("Network") {
        return Some(("network", "network error reaching the provider"));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_blocked_is_categorized() {
        // The status-arm signals it via the server status string; a create
        // error from a drained account may also carry it. Both should map to
        // the same admin-actionable category.
        for s in ["server status no_paid", "TimeWeb not_paid", "account_blocked",
                  "API error: 400 insufficient funds"] {
            assert_eq!(categorize_provisioning_error(s).map(|c| c.0), Some("account_blocked"),
                "expected account_blocked for {:?}", s);
        }
    }

    #[test]
    fn unrelated_errors_unaffected() {
        // A 5xx must not get swallowed by the new balance branch.
        assert_eq!(categorize_provisioning_error("API error: 502 bad gateway").map(|c| c.0),
            Some("server_5xx"));
        assert_eq!(categorize_provisioning_error("totally unknown thing"), None);
    }

    #[test]
    fn categories_map_to_expected_ids() {
        let cases = [
            ("Quota exceeded for ram: project has 0", "quota_ram"),
            ("Quota exceeded for local_gb", "quota_disk"),
            ("Quota exceeded for disk", "quota_disk"),
            ("Quota exceeded for cores", "quota"),
            ("no_free_node in region ru-1", "no_free_node"),
            ("No free node", "no_free_node"),
            ("no available resources", "no_capacity"),
            ("API error: 500 internal", "server_5xx"),
            ("API error: 503 unavailable", "server_5xx"),
            ("API error: 404 not found", "not_found_404"),
            ("network error: timed out", "network"),
            ("Network unreachable", "network"),
        ];
        for (msg, expected) in cases {
            assert_eq!(categorize_provisioning_error(msg).map(|c| c.0), Some(expected),
                "wrong category for {:?}", msg);
        }
    }

    #[test]
    fn specific_quota_beats_generic_quota() {
        // The RAM/disk arms must be checked before the catch-all "Quota
        // exceeded" arm, or every quota error collapses into the generic id.
        assert_eq!(categorize_provisioning_error("Quota exceeded for ram").map(|c| c.0),
            Some("quota_ram"));
        assert_eq!(categorize_provisioning_error("Quota exceeded for local_gb").map(|c| c.0),
            Some("quota_disk"));
    }

    #[test]
    fn auth_errors_are_not_notified_here() {
        // Auth has a dedicated notification path; this categorizer stays out of it.
        assert_eq!(categorize_provisioning_error("auth error: 401 unauthorized"), None);
    }

    #[test]
    fn every_category_has_a_non_empty_label() {
        for msg in ["account_blocked", "Quota exceeded for ram", "no_free_node",
                    "no available resources", "API error: 500", "network error"] {
            let (_id, label) = categorize_provisioning_error(msg).unwrap();
            assert!(!label.is_empty(), "empty label for {:?}", msg);
        }
    }
}

