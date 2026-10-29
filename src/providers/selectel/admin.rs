use std::collections::HashMap;
use std::sync::LazyLock;
use tokio::sync::Mutex;

use rand::Rng as _;
use sqlx::SqlitePool;
use teloxide::prelude::*;
use teloxide::types::{
    CallbackQuery, InlineKeyboardButton, InlineKeyboardMarkup, Message, ParseMode,
};

use crate::db;
use crate::provisioner;
use crate::provider::CloudProvider;
use crate::utils::escape_md;

use super::buttons as btn;
use super::client::SelectelClient;
use super::db as sel_db;
use crate::providers::common::admin_ui;

// ── State Machine ──

#[derive(Debug)]
enum SelAdminState {
    WaitingForAccountName,
    WaitingForUsername { name: String },
    WaitingForPassword { name: String, username: String },
    WaitingForAccountId { name: String, username: String, password: String },
    WaitingForProjectId { name: String, username: String, password: String, account_id: String },
    WaitingForDeactivateConfirm { account_id: i64, confirm_digit: u8 },
    /// Hard-delete confirm step.
    WaitingForRemoveConfirm { account_id: i64, confirm_digit: u8 },
}

#[derive(Debug)]
struct PendingSelAccount {
    name: String,
    username: String,
    password: String,
    account_id: String,
    project_id: String,
}

static SEL_STATE: LazyLock<Mutex<HashMap<i64, SelAdminState>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

static SEL_PENDING: LazyLock<Mutex<HashMap<i64, PendingSelAccount>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

use crate::providers::common::admin_ui::back_keyboard;

pub async fn clear_state(telegram_id: i64) {
    SEL_STATE.lock().await.remove(&telegram_id);
    SEL_PENDING.lock().await.remove(&telegram_id);
}

pub fn matches_callback(data: &str) -> bool {
    data == btn::CB_SELECTEL_ACCOUNTS
        || data == btn::CB_SELECTEL_ADD_ACCOUNT
        || data.starts_with(btn::CB_SELECTEL_DETAIL_PREFIX)
        || data.starts_with(btn::CB_SELECTEL_DELETE_PREFIX)
        || data.starts_with(btn::CB_SELECTEL_ACTIVATE_PREFIX)
        || data.starts_with(btn::CB_SELECTEL_DEACTIVATE_DB_PREFIX)
        || data.starts_with(btn::CB_SELECTEL_DEACTIVATE_PROVIDER_PREFIX)
        || data.starts_with(btn::CB_SELECTEL_REMOVE_PREFIX)
        || data.starts_with(btn::CB_SELECTEL_REGION_PREFIX)
        || data.starts_with(btn::CB_SELECTEL_CHANGE_REGION_PREFIX)
        || data.starts_with(btn::CB_SELECTEL_CHGRGN_PICK_PREFIX)
        || data.starts_with(btn::CB_SELECTEL_CHGRGN_CONFIRM_PREFIX)
        || data.starts_with(btn::CB_SELECTEL_MR_LIST_PREFIX)
        || data.starts_with(btn::CB_SELECTEL_MR_ADD_PREFIX)
        || data.starts_with(btn::CB_SELECTEL_MR_ADD_PICK_PREFIX)
        || data.starts_with(btn::CB_SELECTEL_MR_REMOVE_PREFIX)
}

pub async fn handle_callback(bot: &Bot, cq: &CallbackQuery, pool: &SqlitePool, data: &str, telegram_id: i64) {
    match data {
        btn::CB_SELECTEL_ACCOUNTS => {
            clear_state(telegram_id).await;
            handle_accounts_list(bot, cq, pool).await;
        }
        btn::CB_SELECTEL_ADD_ACCOUNT => {
            handle_add_start(bot, cq, telegram_id).await;
        }
        other => {
            if let Some(id_str) = other.strip_prefix(btn::CB_SELECTEL_DETAIL_PREFIX) {
                if let Ok(id) = id_str.parse::<i64>() {
                    handle_detail(bot, cq, pool, id).await;
                }
            } else if let Some(id_str) = other.strip_prefix(btn::CB_SELECTEL_DELETE_PREFIX) {
                if let Ok(id) = id_str.parse::<i64>() {
                    handle_delete_prompt(bot, cq, pool, id).await;
                }
            } else if let Some(id_str) = other.strip_prefix(btn::CB_SELECTEL_ACTIVATE_PREFIX) {
                if let Ok(id) = id_str.parse::<i64>() {
                    handle_account_activate(bot, cq, pool, id).await;
                }
            } else if let Some(id_str) = other.strip_prefix(btn::CB_SELECTEL_DEACTIVATE_DB_PREFIX) {
                if let Ok(id) = id_str.parse::<i64>() {
                    handle_deactivate_db_only(bot, cq, pool, id).await;
                }
            } else if let Some(id_str) = other.strip_prefix(btn::CB_SELECTEL_DEACTIVATE_PROVIDER_PREFIX) {
                if let Ok(id) = id_str.parse::<i64>() {
                    handle_deactivate_provider_start(bot, cq, telegram_id, id).await;
                }
            } else if let Some(id_str) = other.strip_prefix(btn::CB_SELECTEL_REMOVE_PREFIX) {
                if let Ok(id) = id_str.parse::<i64>() {
                    handle_account_remove_start(bot, cq, pool, telegram_id, id).await;
                }
            } else if let Some(region) = other.strip_prefix(btn::CB_SELECTEL_REGION_PREFIX) {
                handle_region_selected(bot, cq, pool, telegram_id, region).await;
            } else if let Some(rest) = other.strip_prefix(btn::CB_SELECTEL_CHGRGN_PICK_PREFIX) {
                if let Some((id_str, region)) = rest.split_once(':') {
                    if let Ok(id) = id_str.parse::<i64>() {
                        handle_change_region_pick(bot, cq, pool, id, region).await;
                    }
                }
            } else if let Some(rest) = other.strip_prefix(btn::CB_SELECTEL_CHGRGN_CONFIRM_PREFIX) {
                if let Some((id_str, region)) = rest.split_once(':') {
                    if let Ok(id) = id_str.parse::<i64>() {
                        handle_change_region_confirm(bot, cq, pool, id, region).await;
                    }
                }
            } else if let Some(id_str) = other.strip_prefix(btn::CB_SELECTEL_CHANGE_REGION_PREFIX) {
                if let Ok(id) = id_str.parse::<i64>() {
                    handle_change_region_prompt(bot, cq, pool, id).await;
                }
            } else if let Some(rest) = other.strip_prefix(btn::CB_SELECTEL_MR_ADD_PICK_PREFIX) {
                if let Some((id_str, region)) = rest.split_once(':') {
                    if let Ok(id) = id_str.parse::<i64>() {
                        handle_mr_add_apply(bot, cq, pool, id, region).await;
                    }
                }
            } else if let Some(rest) = other.strip_prefix(btn::CB_SELECTEL_MR_REMOVE_PREFIX) {
                if let Some((id_str, region)) = rest.split_once(':') {
                    if let Ok(id) = id_str.parse::<i64>() {
                        handle_mr_remove(bot, cq, pool, id, region).await;
                    }
                }
            } else if let Some(id_str) = other.strip_prefix(btn::CB_SELECTEL_MR_ADD_PREFIX) {
                if let Ok(id) = id_str.parse::<i64>() {
                    handle_mr_add_prompt(bot, cq, pool, id).await;
                }
            } else if let Some(id_str) = other.strip_prefix(btn::CB_SELECTEL_MR_LIST_PREFIX) {
                if let Ok(id) = id_str.parse::<i64>() {
                    handle_mr_list(bot, cq, pool, id).await;
                }
            }
        }
    }
}

pub async fn handle_text_input(bot: &Bot, msg: &Message, pool: &SqlitePool, telegram_id: i64) -> bool {
    let state = SEL_STATE.lock().await.remove(&telegram_id);
    let Some(state) = state else { return false };

    let text = msg.text().unwrap_or("").trim();

    match state {
        SelAdminState::WaitingForAccountName => {
            handle_name_input(bot, msg, telegram_id, text).await;
        }
        SelAdminState::WaitingForUsername { name } => {
            handle_username_input(bot, msg, telegram_id, text, name).await;
        }
        SelAdminState::WaitingForPassword { name, username } => {
            handle_password_input(bot, msg, telegram_id, text, name, username).await;
        }
        SelAdminState::WaitingForAccountId { name, username, password } => {
            handle_account_id_input(bot, msg, telegram_id, text, name, username, password).await;
        }
        SelAdminState::WaitingForProjectId { name, username, password, account_id } => {
            handle_project_id_input(bot, msg, telegram_id, text, name, username, password, account_id).await;
        }
        SelAdminState::WaitingForDeactivateConfirm { account_id, confirm_digit } => {
            handle_deactivate_provider_confirm(bot, msg, pool, text, account_id, confirm_digit).await;
        }
        SelAdminState::WaitingForRemoveConfirm { account_id, confirm_digit } => {
            handle_account_remove_confirm(bot, msg, pool, text, account_id, confirm_digit).await;
        }
    }

    true
}

pub async fn active_account_count(pool: &SqlitePool) -> usize {
    sel_db::list_active_accounts(pool).await.map(|a| a.len()).unwrap_or(0)
}

pub async fn render_server_status(pool: &SqlitePool) -> String {
    let accounts = sel_db::list_active_accounts(pool).await.unwrap_or_default();
    let mut text = String::new();

    for account in &accounts {
        text.push_str(&format!("*Selectel: {}* \\({}\\):\n", escape_md(&account.name), escape_md(&account.region)));
        match db::get_active_server_for_account(pool, "selectel", account.id).await {
            Ok(Some(srv)) => {
                let ip = srv.ipv4.as_deref().unwrap_or("pending");
                let deployed_icon = if srv.script_deployed { "✅" } else { "⏳" };
                let status_icon = match srv.status.as_str() {
                    "on" => "🟢",
                    "creating" | "installing" => "🟡",
                    _ => "🔴",
                };
                let mz = if srv.front_published { "✅" } else { "⏳" };

                text.push_str(&format!(
                    "  {} {} \\| IP: `{}` \\| ID: {}\n  {} Deploy \\| {} Published \\| Curl:{} SSH:{}\n",
                    status_icon, escape_md(&srv.status), ip, escape_md(&srv.provider_server_id),
                    deployed_icon, mz,
                    srv.consecutive_curl_fails, srv.consecutive_ssh_fails,
                ));

                // Warm floating-IP pool: one line per member with its last
                // per-IP check result (see selectel::hooks::manage_pool).
                let fips = db::list_floating_ip_pool(pool, srv.id).await.unwrap_or_default();
                for f in &fips {
                    let role_icon = if f.role == "active" { "🎯" } else { "♻️" };
                    let check_icon = match f.status.as_str() {
                        "good" => "🟢",
                        "blocked" => "🔴",
                        _ => "⚪",
                    };
                    text.push_str(&format!(
                        "    {} {} `{}` {} fails:{}\n",
                        role_icon, check_icon, f.floating_ip,
                        escape_md(&f.status), f.consecutive_fails,
                    ));
                }
                text.push('\n');
            }
            _ => {
                text.push_str(&admin_ui::no_server_line(pool, "selectel", account.id).await);
                text.push('\n');
            }
        }
    }

    text
}

// ── Accounts List ──

async fn handle_accounts_list(bot: &Bot, cq: &CallbackQuery, pool: &SqlitePool) {
    let Some(msg) = &cq.message else { return };

    let accounts = sel_db::list_all_accounts(pool).await.unwrap_or_default();

    if accounts.is_empty() {
        let _ = bot.edit_message_text(msg.chat().id, msg.id(),
            "☁️ *Selectel Accounts*\n\nNo Selectel accounts configured\\.")
            .parse_mode(ParseMode::MarkdownV2)
            .reply_markup(InlineKeyboardMarkup::new(vec![
                vec![InlineKeyboardButton::callback("➕ Add Selectel Account", btn::CB_SELECTEL_ADD_ACCOUNT)],
                vec![InlineKeyboardButton::callback("⬅️ Back", "adm_back")],
            ]))
            .await;
        return;
    }

    let mut text = String::from("☁️ *Selectel Accounts*\n\n");
    let mut buttons: Vec<Vec<InlineKeyboardButton>> = Vec::new();

    for a in &accounts {
        let active_icon = if a.is_active { "🟢" } else { "🔴" };

        let srv_status = if a.is_active {
            match db::get_active_server_for_account(pool, "selectel", a.id).await {
                Ok(Some(srv)) => {
                    let ip = srv.ipv4.as_deref().unwrap_or("pending");
                    if srv.script_deployed {
                        format!("🟢 {}", ip)
                    } else {
                        format!("🟡 {}", srv.status)
                    }
                }
                _ => "⚪ No server".to_string(),
            }
        } else {
            "inactive".to_string()
        };

        text.push_str(&format!(
            "{} *\\#{}* {} \\({}\\)\n   Server: {}\n\n",
            active_icon, a.id, escape_md(&a.name), escape_md(&a.region),
            escape_md(&srv_status)
        ));

        buttons.push(vec![
            InlineKeyboardButton::callback(
                format!("#{} {} ({})", a.id, a.name, a.region),
                format!("{}{}", btn::CB_SELECTEL_DETAIL_PREFIX, a.id),
            ),
        ]);
    }

    buttons.push(vec![InlineKeyboardButton::callback("➕ Add Selectel Account", btn::CB_SELECTEL_ADD_ACCOUNT)]);
    buttons.push(vec![InlineKeyboardButton::callback("⬅️ Back", "adm_back")]);

    let _ = bot.edit_message_text(msg.chat().id, msg.id(), text)
        .parse_mode(ParseMode::MarkdownV2)
        .reply_markup(InlineKeyboardMarkup::new(buttons))
        .await;
}

async fn handle_detail(bot: &Bot, cq: &CallbackQuery, pool: &SqlitePool, acct_id: i64) {
    let Some(msg) = &cq.message else { return };

    let account = match sel_db::get_account(pool, acct_id).await {
        Ok(Some(a)) => a,
        _ => {
            let _ = bot.edit_message_text(msg.chat().id, msg.id(), "Account not found\\.")
                .parse_mode(ParseMode::MarkdownV2)
                .reply_markup(back_keyboard())
                .await;
            return;
        }
    };

    let status = if account.is_active { "🟢 Active" } else { "🔴 Inactive" };

    let mut srv_lines = String::new();
    if account.is_active {
        match db::get_active_server_for_account(pool, "selectel", account.id).await {
            Ok(Some(srv)) => {
                let ip = srv.ipv4.as_deref().unwrap_or("pending");
                srv_lines.push_str(&format!("🖥 Server: {} \\- ID: {}\n", escape_md(&srv.status), escape_md(&srv.provider_server_id)));
                srv_lines.push_str(&format!("🌐 IP: `{}`\n", ip));
                if let Some(pass) = &srv.root_pass {
                    srv_lines.push_str(&format!("🔑 Pass: `{}`\n", escape_md(pass)));
                }
            }
            _ => {
                srv_lines.push_str("🖥 No server\n");
            }
        }
    } else {
        srv_lines.push_str("🖥 Inactive\n");
    }

    let text = format!(
        "☁️ *Selectel Account \\#{}*\n\n\
         📝 Name: {}\n\
         📍 Region: {}\n\
         👤 User: `{}`\n\
         🏢 Account: `{}`\n\
         📊 Status: {}\n\
         {}\
         📅 Created: {}",
        account.id,
        escape_md(&account.name),
        escape_md(&account.region),
        escape_md(&account.username),
        escape_md(&account.account_id),
        status,
        srv_lines,
        escape_md(&account.created_at),
    );

    let mut buttons = Vec::new();
    if account.is_active {
        // Link to global server-detail (Force Replace, Delete, etc.) when
        // there's a live server for this account.
        if let Ok(Some(srv)) = db::get_active_server_for_account(pool, "selectel", account.id).await {
            buttons.push(vec![InlineKeyboardButton::callback(
                "🖥 View server detail",
                format!("{}{}", crate::handlers::admin_buttons::CB_SERVER_DETAIL_PREFIX, srv.id),
            )]);
        }
        buttons.push(vec![
            InlineKeyboardButton::callback("🌍 Change Zone", format!("{}{}", btn::CB_SELECTEL_CHANGE_REGION_PREFIX, acct_id)),
            InlineKeyboardButton::callback("🌐 Failover Zones", format!("{}{}", btn::CB_SELECTEL_MR_LIST_PREFIX, acct_id)),
        ]);
        buttons.push(vec![
            InlineKeyboardButton::callback("📴 Deactivate", format!("{}{}", btn::CB_SELECTEL_DELETE_PREFIX, acct_id)),
        ]);
    } else {
        buttons.push(vec![
            InlineKeyboardButton::callback("✅ Activate", format!("{}{}", btn::CB_SELECTEL_ACTIVATE_PREFIX, acct_id)),
        ]);
        buttons.push(vec![
            InlineKeyboardButton::callback("🗑 Delete account", format!("{}{}", btn::CB_SELECTEL_REMOVE_PREFIX, acct_id)),
        ]);
    }
    buttons.push(vec![InlineKeyboardButton::callback("⬅️ Back", btn::CB_SELECTEL_ACCOUNTS)]);

    let _ = bot.edit_message_text(msg.chat().id, msg.id(), text)
        .parse_mode(ParseMode::MarkdownV2)
        .reply_markup(InlineKeyboardMarkup::new(buttons))
        .await;
}

// ── Change Region ──

/// Selectel availability zones. We store the AZ directly (not just the region)
/// because Selectel's own console exposes one AZ at a time and pinning to a
/// specific zone (e.g. ru-1a vs ru-1b) is sometimes the difference between a
/// blocked-in-TM IP pool and a clean one.
///
/// To add new zones: append rows here and ship a new binary. The base_url
/// derivation in client.rs strips the trailing zone letter to get the parent
/// region for endpoint URLs.
const SELECTEL_ZONES: &[(&str, &str)] = &[
    ("ru-1a", "St. Petersburg, RU (1a)"),
    ("ru-1b", "St. Petersburg, RU (1b)"),
    ("ru-2a", "Moscow, RU (2a)"),
    ("ru-3a", "Moscow, RU (3a)"),
    ("ru-7a", "St. Petersburg, RU (7a)"),
    ("ru-7b", "St. Petersburg, RU (7b)"),
    ("ru-7c", "St. Petersburg, RU (7c)"),
    ("ru-8a", "Moscow, RU (8a)"),
    ("ru-9a", "Moscow, RU (9a)"),
    ("gis-1a", "St. Petersburg GIS, RU (1a)"),
    ("uz-1a", "Tashkent, UZ (1a)"),
    ("uz-2a", "Tashkent, UZ (2a)"),
    ("kz-1a", "Almaty, KZ (1a)"),
    ("ke-1a", "Nairobi, KE (1a)"),
];

async fn handle_change_region_prompt(bot: &Bot, cq: &CallbackQuery, pool: &SqlitePool, acct_id: i64) {
    let Some(msg) = &cq.message else { return };

    let account = match sel_db::get_account(pool, acct_id).await {
        Ok(Some(a)) => a,
        _ => {
            let _ = bot.edit_message_text(msg.chat().id, msg.id(), "Account not found\\.")
                .parse_mode(ParseMode::MarkdownV2)
                .reply_markup(back_keyboard())
                .await;
            return;
        }
    };

    let mut buttons: Vec<Vec<InlineKeyboardButton>> = Vec::new();
    for (id, desc) in SELECTEL_ZONES {
        if *id == account.region.as_str() {
            continue; // skip current
        }
        buttons.push(vec![
            InlineKeyboardButton::callback(
                format!("{} — {}", id, desc),
                format!("{}{}:{}", btn::CB_SELECTEL_CHGRGN_PICK_PREFIX, acct_id, id),
            ),
        ]);
    }
    buttons.push(vec![InlineKeyboardButton::callback("❌ Cancel", format!("{}{}", btn::CB_SELECTEL_DETAIL_PREFIX, acct_id))]);

    let _ = bot.edit_message_text(msg.chat().id, msg.id(), format!(
        "🌍 *Change zone for Selectel account \\#{} \\({}\\)*\n\n\
         Current zone: `{}`\n\n\
         ⚠️ Selectel API endpoints are region\\-specific \\(parent of the zone\\)\\. Switching to a zone in a different parent region requires deleting any existing server in the old zone — the bot can't manage it after the switch\\.\n\n\
         Pick a new availability zone:",
        acct_id, escape_md(&account.name), escape_md(&account.region)
    ))
        .parse_mode(ParseMode::MarkdownV2)
        .reply_markup(InlineKeyboardMarkup::new(buttons))
        .await;
}

async fn handle_change_region_pick(bot: &Bot, cq: &CallbackQuery, pool: &SqlitePool, acct_id: i64, new_region: &str) {
    let Some(msg) = &cq.message else { return };

    if !SELECTEL_ZONES.iter().any(|(id, _)| *id == new_region) {
        let _ = bot.edit_message_text(msg.chat().id, msg.id(), "⚠️ Unknown region\\.")
            .parse_mode(ParseMode::MarkdownV2)
            .reply_markup(back_keyboard())
            .await;
        return;
    }

    let account = match sel_db::get_account(pool, acct_id).await {
        Ok(Some(a)) => a,
        _ => return,
    };

    let server_count = db::get_active_servers_for_account(pool, "selectel", acct_id)
        .await.map(|s| s.len()).unwrap_or(0);

    let _ = bot.edit_message_text(msg.chat().id, msg.id(), format!(
        "⚠️ *Change region for Selectel account \\#{}*\n\n\
         📍 `{}` → `{}`\n\
         🖥 Active servers: {}\n\n\
         Confirm? Existing server\\(s\\) in `{}` will be deleted, and a fresh server will be provisioned in `{}` on the next provisioner cycle\\.",
        acct_id,
        escape_md(&account.region),
        escape_md(new_region),
        server_count,
        escape_md(&account.region),
        escape_md(new_region),
    ))
        .parse_mode(ParseMode::MarkdownV2)
        .reply_markup(InlineKeyboardMarkup::new(vec![
            vec![InlineKeyboardButton::callback(
                format!("✅ Confirm — switch to {}", new_region),
                format!("{}{}:{}", btn::CB_SELECTEL_CHGRGN_CONFIRM_PREFIX, acct_id, new_region),
            )],
            vec![InlineKeyboardButton::callback(
                "❌ Cancel",
                format!("{}{}", btn::CB_SELECTEL_DETAIL_PREFIX, acct_id),
            )],
        ]))
        .await;
}

async fn handle_change_region_confirm(bot: &Bot, cq: &CallbackQuery, pool: &SqlitePool, acct_id: i64, new_region: &str) {
    let Some(msg) = &cq.message else { return };

    if !SELECTEL_ZONES.iter().any(|(id, _)| *id == new_region) {
        let _ = bot.edit_message_text(msg.chat().id, msg.id(), "⚠️ Unknown region\\.")
            .parse_mode(ParseMode::MarkdownV2)
            .reply_markup(back_keyboard())
            .await;
        return;
    }

    let account = match sel_db::get_account(pool, acct_id).await {
        Ok(Some(a)) => a,
        _ => {
            let _ = bot.edit_message_text(msg.chat().id, msg.id(), "⚠️ Account not found\\.")
                .parse_mode(ParseMode::MarkdownV2)
                .reply_markup(back_keyboard())
                .await;
            return;
        }
    };

    if account.region == new_region {
        let _ = bot.edit_message_text(msg.chat().id, msg.id(), "ℹ️ Region unchanged\\.")
            .parse_mode(ParseMode::MarkdownV2)
            .reply_markup(InlineKeyboardMarkup::new(vec![
                vec![InlineKeyboardButton::callback("⬅️ Back", format!("{}{}", btn::CB_SELECTEL_DETAIL_PREFIX, acct_id))],
            ]))
            .await;
        return;
    }

    // Delete servers in OLD region using the OLD region's client
    let old_client = SelectelClient::new(
        account.username.clone(), account.password.clone(),
        account.account_id.clone(), account.project_id.clone(),
        account.region.clone(),
    );
    let (deleted, failed) = provisioner::delete_account_servers(pool, "selectel", acct_id, &old_client).await;

    if failed > 0 {
        let _ = bot.edit_message_text(msg.chat().id, msg.id(), format!(
            "⚠️ {} server\\(s\\) failed to delete in `{}`\\. {} succeeded\\.\n\n\
             Region NOT changed\\. Fix the issue and try again\\.",
            failed, escape_md(&account.region), deleted,
        ))
            .parse_mode(ParseMode::MarkdownV2)
            .reply_markup(InlineKeyboardMarkup::new(vec![
                vec![InlineKeyboardButton::callback("⬅️ Back", format!("{}{}", btn::CB_SELECTEL_DETAIL_PREFIX, acct_id))],
            ]))
            .await;
        return;
    }

    // Apply region change in DB
    if let Err(e) = sel_db::update_region(pool, acct_id, new_region).await {
        let _ = bot.edit_message_text(msg.chat().id, msg.id(),
            format!("❌ DB update failed: {}", escape_md(&e.to_string())))
            .parse_mode(ParseMode::MarkdownV2)
            .reply_markup(back_keyboard())
            .await;
        return;
    }

    // Ensure the new active region is in the failover priority list.
    let next_prio = sel_db::next_priority(pool, acct_id).await.unwrap_or(1);
    if let Err(e) = sel_db::add_account_region(pool, acct_id, new_region, next_prio).await {
        log::warn!("Could not append region to failover list: {}", e);
    }

    // Re-publish the front IP so the OpenVPN-AS backend drops the deleted server(s)
    let mut footer = String::new();
    if deleted > 0 {
        match provisioner::publish_front_ip(pool).await {
            Ok(()) => footer.push_str("\n🔄 OpenVPN backend re-pointed\\."),
            Err(e) => footer.push_str(&format!("\n⚠️ OpenVPN publish failed: {}", escape_md(&e.to_string()))),
        }
    }

    let _ = bot.edit_message_text(msg.chat().id, msg.id(), format!(
        "✅ *Region changed*\n\n\
         📍 `{}` → `{}`\n\
         🗑 {} server\\(s\\) deleted\\.{}\
         \n\nProvisioner will respawn with new region within 30 seconds\\.",
        escape_md(&account.region),
        escape_md(new_region),
        deleted,
        footer,
    ))
        .parse_mode(ParseMode::MarkdownV2)
        .reply_markup(InlineKeyboardMarkup::new(vec![
            vec![InlineKeyboardButton::callback("☁️ View Selectel Accounts", btn::CB_SELECTEL_ACCOUNTS)],
            vec![InlineKeyboardButton::callback("⬅️ Back to Menu", "adm_back")],
        ]))
        .await;
}

// ── Failover Regions (multi-region priority list) ──

async fn handle_mr_list(bot: &Bot, cq: &CallbackQuery, pool: &SqlitePool, acct_id: i64) {
    let Some(msg) = &cq.message else { return };

    let account = match sel_db::get_account(pool, acct_id).await {
        Ok(Some(a)) => a,
        _ => {
            let _ = bot.edit_message_text(msg.chat().id, msg.id(), "Account not found\\.")
                .parse_mode(ParseMode::MarkdownV2)
                .reply_markup(back_keyboard())
                .await;
            return;
        }
    };

    let regions = sel_db::list_account_regions(pool, acct_id).await.unwrap_or_default();

    let mut text = format!(
        "🌐 *Failover zones for Selectel \\#{} \\({}\\)*\n\n\
         Active zone: `{}`\n\n\
         Priority reference for zone changes — the bot does *not* switch zones on its own \\(IP trouble is handled by the warm floating\\-IP pool on the current server\\)\\. To move: use 🌍 Change Zone; the provisioner respawns in the new zone\\. The active zone cannot be removed — switch off it first\\.\n\n",
        acct_id, escape_md(&account.name), escape_md(&account.region),
    );

    let mut buttons: Vec<Vec<InlineKeyboardButton>> = Vec::new();
    if regions.is_empty() {
        text.push_str("_No zones configured\\._\n");
    } else {
        text.push_str("Priority list:\n");
        for (region_id, prio) in &regions {
            let marker = if region_id == &account.region { "🟢 active" } else { "" };
            text.push_str(&format!("• `#{}` `{}` {}\n", prio, escape_md(region_id), marker));
            if region_id != &account.region {
                buttons.push(vec![InlineKeyboardButton::callback(
                    format!("✕ Remove {}", region_id),
                    format!("{}{}:{}", btn::CB_SELECTEL_MR_REMOVE_PREFIX, acct_id, region_id),
                )]);
            }
        }
    }

    buttons.push(vec![InlineKeyboardButton::callback(
        "➕ Add zone",
        format!("{}{}", btn::CB_SELECTEL_MR_ADD_PREFIX, acct_id),
    )]);
    buttons.push(vec![InlineKeyboardButton::callback(
        "⬅️ Back",
        format!("{}{}", btn::CB_SELECTEL_DETAIL_PREFIX, acct_id),
    )]);

    let _ = bot.edit_message_text(msg.chat().id, msg.id(), text)
        .parse_mode(ParseMode::MarkdownV2)
        .reply_markup(InlineKeyboardMarkup::new(buttons))
        .await;
}

async fn handle_mr_add_prompt(bot: &Bot, cq: &CallbackQuery, pool: &SqlitePool, acct_id: i64) {
    let Some(msg) = &cq.message else { return };

    let configured = sel_db::list_account_regions(pool, acct_id).await.unwrap_or_default();
    let configured_ids: std::collections::HashSet<&str> =
        configured.iter().map(|(r, _)| r.as_str()).collect();

    let mut buttons: Vec<Vec<InlineKeyboardButton>> = Vec::new();
    for (id, desc) in SELECTEL_ZONES {
        if configured_ids.contains(*id) {
            continue; // already in list
        }
        buttons.push(vec![InlineKeyboardButton::callback(
            format!("{} — {}", id, desc),
            format!("{}{}:{}", btn::CB_SELECTEL_MR_ADD_PICK_PREFIX, acct_id, id),
        )]);
    }
    buttons.push(vec![InlineKeyboardButton::callback(
        "⬅️ Back",
        format!("{}{}", btn::CB_SELECTEL_MR_LIST_PREFIX, acct_id),
    )]);

    let _ = bot.edit_message_text(msg.chat().id, msg.id(), format!(
        "➕ *Add failover zone for Selectel \\#{}*\n\nPick an availability zone to append to the priority list:",
        acct_id,
    ))
        .parse_mode(ParseMode::MarkdownV2)
        .reply_markup(InlineKeyboardMarkup::new(buttons))
        .await;
}

async fn handle_mr_add_apply(bot: &Bot, cq: &CallbackQuery, pool: &SqlitePool, acct_id: i64, region: &str) {
    if !SELECTEL_ZONES.iter().any(|(id, _)| *id == region) {
        if let Some(msg) = &cq.message {
            let _ = bot.edit_message_text(msg.chat().id, msg.id(), "⚠️ Unknown region\\.")
                .parse_mode(ParseMode::MarkdownV2)
                .reply_markup(back_keyboard())
                .await;
        }
        return;
    }

    let priority = match sel_db::next_priority(pool, acct_id).await {
        Ok(p) => p,
        Err(e) => {
            log::error!("next_priority({}): {}", acct_id, e);
            1
        }
    };
    if let Err(e) = sel_db::add_account_region(pool, acct_id, region, priority).await {
        if let Some(msg) = &cq.message {
            let _ = bot.edit_message_text(msg.chat().id, msg.id(),
                format!("❌ Failed to add region: {}", escape_md(&e.to_string())))
                .parse_mode(ParseMode::MarkdownV2)
                .reply_markup(back_keyboard())
                .await;
        }
        return;
    }

    handle_mr_list(bot, cq, pool, acct_id).await;
}

async fn handle_mr_remove(bot: &Bot, cq: &CallbackQuery, pool: &SqlitePool, acct_id: i64, region: &str) {
    // Refuse to remove the currently-active region
    if let Ok(Some(account)) = sel_db::get_account(pool, acct_id).await {
        if account.region == region {
            if let Some(msg) = &cq.message {
                let _ = bot.edit_message_text(msg.chat().id, msg.id(),
                    "⚠️ Cannot remove the active region\\. Use 🌍 Change Region first\\.")
                    .parse_mode(ParseMode::MarkdownV2)
                    .reply_markup(InlineKeyboardMarkup::new(vec![
                        vec![InlineKeyboardButton::callback("⬅️ Back",
                            format!("{}{}", btn::CB_SELECTEL_MR_LIST_PREFIX, acct_id))],
                    ]))
                    .await;
            }
            return;
        }
    }

    if let Err(e) = sel_db::remove_account_region(pool, acct_id, region).await {
        log::warn!("remove_account_region({},{}): {}", acct_id, region, e);
    }
    handle_mr_list(bot, cq, pool, acct_id).await;
}

// ── Activate Account ──

async fn handle_account_activate(bot: &Bot, cq: &CallbackQuery, pool: &SqlitePool, acct_id: i64) {
    let Some(msg) = &cq.message else { return };

    let account = match sel_db::get_account(pool, acct_id).await {
        Ok(Some(a)) => a,
        _ => {
            let _ = bot.edit_message_text(msg.chat().id, msg.id(), "⚠️ Account not found\\.")
                .parse_mode(ParseMode::MarkdownV2)
                .reply_markup(back_keyboard())
                .await;
            return;
        }
    };

    // Validate credentials by listing availability zones
    let client = SelectelClient::new(
        account.username, account.password,
        account.account_id, account.project_id, account.region,
    );
    if let Err(e) = client.list_regions().await {
        let _ = bot.edit_message_text(msg.chat().id, msg.id(), format!(
            "❌ Credential validation failed: {}\n\nAccount NOT activated\\.",
            escape_md(&e.to_string())
        ))
            .parse_mode(ParseMode::MarkdownV2)
            .reply_markup(InlineKeyboardMarkup::new(vec![
                vec![InlineKeyboardButton::callback("☁️ View Selectel Accounts", btn::CB_SELECTEL_ACCOUNTS)],
                vec![InlineKeyboardButton::callback("⬅️ Back to Menu", "adm_back")],
            ]))
            .await;
        return;
    }

    match sel_db::activate_account(pool, acct_id).await {
        Ok(true) => {
            let _ = bot.edit_message_text(msg.chat().id, msg.id(),
                format!("✅ Selectel account \\#{} activated\\.\nProvisioner will start within 30s\\.", acct_id))
                .parse_mode(ParseMode::MarkdownV2)
                .reply_markup(InlineKeyboardMarkup::new(vec![
                    vec![InlineKeyboardButton::callback("☁️ View Selectel Accounts", btn::CB_SELECTEL_ACCOUNTS)],
                    vec![InlineKeyboardButton::callback("⬅️ Back to Menu", "adm_back")],
                ]))
                .await;
        }
        Ok(false) => {
            let _ = bot.edit_message_text(msg.chat().id, msg.id(), "⚠️ Account not found\\.")
                .parse_mode(ParseMode::MarkdownV2)
                .reply_markup(back_keyboard())
                .await;
        }
        Err(e) => {
            let _ = bot.edit_message_text(msg.chat().id, msg.id(),
                format!("❌ Error: {}", escape_md(&e.to_string())))
                .parse_mode(ParseMode::MarkdownV2)
                .reply_markup(back_keyboard())
                .await;
        }
    }
}

// ── Add Account Flow ──

async fn handle_add_start(bot: &Bot, cq: &CallbackQuery, telegram_id: i64) {
    let Some(msg) = &cq.message else { return };

    SEL_STATE.lock().await.insert(telegram_id, SelAdminState::WaitingForAccountName);

    let _ = bot.edit_message_text(msg.chat().id, msg.id(),
        "➕ *Add Selectel Account*\n\n\
         Step 1/6: Enter a name for this account\\.\n\n\
         Example: `Tashkent`, `Moscow`\n\n\
         Send /start to cancel\\.")
        .parse_mode(ParseMode::MarkdownV2)
        .reply_markup(back_keyboard())
        .await;
}

async fn handle_name_input(bot: &Bot, msg: &Message, telegram_id: i64, text: &str) {
    if text.is_empty() || text.len() > 50 {
        SEL_STATE.lock().await.insert(telegram_id, SelAdminState::WaitingForAccountName);
        let _ = bot.send_message(msg.chat.id, "⚠️ Name must be 1\\-50 characters\\.")
            .parse_mode(ParseMode::MarkdownV2)
            .reply_markup(back_keyboard())
            .await;
        return;
    }

    SEL_STATE.lock().await.insert(telegram_id, SelAdminState::WaitingForUsername {
        name: text.to_string(),
    });

    let _ = bot.send_message(msg.chat.id, format!(
        "✅ Name: `{}`\n\n\
         Step 2/6: Enter the service user username\\.",
        escape_md(text)
    ))
        .parse_mode(ParseMode::MarkdownV2)
        .reply_markup(back_keyboard())
        .await;
}

async fn handle_username_input(bot: &Bot, msg: &Message, telegram_id: i64, text: &str, name: String) {
    if text.is_empty() {
        SEL_STATE.lock().await.insert(telegram_id, SelAdminState::WaitingForUsername { name });
        let _ = bot.send_message(msg.chat.id, "⚠️ Username cannot be empty\\.")
            .parse_mode(ParseMode::MarkdownV2)
            .reply_markup(back_keyboard())
            .await;
        return;
    }

    SEL_STATE.lock().await.insert(telegram_id, SelAdminState::WaitingForPassword {
        name,
        username: text.to_string(),
    });

    let _ = bot.send_message(msg.chat.id, format!(
        "✅ Username: `{}`\n\n\
         Step 3/6: Enter the service user password\\.",
        escape_md(text)
    ))
        .parse_mode(ParseMode::MarkdownV2)
        .reply_markup(back_keyboard())
        .await;
}

async fn handle_password_input(bot: &Bot, msg: &Message, telegram_id: i64, text: &str, name: String, username: String) {
    if text.is_empty() {
        SEL_STATE.lock().await.insert(telegram_id, SelAdminState::WaitingForPassword { name, username });
        let _ = bot.send_message(msg.chat.id, "⚠️ Password cannot be empty\\.")
            .parse_mode(ParseMode::MarkdownV2)
            .reply_markup(back_keyboard())
            .await;
        return;
    }

    SEL_STATE.lock().await.insert(telegram_id, SelAdminState::WaitingForAccountId {
        name,
        username,
        password: text.to_string(),
    });

    let _ = bot.send_message(msg.chat.id,
        "✅ Password saved\\.\n\n\
         Step 4/6: Enter the Selectel Account ID \\(numeric\\)\\.\n\n\
         Find it in the top\\-right corner of my\\.selectel\\.ru")
        .parse_mode(ParseMode::MarkdownV2)
        .reply_markup(back_keyboard())
        .await;
}

async fn handle_account_id_input(
    bot: &Bot, msg: &Message, telegram_id: i64, text: &str,
    name: String, username: String, password: String,
) {
    if text.is_empty() {
        SEL_STATE.lock().await.insert(telegram_id, SelAdminState::WaitingForAccountId { name, username, password });
        let _ = bot.send_message(msg.chat.id, "⚠️ Account ID cannot be empty\\.")
            .parse_mode(ParseMode::MarkdownV2)
            .reply_markup(back_keyboard())
            .await;
        return;
    }

    SEL_STATE.lock().await.insert(telegram_id, SelAdminState::WaitingForProjectId {
        name,
        username,
        password,
        account_id: text.to_string(),
    });

    let _ = bot.send_message(msg.chat.id, format!(
        "✅ Account ID: `{}`\n\n\
         Step 5/6: Enter the OpenStack Project ID \\(UUID\\)\\.\n\n\
         Find it in Cloud Platform → Project settings",
        escape_md(text)
    ))
        .parse_mode(ParseMode::MarkdownV2)
        .reply_markup(back_keyboard())
        .await;
}

async fn handle_project_id_input(
    bot: &Bot, msg: &Message, telegram_id: i64, text: &str,
    name: String, username: String, password: String, account_id: String,
) {
    if text.is_empty() {
        SEL_STATE.lock().await.insert(telegram_id, SelAdminState::WaitingForProjectId {
            name, username, password, account_id,
        });
        let _ = bot.send_message(msg.chat.id, "⚠️ Project ID cannot be empty\\.")
            .parse_mode(ParseMode::MarkdownV2)
            .reply_markup(back_keyboard())
            .await;
        return;
    }

    let project_id = text.to_string();

    // Validate credentials by authenticating
    let client = SelectelClient::new(
        username.clone(), password.clone(),
        account_id.clone(), project_id.clone(),
        "ru-1".to_string(), // Use ru-1 for validation (all regions share same Keystone)
    );
    if let Err(e) = client.list_regions().await {
        SEL_STATE.lock().await.insert(telegram_id, SelAdminState::WaitingForProjectId {
            name, username, password, account_id,
        });
        let _ = bot.send_message(msg.chat.id, format!(
            "❌ Failed to validate credentials: {}\n\nPlease check and try again\\.",
            escape_md(&e.to_string())
        ))
            .parse_mode(ParseMode::MarkdownV2)
            .reply_markup(back_keyboard())
            .await;
        return;
    }

    // Store pending account
    SEL_PENDING.lock().await.insert(telegram_id, PendingSelAccount {
        name,
        username,
        password,
        account_id,
        project_id,
    });

    // Show availability-zone selection (Selectel exposes one AZ at a time —
    // ru-1a, ru-1b, etc. — pin the account to a specific zone).
    let mut buttons: Vec<Vec<InlineKeyboardButton>> = Vec::new();
    for (id, desc) in SELECTEL_ZONES {
        buttons.push(vec![
            InlineKeyboardButton::callback(
                format!("{} — {}", id, desc),
                format!("{}{}", btn::CB_SELECTEL_REGION_PREFIX, id),
            ),
        ]);
    }
    buttons.push(vec![InlineKeyboardButton::callback("❌ Cancel", btn::CB_SELECTEL_ACCOUNTS)]);

    let _ = bot.send_message(msg.chat.id,
        "✅ Credentials validated\\!\n\n\
         Step 6/6: Select an availability zone:")
        .parse_mode(ParseMode::MarkdownV2)
        .reply_markup(InlineKeyboardMarkup::new(buttons))
        .await;
}

async fn handle_region_selected(bot: &Bot, cq: &CallbackQuery, pool: &SqlitePool, telegram_id: i64, region: &str) {
    let Some(msg) = &cq.message else { return };

    let pending = SEL_PENDING.lock().await.remove(&telegram_id);
    let Some(pending) = pending else {
        let _ = bot.edit_message_text(msg.chat().id, msg.id(), "⚠️ Session expired\\. Start again\\.")
            .parse_mode(ParseMode::MarkdownV2)
            .reply_markup(back_keyboard())
            .await;
        return;
    };

    let acct_id = match sel_db::add_account(
        pool, &pending.name, &pending.username, &pending.password,
        &pending.account_id, &pending.project_id, region,
    ).await {
        Ok(id) => id,
        Err(e) => {
            let _ = bot.edit_message_text(msg.chat().id, msg.id(),
                format!("❌ Failed to add account: {}", escape_md(&e.to_string())))
                .parse_mode(ParseMode::MarkdownV2)
                .reply_markup(back_keyboard())
                .await;
            return;
        }
    };

    // Seed the multi-region priority list with the current region as
    // priority 1 (admin reference for manual zone changes).
    if let Err(e) = sel_db::add_account_region(pool, acct_id, region, 1).await {
        log::warn!("Failed to seed selectel_account_regions for {}: {}", acct_id, e);
    }

    let _ = bot.edit_message_text(msg.chat().id, msg.id(), format!(
        "✅ *Selectel account added\\!*\n\n\
         🆔 ID: {}\n\
         📝 Name: {}\n\
         📍 Region: {}\n\n\
         Provisioner will start within 30 seconds\\.",
        acct_id, escape_md(&pending.name), escape_md(region)
    ))
        .parse_mode(ParseMode::MarkdownV2)
        .reply_markup(InlineKeyboardMarkup::new(vec![
            vec![InlineKeyboardButton::callback("☁️ View Selectel Accounts", btn::CB_SELECTEL_ACCOUNTS)],
            vec![InlineKeyboardButton::callback("⬅️ Back to Menu", "adm_back")],
        ]))
        .await;
}

// ── Deactivate Account ──

async fn handle_delete_prompt(bot: &Bot, cq: &CallbackQuery, pool: &SqlitePool, acct_id: i64) {
    let Some(msg) = &cq.message else { return };

    let account = match sel_db::get_account(pool, acct_id).await {
        Ok(Some(a)) => a,
        _ => {
            let _ = bot.edit_message_text(msg.chat().id, msg.id(), "Account not found\\.")
                .parse_mode(ParseMode::MarkdownV2)
                .reply_markup(back_keyboard())
                .await;
            return;
        }
    };

    let (text, kb) = admin_ui::deactivate_prompt(
        "Selectel", acct_id, &account.name,
        &format!("{}{}", btn::CB_SELECTEL_DEACTIVATE_DB_PREFIX, acct_id),
        &format!("{}{}", btn::CB_SELECTEL_DEACTIVATE_PROVIDER_PREFIX, acct_id),
        &format!("{}{}", btn::CB_SELECTEL_DETAIL_PREFIX, acct_id),
    );
    let _ = bot.edit_message_text(msg.chat().id, msg.id(), text)
        .parse_mode(ParseMode::MarkdownV2)
        .reply_markup(kb)
        .await;
}

async fn handle_deactivate_db_only(bot: &Bot, cq: &CallbackQuery, pool: &SqlitePool, acct_id: i64) {
    let Some(msg) = &cq.message else { return };

    match sel_db::deactivate_account(pool, acct_id).await {
        Ok(true) => {
            let mut text = format!("✅ Selectel account \\#{} deactivated\\.\nProvisioner will stop within 30s\\.\nServers kept\\.", acct_id);
            match provisioner::publish_front_ip(pool).await {
                Ok(()) => text.push_str("\n🔄 OpenVPN backend re-pointed\\."),
                Err(e) => {
                    log::error!("OpenVPN publish after Selectel account deactivation: {}", e);
                    text.push_str(&format!("\n⚠️ OpenVPN publish failed: {}", escape_md(&e.to_string())));
                }
            }

            let _ = bot.edit_message_text(msg.chat().id, msg.id(), text)
                .parse_mode(ParseMode::MarkdownV2)
                .reply_markup(InlineKeyboardMarkup::new(vec![
                    vec![InlineKeyboardButton::callback("☁️ View Selectel Accounts", btn::CB_SELECTEL_ACCOUNTS)],
                    vec![InlineKeyboardButton::callback("⬅️ Back to Menu", "adm_back")],
                ]))
                .await;
        }
        Ok(false) => {
            let _ = bot.edit_message_text(msg.chat().id, msg.id(), "⚠️ Account not found\\.")
                .parse_mode(ParseMode::MarkdownV2)
                .reply_markup(back_keyboard())
                .await;
        }
        Err(e) => {
            let _ = bot.edit_message_text(msg.chat().id, msg.id(),
                format!("❌ Error: {}", escape_md(&e.to_string())))
                .parse_mode(ParseMode::MarkdownV2)
                .reply_markup(back_keyboard())
                .await;
        }
    }
}

async fn handle_deactivate_provider_start(bot: &Bot, cq: &CallbackQuery, telegram_id: i64, acct_id: i64) {
    let Some(msg) = &cq.message else { return };

    let digit = rand::thread_rng().gen_range(0..=9u8);
    SEL_STATE.lock().await.insert(telegram_id, SelAdminState::WaitingForDeactivateConfirm {
        account_id: acct_id,
        confirm_digit: digit,
    });

    let (text, kb) = admin_ui::digit_confirm_prompt(
        "Selectel", acct_id, digit,
        &format!("{}{}", btn::CB_SELECTEL_DETAIL_PREFIX, acct_id),
    );
    let _ = bot.edit_message_text(msg.chat().id, msg.id(), text)
        .parse_mode(ParseMode::MarkdownV2)
        .reply_markup(kb)
        .await;
}

async fn handle_deactivate_provider_confirm(
    bot: &Bot, msg: &Message, pool: &SqlitePool, text: &str, account_id: i64, confirm_digit: u8,
) {
    if text.trim() != confirm_digit.to_string() {
        let _ = bot.send_message(msg.chat.id, "❌ Wrong code\\. Deactivation cancelled\\.")
            .parse_mode(ParseMode::MarkdownV2)
            .reply_markup(admin_ui::accounts_menu_keyboard(btn::CB_SELECTEL_ACCOUNTS))
            .await;
        return;
    }

    let account = match sel_db::get_account(pool, account_id).await {
        Ok(Some(a)) => a,
        _ => {
            let _ = bot.send_message(msg.chat.id, "⚠️ Account not found\\.")
                .parse_mode(ParseMode::MarkdownV2)
                .reply_markup(back_keyboard())
                .await;
            return;
        }
    };

    let client = SelectelClient::new(
        account.username, account.password,
        account.account_id, account.project_id, account.region,
    );
    let (deleted, failed) = provisioner::delete_account_servers(pool, "selectel", account_id, &client).await;

    if failed > 0 {
        let _ = bot.send_message(msg.chat.id, admin_ui::partial_delete_failure_message(deleted, failed))
            .parse_mode(ParseMode::MarkdownV2)
            .reply_markup(admin_ui::accounts_menu_keyboard(btn::CB_SELECTEL_ACCOUNTS))
            .await;
        return;
    }

    sel_db::deactivate_account(pool, account_id).await.ok();

    let mut result = format!("✅ Selectel account \\#{} deactivated\\.", account_id);
    if deleted > 0 {
        result.push_str(&format!("\n🗑 {} server\\(s\\) deleted\\.", deleted));
    }
    match provisioner::publish_front_ip(pool).await {
        Ok(()) => result.push_str("\n🔄 OpenVPN backend re-pointed\\."),
        Err(e) => {
            log::error!("OpenVPN publish after Selectel deactivation+delete: {}", e);
            result.push_str(&format!("\n⚠️ OpenVPN publish failed: {}", escape_md(&e.to_string())));
        }
    }

    let _ = bot.send_message(msg.chat.id, result)
        .parse_mode(ParseMode::MarkdownV2)
        .reply_markup(InlineKeyboardMarkup::new(vec![
            vec![InlineKeyboardButton::callback("☁️ View Selectel Accounts", btn::CB_SELECTEL_ACCOUNTS)],
            vec![InlineKeyboardButton::callback("⬅️ Back to Menu", "adm_back")],
        ]))
        .await;
}

// ── Delete account (hard-remove DB row) ──

async fn handle_account_remove_start(
    bot: &Bot, cq: &CallbackQuery, pool: &SqlitePool, telegram_id: i64, acct_id: i64,
) {
    let Some(msg) = &cq.message else { return };

    match crate::db::count_active_servers(pool, "selectel", acct_id).await {
        Ok(n) if n > 0 => {
            let _ = bot.edit_message_text(msg.chat().id, msg.id(),
                admin_ui::delete_blocked_by_servers_message("Selectel", acct_id, n))
                .parse_mode(ParseMode::MarkdownV2)
                .reply_markup(InlineKeyboardMarkup::new(vec![
                    vec![InlineKeyboardButton::callback("🖥 Server status", "adm_server")],
                    vec![InlineKeyboardButton::callback("⬅️ Back", format!("{}{}", btn::CB_SELECTEL_DETAIL_PREFIX, acct_id))],
                ]))
                .await;
            return;
        }
        Ok(_) => {}
        Err(e) => {
            log::error!("count_active_servers(selectel, {}): {}", acct_id, e);
            let _ = bot.edit_message_text(msg.chat().id, msg.id(),
                format!("❌ DB error: {}", escape_md(&e.to_string())))
                .parse_mode(ParseMode::MarkdownV2)
                .reply_markup(back_keyboard())
                .await;
            return;
        }
    }

    if let Ok(Some(account)) = sel_db::get_account(pool, acct_id).await {
        if account.is_active {
            let _ = bot.edit_message_text(msg.chat().id, msg.id(),
                "⚠️ Account is still active\\. Deactivate it first, then delete\\.")
                .parse_mode(ParseMode::MarkdownV2)
                .reply_markup(back_keyboard())
                .await;
            return;
        }
    }

    let digit = rand::thread_rng().gen_range(0..=9u8);
    SEL_STATE.lock().await.insert(telegram_id, SelAdminState::WaitingForRemoveConfirm {
        account_id: acct_id,
        confirm_digit: digit,
    });

    let (text, kb) = admin_ui::delete_confirm_prompt(
        "Selectel", acct_id, digit,
        &format!("{}{}", btn::CB_SELECTEL_DETAIL_PREFIX, acct_id),
    );
    let _ = bot.edit_message_text(msg.chat().id, msg.id(), text)
        .parse_mode(ParseMode::MarkdownV2)
        .reply_markup(kb)
        .await;
}

async fn handle_account_remove_confirm(
    bot: &Bot, msg: &Message, pool: &SqlitePool, text: &str, account_id: i64, confirm_digit: u8,
) {
    if text.trim() != confirm_digit.to_string() {
        let _ = bot.send_message(msg.chat.id, "❌ Wrong code\\. Delete cancelled\\.")
            .parse_mode(ParseMode::MarkdownV2)
            .reply_markup(admin_ui::accounts_menu_keyboard(btn::CB_SELECTEL_ACCOUNTS))
            .await;
        return;
    }

    match sel_db::delete_account(pool, account_id).await {
        Ok(true) => {
            let _ = bot.send_message(msg.chat.id,
                format!("🗑 Selectel account \\#{} deleted\\.", account_id))
                .parse_mode(ParseMode::MarkdownV2)
                .reply_markup(admin_ui::accounts_menu_keyboard(btn::CB_SELECTEL_ACCOUNTS))
                .await;
        }
        Ok(false) => {
            let _ = bot.send_message(msg.chat.id,
                format!("⚠️ Account \\#{} not deleted \\(already gone or still active\\)\\.", account_id))
                .parse_mode(ParseMode::MarkdownV2)
                .reply_markup(admin_ui::accounts_menu_keyboard(btn::CB_SELECTEL_ACCOUNTS))
                .await;
        }
        Err(e) => {
            let _ = bot.send_message(msg.chat.id,
                format!("❌ DB error: {}", escape_md(&e.to_string())))
                .parse_mode(ParseMode::MarkdownV2)
                .reply_markup(back_keyboard())
                .await;
        }
    }
}
