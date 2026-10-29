//! Shared admin UI helpers used by all provider admin modules.
//!
//! Every provider (TimeWeb, Beget, Vultr, Selectel) has the same deactivate flow:
//! list → detail → deactivate prompt (DB-only vs Provider+digit) → digit confirm.
//! These helpers build the standard messages and keyboards so each provider
//! just passes its name and callback prefixes.
//!
//! Keep this layer thin — it's just formatting. Provider-specific logic
//! (credentials validation, region picker, etc.) stays in each provider's admin.rs.

use sqlx::SqlitePool;
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup};

use crate::diagnose;
use crate::utils::escape_md;

/// Format the per-account "no server" line shown on every provider's status
/// screen. Calls the diagnose helper so the admin sees *why* there's no server
/// (cooldown / off-window / no destinations / last error) instead of a bare
/// "⚪ No server".
///
/// `account_id` is the provisioner-key id (region-row id for multi-region
/// providers, parent account id for single-region, manual_servers.id for manual).
/// Returns a formatted line ready to splice into the status block — already
/// indented with two spaces and terminated with a newline.
pub async fn no_server_line(
    pool: &SqlitePool,
    provider_type: &str,
    account_id: i64,
) -> String {
    match diagnose::diagnose_no_server(pool, provider_type, account_id).await {
        Some(reason) => format!("  ⚪ No server — {}\n", escape_md(&reason)),
        None => "  ⚪ No server\n".to_string(),
    }
}

/// Standard "Back" row used when there's no better context.
pub fn back_row(cb_back: &'static str) -> Vec<InlineKeyboardButton> {
    vec![InlineKeyboardButton::callback("⬅️ Back", cb_back)]
}

/// Standalone "⬅️ Back" keyboard returning to the main admin menu (`adm_back`).
/// Every provider admin module used to define this verbatim; shared here.
pub fn back_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![back_row("adm_back")])
}

/// Row linking to the provider's accounts list + Back to main menu.
pub fn accounts_and_back(accounts_cb: &'static str) -> Vec<Vec<InlineKeyboardButton>> {
    vec![
        vec![InlineKeyboardButton::callback("📂 Accounts", accounts_cb)],
        back_row("adm_back"),
    ]
}

/// The message + keyboard shown when admin clicks "Deactivate" on an account.
/// Gives two options: DB-only (keep provider servers) or Provider (delete servers too).
pub fn deactivate_prompt(
    provider_display: &str,
    account_id: i64,
    account_name: &str,
    cb_deactivate_db: &str,
    cb_deactivate_provider: &str,
    cb_cancel: &str,
) -> (String, InlineKeyboardMarkup) {
    let text = format!(
        "⚠️ *Deactivate {} account \\#{} \\({}\\)?*\n\n\
         📴 *Deactivate only* — Stop provisioner, keep servers\\.\n\
         ☁️ *Deactivate \\+ Delete servers* — Also delete servers \\(irreversible\\)\\.",
        escape_md(provider_display), account_id, escape_md(account_name)
    );
    let kb = InlineKeyboardMarkup::new(vec![
        vec![InlineKeyboardButton::callback("📴 Deactivate only", cb_deactivate_db)],
        vec![InlineKeyboardButton::callback("☁️ Deactivate + Delete servers", cb_deactivate_provider)],
        vec![InlineKeyboardButton::callback("❌ Cancel", cb_cancel)],
    ]);
    (text, kb)
}

/// Message + keyboard for the digit-confirm step before deleting servers.
pub fn digit_confirm_prompt(
    provider_display: &str,
    account_id: i64,
    confirm_digit: u8,
    cb_cancel: &str,
) -> (String, InlineKeyboardMarkup) {
    let text = format!(
        "⚠️ *This will deactivate {} account \\#{} AND delete all its servers\\!*\n\n\
         This action is *irreversible*\\.\n\n\
         Type `{}` to confirm\\.",
        escape_md(provider_display), account_id, confirm_digit
    );
    let kb = InlineKeyboardMarkup::new(vec![
        vec![InlineKeyboardButton::callback("❌ Cancel", cb_cancel)],
    ]);
    (text, kb)
}

/// Keyboard with links to accounts list + main menu. Used for both
/// post-deactivation success and wrong-code/cancel messages.
pub fn accounts_menu_keyboard(accounts_cb: &'static str) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(accounts_and_back(accounts_cb))
}

/// Prompt for the *Delete account* confirm step. Distinct from the
/// deactivate confirm — at this point the account is already inactive and
/// its servers are already cleaned up; this just removes the DB row.
/// Server cleanup is NOT done here (the admin had to do it beforehand via
/// Deactivate + Delete servers or per-server delete).
pub fn delete_confirm_prompt(
    provider_display: &str,
    account_id: i64,
    confirm_digit: u8,
    cb_cancel: &str,
) -> (String, InlineKeyboardMarkup) {
    let text = format!(
        "🗑 *Delete {} account \\#{} from the bot?*\n\n\
         This removes the row from the DB only \\(servers already cleaned up\\)\\.\n\
         No provider API or backend calls — *non\\-destructive*\\.\n\n\
         Type `{}` to confirm\\.",
        escape_md(provider_display), account_id, confirm_digit
    );
    let kb = InlineKeyboardMarkup::new(vec![
        vec![InlineKeyboardButton::callback("❌ Cancel", cb_cancel)],
    ]);
    (text, kb)
}

/// Message shown when the admin tries to delete an account that still has
/// active `servers` rows. Tells them what to clean up first.
pub fn delete_blocked_by_servers_message(
    provider_display: &str,
    account_id: i64,
    server_count: i64,
) -> String {
    format!(
        "⚠️ Can't delete {} account \\#{} \\— it still has {} active server\\(s\\)\\.\n\n\
         Clean them up first via *Server status \\> tap the server \\> 🗑 Delete*, \
         then come back\\.",
        provider_display, account_id, server_count,
    )
}

/// Message when deactivation + provider deletion partially fails.
pub fn partial_delete_failure_message(deleted: u32, failed: u32) -> String {
    format!(
        "⚠️ {} server\\(s\\) failed to delete\\.\n\
         {} server\\(s\\) deleted successfully\\.\n\n\
         Account NOT deactivated\\. Fix the issue and try again\\.",
        failed, deleted
    )
}
