//! Best-effort "why isn't this account producing a server right now?" helper.
//!
//! Surfaces in the admin UI next to "⚪ No server" / stuck states. Reads only —
//! never writes state. Each check returns at most one human-readable reason
//! line; the function returns the highest-priority active reason (Gcore window
//! beats cooldown beats no-destinations beats last-error).
//!
//! All reasons are short enough to fit on one line in a Telegram detail page.

use sqlx::SqlitePool;

use crate::db;
use crate::time_utils;

/// Map a stored error category to a short user-facing label.
fn error_label(category: &str) -> &'static str {
    match category {
        "quota_ram" => "RAM quota exhausted on provider",
        "quota_disk" => "disk quota exhausted on provider",
        "quota" => "provider quota exhausted",
        "no_free_node" => "no inventory in selected region",
        "no_capacity" => "no capacity in this availability zone",
        "server_5xx" => "provider 5xx error",
        "not_found_404" => "provider returned 404",
        "network" => "network error reaching provider",
        "ip_daily_limit" => "IP daily limit exhausted on provider",
        _ => "previous cycle failed",
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max - 1).collect();
        format!("{}…", head)
    }
}

/// One-line reason explaining why the account/region currently has no server.
/// Returns `None` if no obvious blocker is detected — the cycle may simply not
/// have run yet (provisioner manager ticks every 30s).
pub async fn diagnose_no_server(
    pool: &SqlitePool,
    provider_type: &str,
    account_id: i64,
) -> Option<String> {
    // Last cycle's error (persisted from `handle_cycle_result`).
    if let Ok(Some(state)) = db::get_account_state(pool, provider_type, account_id).await {
        if let (Some(category), Some(at)) = (
            state.last_error_category.as_deref(),
            state.last_error_at.as_deref(),
        ) {
            let mins = time_utils::minutes_since(at);
            let label = error_label(category);
            let when = if mins < 60 {
                format!("{}m ago", mins)
            } else {
                format!("{}h{:02}m ago", mins / 60, mins % 60)
            };
            let mut out = format!("Last cycle failed {} — {}", when, label);
            if let Some(msg) = state.last_error_message.as_deref() {
                out.push_str(&format!(" ({})", truncate(msg, 100)));
            }
            return Some(out);
        }
    }

    None
}
