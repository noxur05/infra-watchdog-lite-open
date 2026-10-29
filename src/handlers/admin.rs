use std::collections::HashMap;
use std::sync::LazyLock;
use tokio::sync::Mutex;

use sqlx::SqlitePool;
use teloxide::prelude::*;
use teloxide::types::{
    CallbackQuery, InlineKeyboardButton, InlineKeyboardMarkup, Message, ParseMode,
};

use rand::Rng as _;

use crate::config::get_config;
use crate::db;
use crate::provisioner;
use crate::providers;
use crate::utils::escape_md;

use super::admin_buttons as btn;

// ── Admin State Machine ──

#[derive(Debug)]
enum AdminState {
    WaitingForServerDeleteConfirm { server_id: i64, confirm_digit: u8 },
    WaitingForServerForceReplaceConfirm { server_id: i64, confirm_digit: u8 },
    WaitingForTestUrl,
    /// Admin pasted a free-form blob of emails to add to the BCC list.
    WaitingForBulkEmails,
    /// Admin is sending the id of an email to delete.
    WaitingForEmailIdToDelete,
    /// Admin is sending a new value for one OVPN-AS backend field
    /// (🖥 Main Server screen, field-by-field editing).
    WaitingForMainSrvValue { field: MainSrvField },
    /// Admin is sending a new mail-template subject.
    WaitingForMailTplSubject,
    /// Admin is sending a new mail-template body.
    WaitingForMailTplBody,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MainSrvField {
    Ip,
    Port,
    User,
    Pass,
}

static ADMIN_STATE: LazyLock<Mutex<HashMap<i64, AdminState>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn is_admin_id(telegram_id: i64) -> bool {
    let cfg = get_config();
    let ok = cfg.is_admin(telegram_id);
    log::debug!("Admin check: user={} allowed={} (admins={:?})", telegram_id, ok, cfg.admin_telegram_ids);
    ok
}

fn is_admin(msg: &Message) -> bool {
    msg.from.as_ref()
        .map(|u| is_admin_id(u.id.0 as i64))
        .unwrap_or(false)
}

async fn clear_state(telegram_id: i64) {
    ADMIN_STATE.lock().await.remove(&telegram_id);
    for module in providers::all() {
        module.clear_state(telegram_id).await;
    }
}

// ── Keyboards ──

fn admin_menu_keyboard() -> InlineKeyboardMarkup {
    let mut rows = vec![
        vec![
            InlineKeyboardButton::callback("🖥 Server status", btn::CB_ADMIN_SERVER),
        ],
    ];

    // Provider buttons, two per row.
    let provider_buttons: Vec<InlineKeyboardButton> = providers::all()
        .iter()
        .map(|m| m.menu_button())
        .collect();
    for chunk in provider_buttons.chunks(2) {
        rows.push(chunk.to_vec());
    }

    // Settings row. Destinations + Node Cert are gone: the bot blanket-DNATs
    // all ports to the fixed OpenVPN-AS backend, so there's nothing per-
    // destination to configure.
    rows.push(vec![
        InlineKeyboardButton::callback("🌐 Test URLs", btn::CB_ADMIN_TEST_URLS),
    ]);
    rows.push(vec![
        InlineKeyboardButton::callback("📧 Emails", btn::CB_ADMIN_EMAILS),
    ]);
    rows.push(vec![
        InlineKeyboardButton::callback("🖥 Main Server", btn::CB_ADMIN_MAINSRV),
    ]);

    InlineKeyboardMarkup::new(rows)
}

fn back_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![vec![
        InlineKeyboardButton::callback("⬅️ Back", btn::CB_ADMIN_BACK),
    ]])
}

const MAX_SERVER_BUTTONS: usize = 20;

fn server_list_keyboard(servers: &[db::Server]) -> InlineKeyboardMarkup {
    let mut buttons: Vec<Vec<InlineKeyboardButton>> = Vec::new();
    let shown = servers.len().min(MAX_SERVER_BUTTONS);
    for srv in &servers[..shown] {
        let ip = srv.ipv4.as_deref().unwrap_or("pending");
        let status_icon = match srv.status.as_str() {
            "on" => "🟢",
            "creating" | "installing" => "🟡",
            _ => "🔴",
        };
        // Row = [detail button] [🔁 force-replace] [🗑 delete]. The two icon
        // buttons reuse the existing detail-page callbacks (both land on a
        // confirm step), so admin can recycle/remove a server without drilling
        // into the detail page first.
        // Label trimmed to status-circle + #id + IP so the IP stays visible in
        // the third of the row width Telegram leaves after the two icon buttons.
        // Provider is dropped (rows are grouped by provider in the text above)
        // and the status word is dropped (the circle already conveys it).
        let mut row = vec![
            InlineKeyboardButton::callback(
                format!("{} #{} {}", status_icon, srv.id, ip),
                format!("{}{}", btn::CB_SERVER_DETAIL_PREFIX, srv.id),
            ),
        ];
        // Force Replace is only meaningful for bot-managed providers (not
        // 'manual', whose lifecycle is outside AccountProvisioner) and not for
        // an already-deleted row — mirrors the detail-page gating.
        if srv.deleted_at.is_none() && srv.provider_type != "manual" {
            row.push(InlineKeyboardButton::callback(
                "🔁",
                format!("{}{}", btn::CB_SERVER_FORCE_REPLACE_PREFIX, srv.id),
            ));
        }
        row.push(InlineKeyboardButton::callback(
            "🗑",
            format!("{}{}", btn::CB_SERVER_DELETE_PREFIX, srv.id),
        ));
        buttons.push(row);
    }
    if servers.len() > MAX_SERVER_BUTTONS {
        buttons.push(vec![InlineKeyboardButton::callback(
            format!("... and {} more", servers.len() - MAX_SERVER_BUTTONS),
            btn::CB_SERVER_REFRESH,
        )]);
    }
    buttons.push(vec![InlineKeyboardButton::callback("🔄 Refresh", btn::CB_SERVER_REFRESH)]);
    buttons.push(vec![InlineKeyboardButton::callback("⬅️ Back", btn::CB_ADMIN_BACK)]);
    InlineKeyboardMarkup::new(buttons)
}

// ── /admin Command Handler ──

pub async fn handle_admin_command(bot: &Bot, msg: &Message, pool: &SqlitePool) -> ResponseResult<()> {
    if !is_admin(msg) {
        return Ok(());
    }

    let telegram_id = msg.from.as_ref().map(|u| u.id.0 as i64).unwrap_or(0);
    clear_state(telegram_id).await;

    let text = build_admin_panel_text(pool).await;

    bot.send_message(msg.chat.id, text)
        .parse_mode(ParseMode::MarkdownV2)
        .reply_markup(admin_menu_keyboard())
        .await?;

    Ok(())
}

async fn build_admin_panel_text(pool: &SqlitePool) -> String {
    let server_count = db::get_all_active_servers(pool).await.map(|s| s.len()).unwrap_or(0);

    // Show only providers that have at least one active account — keeps the
    // panel from being a "0 0 0 0 0 0" wall when only some providers are in use.
    let mut acct_parts = Vec::new();
    for module in providers::all() {
        let count = module.active_account_count(pool).await;
        if count > 0 {
            acct_parts.push(format!("{} {}", count, module.display_name()));
        }
    }
    let acct_summary = if acct_parts.is_empty() {
        "_none_".to_string()
    } else {
        acct_parts.join(" · ")
    };

    format!(
        "🔧 *Admin Panel*\n\n\
         🖥 *{}* server{}\n\
         🔑 Accounts: {}",
        server_count, if server_count == 1 { "" } else { "s" },
        acct_summary,
    )
}

// ── Text Input Handler (for multi-step flows) ──

pub async fn handle_admin_text_input(
    bot: &Bot, msg: &Message, pool: &SqlitePool,
) -> bool {
    let Some(user) = msg.from.as_ref() else { return false };
    let telegram_id = user.id.0 as i64;
    if !is_admin_id(telegram_id) {
        return false;
    }

    // Check shared destination state first
    let state = ADMIN_STATE.lock().await.remove(&telegram_id);
    if let Some(state) = state {
        let text = msg.text().unwrap_or("").trim();

        match state {
            AdminState::WaitingForServerDeleteConfirm { server_id, confirm_digit } => {
                handle_server_delete_provider_confirm(bot, msg, pool, text, server_id, confirm_digit).await;
            }
            AdminState::WaitingForServerForceReplaceConfirm { server_id, confirm_digit } => {
                handle_server_force_replace_confirm(bot, msg, pool, text, server_id, confirm_digit).await;
            }
            AdminState::WaitingForTestUrl => {
                handle_test_url_input(bot, msg, pool, telegram_id, text).await;
            }
            AdminState::WaitingForBulkEmails => {
                handle_bulk_emails_input(bot, msg, pool, telegram_id, text).await;
            }
            AdminState::WaitingForEmailIdToDelete => {
                handle_email_id_to_delete_input(bot, msg, pool, telegram_id, text).await;
            }
            AdminState::WaitingForMainSrvValue { field } => {
                handle_mainsrv_value_input(bot, msg, pool, telegram_id, text, field).await;
            }
            AdminState::WaitingForMailTplSubject => {
                handle_mail_tpl_input(bot, msg, pool, telegram_id, text, true).await;
            }
            AdminState::WaitingForMailTplBody => {
                handle_mail_tpl_input(bot, msg, pool, telegram_id, text, false).await;
            }
        }

        return true;
    }

    // Try each provider's text input handler
    for module in providers::all() {
        if module.handle_text_input(bot, msg, pool, telegram_id).await {
            return true;
        }
    }

    false
}

// ── Callback Query Handler ──

pub async fn handle_admin_callback(
    bot: &Bot, cq: &CallbackQuery, pool: &SqlitePool, data: &str,
) -> ResponseResult<()> {
    let telegram_id = cq.from.id.0 as i64;
    if !is_admin_id(telegram_id) {
        bot.answer_callback_query(cq.id.clone()).await?;
        return Ok(());
    }

    bot.answer_callback_query(cq.id.clone()).await?;

    match data {
        btn::CB_ADMIN_BACK => {
            clear_state(telegram_id).await;
            handle_back_to_menu(bot, cq, pool).await;
        }
        btn::CB_ADMIN_SERVER | btn::CB_SERVER_REFRESH => {
            clear_state(telegram_id).await;
            handle_server_status_cb(bot, cq, pool).await;
        }
        btn::CB_ADMIN_TEST_URLS => {
            clear_state(telegram_id).await;
            handle_test_urls_list(bot, cq, pool).await;
        }
        btn::CB_ADMIN_ADD_TEST_URL => {
            handle_test_url_add_start(bot, cq, telegram_id).await;
        }
        btn::CB_ADMIN_EMAILS => {
            clear_state(telegram_id).await;
            handle_emails_list(bot, cq, pool).await;
        }
        btn::CB_ADMIN_ADD_EMAILS => {
            handle_emails_add_start(bot, cq, telegram_id).await;
        }
        btn::CB_ADMIN_DEL_EMAIL => {
            handle_email_delete_start(bot, cq, telegram_id).await;
        }
        btn::CB_EMAIL_TEST => {
            handle_email_test(bot, cq, pool).await;
        }
        btn::CB_MAIL_TPL => {
            clear_state(telegram_id).await;
            handle_mail_tpl_screen(bot, cq, pool).await;
        }
        btn::CB_MAIL_TPL_EDIT_SUBJECT => {
            handle_mail_tpl_edit_start(bot, cq, pool, telegram_id, true).await;
        }
        btn::CB_MAIL_TPL_EDIT_BODY => {
            handle_mail_tpl_edit_start(bot, cq, pool, telegram_id, false).await;
        }
        btn::CB_MAIL_TPL_RESET => {
            handle_mail_tpl_reset(bot, cq, pool).await;
        }
        btn::CB_ADMIN_MAINSRV => {
            clear_state(telegram_id).await;
            handle_mainsrv_screen(bot, cq, pool).await;
        }
        btn::CB_MAINSRV_SET_IP => {
            handle_mainsrv_set_start(bot, cq, pool, telegram_id, MainSrvField::Ip).await;
        }
        btn::CB_MAINSRV_SET_PORT => {
            handle_mainsrv_set_start(bot, cq, pool, telegram_id, MainSrvField::Port).await;
        }
        btn::CB_MAINSRV_SET_USER => {
            handle_mainsrv_set_start(bot, cq, pool, telegram_id, MainSrvField::User).await;
        }
        btn::CB_MAINSRV_SET_PASS => {
            handle_mainsrv_set_start(bot, cq, pool, telegram_id, MainSrvField::Pass).await;
        }
        btn::CB_MAINSRV_TEST => {
            handle_mainsrv_test(bot, cq, pool).await;
        }
        btn::CB_MAINSRV_RESET_DNAT => {
            handle_mainsrv_reset_prompt(bot, cq).await;
        }
        btn::CB_MAINSRV_RESET_DNAT_CONFIRM => {
            handle_mainsrv_reset_apply(bot, cq, pool).await;
        }
        other => {
            if let Some(id_str) = other.strip_prefix(btn::CB_SERVER_DETAIL_PREFIX) {
                if let Ok(id) = id_str.parse::<i64>() {
                    handle_server_detail(bot, cq, pool, id).await;
                }
            } else if let Some(id_str) = other.strip_prefix(btn::CB_SERVER_DELETE_PREFIX) {
                if let Ok(id) = id_str.parse::<i64>() {
                    handle_server_delete_prompt(bot, cq, id).await;
                }
            } else if let Some(id_str) = other.strip_prefix(btn::CB_SERVER_DELETE_DB_PREFIX) {
                if let Ok(id) = id_str.parse::<i64>() {
                    handle_server_delete_db(bot, cq, pool, id).await;
                }
            } else if let Some(id_str) = other.strip_prefix(btn::CB_SERVER_DELETE_PROVIDER_PREFIX) {
                if let Ok(id) = id_str.parse::<i64>() {
                    handle_server_delete_provider_start(bot, cq, telegram_id, id).await;
                }
            } else if let Some(id_str) = other.strip_prefix(btn::CB_SERVER_FORCE_REPLACE_PREFIX) {
                if let Ok(id) = id_str.parse::<i64>() {
                    handle_server_force_replace_prompt(bot, cq, telegram_id, id).await;
                }
            } else if let Some(id_str) = other.strip_prefix(btn::CB_TEST_URL_DELETE_PREFIX) {
                if let Ok(id) = id_str.parse::<i64>() {
                    handle_test_url_delete(bot, cq, pool, id).await;
                }
            } else if let Some(id_str) = other.strip_prefix(btn::CB_EMAIL_DELETE_PREFIX) {
                if let Ok(id) = id_str.parse::<i64>() {
                    handle_email_delete(bot, cq, pool, id).await;
                }
            } else if let Some(rest) = other.strip_prefix("adm_retry:") {
                handle_provisioner_retry(bot, cq, rest).await;
            } else {
                // Try provider modules
                for module in providers::all() {
                    if module.matches_callback(other) {
                        module.handle_callback(bot, cq, pool, other, telegram_id).await;
                        break;
                    }
                }
            }
        }
    }

    Ok(())
}

// ── Retry-now button (from provisioner error notifications) ──

async fn handle_provisioner_retry(bot: &Bot, cq: &CallbackQuery, payload: &str) {
    let Some((provider_type, id_str)) = payload.split_once(':') else {
        return;
    };
    let Ok(account_id) = id_str.parse::<i64>() else {
        return;
    };

    let provider_type = provider_type.to_string();
    let fired = provisioner::request_retry(&provider_type, account_id).await;

    let text = if fired {
        format!(
            "🔄 Retry signal sent to {} account #{}\\.\nThe provisioner will run a fresh cycle within seconds and skip any active cooldown\\.",
            escape_md(&provider_type), account_id,
        )
    } else {
        format!(
            "⚠️ No running provisioner found for {} account #{}\\.\nIs the account currently active?",
            escape_md(&provider_type), account_id,
        )
    };

    if let Some(msg) = &cq.message {
        let _ = bot.edit_message_text(msg.chat().id, msg.id(), text)
            .parse_mode(ParseMode::MarkdownV2)
            .await;
    }
}

// ── Menu / Navigation ──

async fn handle_back_to_menu(bot: &Bot, cq: &CallbackQuery, pool: &SqlitePool) {
    let Some(msg) = &cq.message else { return };

    let text = build_admin_panel_text(pool).await;

    let _ = bot.edit_message_text(msg.chat().id, msg.id(), text)
        .parse_mode(ParseMode::MarkdownV2)
        .reply_markup(admin_menu_keyboard())
        .await;
}

// ── Server Status ──

async fn handle_server_status_cb(bot: &Bot, cq: &CallbackQuery, pool: &SqlitePool) {
    let Some(msg) = &cq.message else { return };

    let mut servers = db::get_all_active_servers(pool).await.unwrap_or_default();

    // Keep the button order aligned with the text list below, which is rendered
    // per-provider in providers::all() order then by account. get_all_active_servers
    // returns rows ORDER BY id, so without this re-sort the buttons wouldn't line
    // up with the per-provider sections the admin is reading.
    let provider_order: Vec<String> =
        providers::all().iter().map(|m| m.provider_type().to_string()).collect();
    let rank = |pt: &str| provider_order.iter().position(|p| p == pt).unwrap_or(usize::MAX);
    servers.sort_by(|a, b| {
        rank(&a.provider_type).cmp(&rank(&b.provider_type))
            .then(a.provider_account_id.cmp(&b.provider_account_id))
            .then(a.id.cmp(&b.id))
    });

    let mut has_any_accounts = false;
    let mut text = String::from("🖥 *Server Status*\n\n");

    for module in providers::all() {
        let status = module.render_server_status(pool).await;
        if !status.is_empty() {
            has_any_accounts = true;
            text.push_str(&status);
        }
    }

    if !has_any_accounts {
        let _ = bot.edit_message_text(msg.chat().id, msg.id(),
            "🖥 *Server Status*\n\n⚪ No accounts configured\\.\n\nAdd an account first\\.")
            .parse_mode(ParseMode::MarkdownV2)
            .reply_markup(server_list_keyboard(&[]))
            .await;
        return;
    }

    let _ = bot.edit_message_text(msg.chat().id, msg.id(), text)
        .parse_mode(ParseMode::MarkdownV2)
        .reply_markup(server_list_keyboard(&servers))
        .await;
}

// ── Server Detail ──

async fn handle_server_detail(bot: &Bot, cq: &CallbackQuery, pool: &SqlitePool, server_id: i64) {
    let Some(msg) = &cq.message else { return };

    let srv = match db::get_server_by_id(pool, server_id).await {
        Ok(Some(s)) => s,
        _ => {
            let _ = bot.edit_message_text(msg.chat().id, msg.id(), "Server not found\\.")
                .parse_mode(ParseMode::MarkdownV2)
                .reply_markup(back_keyboard())
                .await;
            return;
        }
    };

    let ip = srv.ipv4.as_deref().unwrap_or("pending");
    let status_icon = match srv.status.as_str() {
        "on" if srv.script_deployed => "🟢",
        "on" => "🟡",
        "creating" | "installing" => "🟡",
        "deleted" => "⚪",
        _ => "🔴",
    };
    let mz = if srv.front_published { "✅ synced" } else { "⏳ pending" };
    let deploy_state = if srv.script_deployed { "✅ deployed" } else { "⏳ pending" };

    // SSH credentials block. `ssh_user_for` gives the right user from the
    // static map (root for most providers).
    let ssh_user: String = crate::utils::ssh_user_for(&srv.provider_type).to_string();
    let creds_block = if srv.deleted_at.is_some() {
        String::new()
    } else {
        match (&srv.root_pass, srv.ipv4.is_some()) {
            (Some(pass), true) => format!(
                "\n🔐 SSH: `{}@{}`\n🔑 Pass: `{}`",
                escape_md(&ssh_user), ip, escape_md(pass),
            ),
            _ => String::new(),
        }
    };

    // Aggregate last-cycle error from provider_account_state, if any. Shown
    // as a single line at the bottom so the admin sees "why is this server
    // stuck" without leaving the page.
    let last_err_line = db::get_account_state(pool, &srv.provider_type, srv.provider_account_id)
        .await
        .ok()
        .flatten()
        .and_then(|s| match (s.last_error_category, s.last_error_at) {
            (Some(cat), Some(at)) => {
                let mins = crate::time_utils::minutes_since(&at);
                Some(format!("\n⚠️ Last cycle error: `{}` \\({}m ago\\)", escape_md(&cat), mins))
            }
            _ => None,
        })
        .unwrap_or_default();

    let text = format!(
        "{} *Server \\#{}* — `{}`\n\
         _{}_\n\n\
         🌐 IP: `{}`{}\n\
         📊 Status: *{}* · {} · {}\n\
         📅 Created: {}\n\
         🔁 Curl fails: {} · SSH fails: {}{}",
        status_icon, srv.id, escape_md(&srv.provider_type),
        escape_md(&srv.provider_server_id),
        ip, creds_block,
        escape_md(&srv.status), deploy_state, mz,
        escape_md(&srv.created_at),
        srv.consecutive_curl_fails, srv.consecutive_ssh_fails,
        last_err_line,
    );

    let mut buttons = vec![
        vec![InlineKeyboardButton::callback(
            "🗑 Delete Server",
            format!("{}{}", btn::CB_SERVER_DELETE_PREFIX, srv.id),
        )],
        vec![InlineKeyboardButton::callback("🖥 Server List", btn::CB_ADMIN_SERVER)],
        vec![InlineKeyboardButton::callback("⬅️ Back to Menu", btn::CB_ADMIN_BACK)],
    ];

    // "Force Replace" — only meaningful for bot-managed providers (i.e. not
    // 'manual', whose lifecycle is intentionally outside AccountProvisioner).
    // Shown alongside Delete so the admin can recycle a server that the bot
    // considers healthy (curl_test passing) but is actually broken from the
    // user-facing side.
    if srv.deleted_at.is_none() && srv.provider_type != "manual" {
        buttons.insert(1, vec![InlineKeyboardButton::callback(
            "🔁 Force Replace",
            format!("{}{}", btn::CB_SERVER_FORCE_REPLACE_PREFIX, srv.id),
        )]);
    }

    // Don't show delete for already deleted
    if srv.deleted_at.is_some() {
        buttons.remove(0);
    }

    let _ = bot.edit_message_text(msg.chat().id, msg.id(), text)
        .parse_mode(ParseMode::MarkdownV2)
        .reply_markup(InlineKeyboardMarkup::new(buttons))
        .await;
}

// ── Server Delete ──

async fn handle_server_delete_prompt(bot: &Bot, cq: &CallbackQuery, server_id: i64) {
    let Some(msg) = &cq.message else { return };

    let _ = bot.edit_message_text(msg.chat().id, msg.id(), format!(
        "⚠️ *Delete server \\#{}?*\n\n\
         🗃 *DB only* — Remove from bot database\\. Server stays on provider\\.\n\
         ☁️ *DB \\+ Provider* — Delete from provider AND database \\(irreversible\\)\\.",
        server_id
    ))
        .parse_mode(ParseMode::MarkdownV2)
        .reply_markup(InlineKeyboardMarkup::new(vec![
            vec![InlineKeyboardButton::callback("🗃 DB only", format!("{}{}", btn::CB_SERVER_DELETE_DB_PREFIX, server_id))],
            vec![InlineKeyboardButton::callback("☁️ DB + Provider", format!("{}{}", btn::CB_SERVER_DELETE_PROVIDER_PREFIX, server_id))],
            vec![InlineKeyboardButton::callback("❌ Cancel", format!("{}{}", btn::CB_SERVER_DETAIL_PREFIX, server_id))],
        ]))
        .await;
}

async fn handle_server_delete_db(bot: &Bot, cq: &CallbackQuery, pool: &SqlitePool, server_id: i64) {
    let Some(msg) = &cq.message else { return };

    // Look up the row before soft-deleting so the cleanup below can republish
    // the front IP (drop the dead address from the OVPN-AS backend).
    let srv_opt = db::get_server_by_id(pool, server_id).await.ok().flatten();

    if let Err(e) = db::soft_delete_server(pool, server_id).await {
        log::error!("soft_delete_server({}): {}", server_id, e);
    }

    let mut text = format!("✅ Server \\#{} removed from database\\.", server_id);
    if let Some(srv) = srv_opt {
        match provisioner::cleanup_server(pool, &srv).await {
            Ok(()) => text.push_str("\n🔄 OpenVPN backend updated\\."),
            Err(e) => {
                log::error!("Publish cleanup after server DB delete: {}", e);
                text.push_str(&format!("\n⚠️ Publish cleanup failed: {}", escape_md(&e)));
            }
        }
    } else {
        match provisioner::publish_front_ip(pool).await {
            Ok(()) => text.push_str("\n🔄 Front IP republished\\."),
            Err(e) => {
                log::error!("Publish after server DB delete: {}", e);
                text.push_str(&format!("\n⚠️ Publish failed: {}", escape_md(&e.to_string())));
            }
        }
    }

    let _ = bot.edit_message_text(msg.chat().id, msg.id(), text)
        .parse_mode(ParseMode::MarkdownV2)
        .reply_markup(InlineKeyboardMarkup::new(vec![
            vec![InlineKeyboardButton::callback("🖥 Server List", btn::CB_ADMIN_SERVER)],
            vec![InlineKeyboardButton::callback("⬅️ Back to Menu", btn::CB_ADMIN_BACK)],
        ]))
        .await;
}

async fn handle_server_delete_provider_start(bot: &Bot, cq: &CallbackQuery, telegram_id: i64, server_id: i64) {
    let Some(msg) = &cq.message else { return };

    let digit = rand::thread_rng().gen_range(0..=9u8);
    ADMIN_STATE.lock().await.insert(telegram_id, AdminState::WaitingForServerDeleteConfirm {
        server_id,
        confirm_digit: digit,
    });

    let _ = bot.edit_message_text(msg.chat().id, msg.id(), format!(
        "⚠️ *This will delete server \\#{} from the provider\\!*\n\n\
         This action is *irreversible*\\.\n\n\
         Type `{}` to confirm\\.",
        server_id, digit
    ))
        .parse_mode(ParseMode::MarkdownV2)
        .reply_markup(InlineKeyboardMarkup::new(vec![
            vec![InlineKeyboardButton::callback("❌ Cancel", format!("{}{}", btn::CB_SERVER_DETAIL_PREFIX, server_id))],
        ]))
        .await;
}

async fn handle_server_delete_provider_confirm(
    bot: &Bot, msg: &Message, pool: &SqlitePool, text: &str, server_id: i64, confirm_digit: u8,
) {
    if text.trim() != confirm_digit.to_string() {
        let _ = bot.send_message(msg.chat.id, "❌ Wrong code\\. Delete cancelled\\.")
            .parse_mode(ParseMode::MarkdownV2)
            .reply_markup(InlineKeyboardMarkup::new(vec![
                vec![InlineKeyboardButton::callback("🖥 Server List", btn::CB_ADMIN_SERVER)],
                vec![InlineKeyboardButton::callback("⬅️ Back to Menu", btn::CB_ADMIN_BACK)],
            ]))
            .await;
        return;
    }

    let srv = match db::get_server_by_id(pool, server_id).await {
        Ok(Some(s)) => s,
        _ => {
            let _ = bot.send_message(msg.chat.id, "⚠️ Server not found\\.")
                .parse_mode(ParseMode::MarkdownV2)
                .reply_markup(back_keyboard())
                .await;
            return;
        }
    };

    // Find the right provider module and delete from provider
    let mut provider_deleted = false;
    for module in providers::all() {
        if module.provider_type() == srv.provider_type {
            match module.get_provider_client(pool, srv.provider_account_id).await {
                Ok(client) => {
                    match client.delete_server(&srv.provider_server_id).await {
                        Ok(()) => {
                            provider_deleted = true;
                            log::info!("Admin deleted server {} from provider {}", srv.provider_server_id, srv.provider_type);
                        }
                        Err(e) => {
                            let _ = bot.send_message(msg.chat.id, format!(
                                "❌ Provider delete failed: {}\n\nServer NOT removed from DB\\.",
                                escape_md(&e.to_string())
                            ))
                                .parse_mode(ParseMode::MarkdownV2)
                                .reply_markup(InlineKeyboardMarkup::new(vec![
                                    vec![InlineKeyboardButton::callback("🖥 Server List", btn::CB_ADMIN_SERVER)],
                                    vec![InlineKeyboardButton::callback("⬅️ Back to Menu", btn::CB_ADMIN_BACK)],
                                ]))
                                .await;
                            return;
                        }
                    }
                }
                Err(e) => {
                    let _ = bot.send_message(msg.chat.id, format!(
                        "❌ Could not create provider client: {}", escape_md(&e)
                    ))
                        .parse_mode(ParseMode::MarkdownV2)
                        .reply_markup(back_keyboard())
                        .await;
                    return;
                }
            }
            break;
        }
    }

    if !provider_deleted {
        let _ = bot.send_message(msg.chat.id,
            format!("⚠️ Unknown provider `{}`\\. Server removed from DB only\\.", escape_md(&srv.provider_type)))
            .parse_mode(ParseMode::MarkdownV2)
            .await;
    }

    if let Err(e) = db::soft_delete_server(pool, server_id).await {
        log::error!("soft_delete_server({}): {}", server_id, e);
    }

    let ip = srv.ipv4.as_deref().unwrap_or("unknown");
    let mut text = format!(
        "✅ Server \\#{} deleted from {} and database\\.\n🌐 IP: `{}`",
        server_id, escape_md(&srv.provider_type), ip
    );
    match provisioner::cleanup_server(pool, &srv).await {
        Ok(()) => text.push_str("\n🔄 OpenVPN backend updated\\."),
        Err(e) => {
            log::error!("Publish cleanup after server provider delete: {}", e);
            text.push_str(&format!("\n⚠️ Publish cleanup failed: {}", escape_md(&e)));
        }
    }

    let _ = bot.send_message(msg.chat.id, text)
        .parse_mode(ParseMode::MarkdownV2)
        .reply_markup(InlineKeyboardMarkup::new(vec![
            vec![InlineKeyboardButton::callback("🖥 Server List", btn::CB_ADMIN_SERVER)],
            vec![InlineKeyboardButton::callback("⬅️ Back to Menu", btn::CB_ADMIN_BACK)],
        ]))
        .await;
}

// ── Force Replace Server ──
//
// Same delete-from-provider mechanics as the regular delete flow, but after
// soft-deleting the row we fire `provisioner::request_retry` so the
// per-account `AccountProvisioner` wakes immediately (clearing any pending
// cooldown) and provisions a fresh server on its next cycle (~seconds, not
// the default check interval). Use case: bot's curl_test on the existing
// server passes, but the admin can see from the user side that it's
// effectively dead — no other path forces a recycle.

async fn handle_server_force_replace_prompt(bot: &Bot, cq: &CallbackQuery, telegram_id: i64, server_id: i64) {
    let Some(msg) = &cq.message else { return };
    let digit = rand::thread_rng().gen_range(0..=9u8);
    ADMIN_STATE.lock().await.insert(telegram_id, AdminState::WaitingForServerForceReplaceConfirm {
        server_id,
        confirm_digit: digit,
    });
    let _ = bot.edit_message_text(msg.chat().id, msg.id(), format!(
        "🔁 *Force replace server \\#{}?*\n\n\
         This will:\n\
         • Delete the server from the provider \\(irreversible\\)\n\
         • Stop advertising it on the OpenVPN backend\n\
         • Immediately wake the provisioner to create a fresh one\n\n\
         Use this when the bot thinks the server is healthy but users can't connect through it\\.\n\n\
         Type `{}` to confirm\\.",
        server_id, digit
    ))
        .parse_mode(ParseMode::MarkdownV2)
        .reply_markup(InlineKeyboardMarkup::new(vec![
            vec![InlineKeyboardButton::callback("❌ Cancel", format!("{}{}", btn::CB_SERVER_DETAIL_PREFIX, server_id))],
        ]))
        .await;
}

async fn handle_server_force_replace_confirm(
    bot: &Bot, msg: &Message, pool: &SqlitePool, text: &str, server_id: i64, confirm_digit: u8,
) {
    if text.trim() != confirm_digit.to_string() {
        let _ = bot.send_message(msg.chat.id, "❌ Wrong code\\. Force replace cancelled\\.")
            .parse_mode(ParseMode::MarkdownV2)
            .reply_markup(InlineKeyboardMarkup::new(vec![
                vec![InlineKeyboardButton::callback("🖥 Server List", btn::CB_ADMIN_SERVER)],
                vec![InlineKeyboardButton::callback("⬅️ Back to Menu", btn::CB_ADMIN_BACK)],
            ]))
            .await;
        return;
    }

    let srv = match db::get_server_by_id(pool, server_id).await {
        Ok(Some(s)) => s,
        _ => {
            let _ = bot.send_message(msg.chat.id, "⚠️ Server not found\\.")
                .parse_mode(ParseMode::MarkdownV2)
                .reply_markup(back_keyboard())
                .await;
            return;
        }
    };
    let provider_type = srv.provider_type.clone();
    let account_id = srv.provider_account_id;
    let ip = srv.ipv4.clone().unwrap_or_else(|| "unknown".to_string());

    // Find the right provider module and delete from provider — same code
    // path as handle_server_delete_provider_confirm; we deliberately don't
    // factor it out yet since the bodies are short and the two flows may
    // diverge (e.g. force-replace might want to skip provider-delete on
    // certain providers in the future).
    let mut provider_deleted = false;
    for module in providers::all() {
        if module.provider_type() == provider_type {
            match module.get_provider_client(pool, account_id).await {
                Ok(client) => match client.delete_server(&srv.provider_server_id).await {
                    Ok(()) => provider_deleted = true,
                    Err(e) => {
                        let _ = bot.send_message(msg.chat.id, format!(
                            "❌ Provider delete failed: {}\nBot DB and OpenVPN state untouched\\.",
                            escape_md(&e.to_string()),
                        )).parse_mode(ParseMode::MarkdownV2).reply_markup(back_keyboard()).await;
                        return;
                    }
                },
                Err(e) => {
                    let _ = bot.send_message(msg.chat.id, format!(
                        "❌ Could not create provider client: {}", escape_md(&e)
                    )).parse_mode(ParseMode::MarkdownV2).reply_markup(back_keyboard()).await;
                    return;
                }
            }
            break;
        }
    }

    if !provider_deleted {
        let _ = bot.send_message(msg.chat.id, format!(
            "⚠️ Unknown provider `{}`\\. Aborting force replace\\.", escape_md(&provider_type)
        )).parse_mode(ParseMode::MarkdownV2).reply_markup(back_keyboard()).await;
        return;
    }

    // Provider delete OK — soft-delete the row + clean publish side. Same
    // helper used by every other server-delete path, so this stays consistent.
    if let Err(e) = db::soft_delete_server(pool, server_id).await {
        log::error!("force_replace soft_delete_server({}): {}", server_id, e);
    }
    let mut text_out = format!(
        "🔁 Server \\#{} force\\-replaced\\.\n🗑 Deleted from {}\n🌐 Old IP: `{}`",
        server_id, escape_md(&provider_type), ip
    );
    match provisioner::cleanup_server(pool, &srv).await {
        Ok(()) => text_out.push_str("\n🔄 OpenVPN backend updated\\."),
        Err(e) => {
            log::error!("force_replace publish cleanup: {}", e);
            text_out.push_str(&format!("\n⚠️ Publish cleanup failed: {}", escape_md(&e)));
        }
    }

    // Wake the per-account provisioner so it stops sleeping and provisions
    // a new server immediately, instead of waiting up to check_interval_secs
    // for the next cycle. `request_retry` returns false if no provisioner
    // is currently running for the key (account just deactivated), which is
    // expected and harmless — the manager's 30s tick will pick it up.
    let awoken = provisioner::request_retry(&provider_type, account_id).await;
    if awoken {
        text_out.push_str("\n⚡ Provisioner woken — fresh server should appear in seconds\\.");
    } else {
        text_out.push_str("\n⏳ Provisioner not running for this account — next 30s tick will pick it up\\.");
    }

    let _ = bot.send_message(msg.chat.id, text_out)
        .parse_mode(ParseMode::MarkdownV2)
        .reply_markup(InlineKeyboardMarkup::new(vec![
            vec![InlineKeyboardButton::callback("🖥 Server List", btn::CB_ADMIN_SERVER)],
            vec![InlineKeyboardButton::callback("⬅️ Back to Menu", btn::CB_ADMIN_BACK)],
        ]))
        .await;
}

// ── Test URLs ──

async fn handle_test_urls_list(bot: &Bot, cq: &CallbackQuery, pool: &SqlitePool) {
    let Some(msg) = &cq.message else { return };

    let urls = db::list_test_urls(pool).await.unwrap_or_default();

    let mut text = String::from("🌐 *Test URLs*\n\n");
    if urls.is_empty() {
        text.push_str("_No URLs configured\\. The bot cannot run probes without at least one\\._\n\n");
    } else {
        text.push_str("These URLs are curl\\-tested from each provisioned server\\. A node is considered UP if any one of them returns 2xx/3xx\\.\n\n");
        for u in &urls {
            text.push_str(&format!("• `{}`\n", escape_md(&u.url)));
        }
    }

    let mut buttons: Vec<Vec<InlineKeyboardButton>> = Vec::new();
    for u in &urls {
        // Truncate the URL display so the button label stays readable.
        let label = if u.url.len() > 32 { format!("🗑 {}…", &u.url[..30]) } else { format!("🗑 {}", u.url) };
        buttons.push(vec![InlineKeyboardButton::callback(
            label,
            format!("{}{}", btn::CB_TEST_URL_DELETE_PREFIX, u.id),
        )]);
    }
    buttons.push(vec![InlineKeyboardButton::callback("➕ Add URL", btn::CB_ADMIN_ADD_TEST_URL)]);
    buttons.push(vec![InlineKeyboardButton::callback("⬅️ Back", btn::CB_ADMIN_BACK)]);

    let _ = bot.edit_message_text(msg.chat().id, msg.id(), text)
        .parse_mode(ParseMode::MarkdownV2)
        .reply_markup(InlineKeyboardMarkup::new(buttons))
        .await;
}

async fn handle_test_url_add_start(bot: &Bot, cq: &CallbackQuery, telegram_id: i64) {
    let Some(msg) = &cq.message else { return };

    ADMIN_STATE.lock().await.insert(telegram_id, AdminState::WaitingForTestUrl);

    let _ = bot.edit_message_text(msg.chat().id, msg.id(),
        "➕ *Add Test URL*\n\n\
         Paste the full URL \\(must start with `http://` or `https://`\\)\\.\n\n\
         Example: `https://example\\.tm/`\n\n\
         Send /start to cancel\\.")
        .parse_mode(ParseMode::MarkdownV2)
        .reply_markup(back_keyboard())
        .await;
}

async fn handle_test_url_input(bot: &Bot, msg: &Message, pool: &SqlitePool, telegram_id: i64, text: &str) {
    let parsed = match reqwest::Url::parse(text) {
        Ok(u) => u,
        Err(e) => {
            ADMIN_STATE.lock().await.insert(telegram_id, AdminState::WaitingForTestUrl);
            let _ = bot.send_message(msg.chat.id, format!(
                "⚠️ Could not parse URL: {}\\. Try again or send /start to cancel\\.",
                escape_md(&e.to_string()),
            ))
                .parse_mode(ParseMode::MarkdownV2)
                .reply_markup(back_keyboard())
                .await;
            return;
        }
    };

    let scheme = parsed.scheme();
    if scheme != "http" && scheme != "https" {
        ADMIN_STATE.lock().await.insert(telegram_id, AdminState::WaitingForTestUrl);
        let _ = bot.send_message(msg.chat.id,
            "⚠️ URL must use `http` or `https`\\. Try again or send /start to cancel\\.")
            .parse_mode(ParseMode::MarkdownV2)
            .reply_markup(back_keyboard())
            .await;
        return;
    }

    let url_str = parsed.as_str().to_string();
    match db::add_test_url(pool, &url_str).await {
        Ok(_) => {
            let _ = bot.send_message(msg.chat.id, format!(
                "✅ Added test URL: `{}`",
                escape_md(&url_str),
            ))
                .parse_mode(ParseMode::MarkdownV2)
                .reply_markup(InlineKeyboardMarkup::new(vec![
                    vec![InlineKeyboardButton::callback("🌐 Test URLs", btn::CB_ADMIN_TEST_URLS)],
                    vec![InlineKeyboardButton::callback("⬅️ Back to Menu", btn::CB_ADMIN_BACK)],
                ]))
                .await;
        }
        Err(e) => {
            let already = e.to_string().contains("UNIQUE constraint failed");
            let body = if already {
                "⚠️ That URL is already in the list\\.".to_string()
            } else {
                format!("⚠️ Database error: {}", escape_md(&e.to_string()))
            };
            let _ = bot.send_message(msg.chat.id, body)
                .parse_mode(ParseMode::MarkdownV2)
                .reply_markup(back_keyboard())
                .await;
        }
    }
}

async fn handle_test_url_delete(bot: &Bot, cq: &CallbackQuery, pool: &SqlitePool, id: i64) {
    let Some(msg) = &cq.message else { return };

    let count = db::count_active_test_urls(pool).await.unwrap_or(0);
    if count <= 1 {
        let _ = bot.edit_message_text(msg.chat().id, msg.id(),
            "⚠️ Cannot delete the last test URL — the bot needs at least one to probe nodes\\. Add another URL first, then delete this one\\.")
            .parse_mode(ParseMode::MarkdownV2)
            .reply_markup(InlineKeyboardMarkup::new(vec![
                vec![InlineKeyboardButton::callback("🌐 Test URLs", btn::CB_ADMIN_TEST_URLS)],
                vec![InlineKeyboardButton::callback("⬅️ Back", btn::CB_ADMIN_BACK)],
            ]))
            .await;
        return;
    }

    let _ = db::delete_test_url(pool, id).await;
    handle_test_urls_list(bot, cq, pool).await;
}

// ── Email list handlers (BCC recipients for front-IP-change notifications) ──
//
// Screens are sent as plain text (no MarkdownV2) — emails are user-pasted and
// full of MarkdownV2 specials (`.`, `-`, `+`, `_`), so plain text avoids both
// escaping noise and send-time 400s.

/// Max per-item delete buttons shown; longer lists fall back to "Delete by id".
const EMAIL_BUTTON_CAP: usize = 25;

async fn handle_emails_list(bot: &Bot, cq: &CallbackQuery, pool: &SqlitePool) {
    let Some(msg) = &cq.message else { return };

    let emails = db::list_emails(pool).await.unwrap_or_default();

    let mut text = format!("📧 Email recipients ({})\n\n", emails.len());
    if emails.is_empty() {
        text.push_str("No recipients saved yet.\nThese addresses are BCC'd the new server IP whenever it changes.");
    } else {
        text.push_str("BCC'd the new server IP whenever it changes.\n\n");
        for e in &emails {
            text.push_str(&format!("{}. {}\n", e.id, e.email));
        }
    }

    let mut buttons: Vec<Vec<InlineKeyboardButton>> = Vec::new();
    for e in emails.iter().take(EMAIL_BUTTON_CAP) {
        let label = if e.email.chars().count() > 32 {
            format!("🗑 {}…", e.email.chars().take(30).collect::<String>())
        } else {
            format!("🗑 {}", e.email)
        };
        buttons.push(vec![InlineKeyboardButton::callback(
            label,
            format!("{}{}", btn::CB_EMAIL_DELETE_PREFIX, e.id),
        )]);
    }
    let mut last_row = vec![InlineKeyboardButton::callback("➕ Bulk add", btn::CB_ADMIN_ADD_EMAILS)];
    if emails.len() > EMAIL_BUTTON_CAP {
        last_row.push(InlineKeyboardButton::callback("🗑 Delete by id", btn::CB_ADMIN_DEL_EMAIL));
    }
    buttons.push(last_row);
    buttons.push(vec![
        InlineKeyboardButton::callback("📝 Mail template", btn::CB_MAIL_TPL),
        InlineKeyboardButton::callback("✉️ Send test email", btn::CB_EMAIL_TEST),
    ]);
    buttons.push(vec![InlineKeyboardButton::callback("⬅️ Back", btn::CB_ADMIN_BACK)]);

    let _ = bot.edit_message_text(msg.chat().id, msg.id(), text)
        .reply_markup(InlineKeyboardMarkup::new(buttons))
        .await;
}

/// Fire a one-off test email through the configured SMTP sender so the admin
/// can verify the credentials without waiting for a real front-IP change.
/// Sends to the saved recipient list; if that's empty, sends to the sender
/// itself so the SMTP login/handshake is still exercised.
async fn handle_email_test(bot: &Bot, cq: &CallbackQuery, pool: &SqlitePool) {
    let Some(msg) = &cq.message else { return };

    let c = get_config();
    let mailer = match crate::mailer::Mailer::new(
        &c.smtp_host, c.smtp_port, &c.smtp_username, &c.smtp_password, &c.smtp_from,
    ) {
        Ok(m) => m,
        Err(crate::mailer::MailerError::NotConfigured) => {
            let _ = bot.edit_message_text(msg.chat().id, msg.id(),
                "✉️ SMTP is not configured.\n\nSet SMTP_HOST / SMTP_PORT / SMTP_USERNAME / \
                 SMTP_PASSWORD / SMTP_FROM in the server .env, then restart the bot.")
                .reply_markup(emails_done_keyboard())
                .await;
            return;
        }
        Err(e) => {
            let _ = bot.edit_message_text(msg.chat().id, msg.id(),
                format!("✉️ SMTP config error: {}", e))
                .reply_markup(emails_done_keyboard())
                .await;
            return;
        }
    };

    // Recipients = saved list, or the sender itself if the list is empty (so the
    // send path is still exercised).
    let mut recipients: Vec<String> = db::list_emails(pool).await
        .unwrap_or_default().into_iter().map(|e| e.email).collect();
    let sent_to_self = recipients.is_empty();
    if sent_to_self {
        recipients.push(c.smtp_from.clone());
    }

    let _ = bot.edit_message_text(msg.chat().id, msg.id(),
        format!("✉️ Sending test email to {} recipient(s)…", recipients.len()))
        .await;

    // Send the REAL template (rendered) so the test previews exactly what
    // recipients receive. Use the current front IP if there is one, else a
    // sample address for the {ip} key.
    let front_ip = current_front_ip(pool).await
        .unwrap_or_else(|| "203.0.113.7".to_string());
    let (subject_tpl, body_tpl) = db::get_mail_template(pool).await;
    let date = crate::time_utils::now_local_string();
    let vars = [("IP", front_ip.as_str()), ("Date", date.as_str())];
    let subject = crate::mailer::render_template(&subject_tpl, &vars);
    let body = crate::mailer::render_template(&body_tpl, &vars);

    let result = mailer.send_bcc(&recipients, &subject, &body).await;
    let text = match result {
        Ok(()) if sent_to_self => format!(
            "✅ Test email sent to the sender ({}).\n\nNo recipients are saved yet — add some with ➕ Bulk add so they get the IP-change notifications.",
            c.smtp_from),
        Ok(()) => format!("✅ Test email sent to {} recipient(s). Check the inbox(es).", recipients.len()),
        Err(e) => format!(
            "❌ Send failed: {}\n\nFor Gmail: use an App Password (not your login password), host smtp.gmail.com, port 465.",
            e),
    };
    let _ = bot.edit_message_text(msg.chat().id, msg.id(), text)
        .reply_markup(emails_done_keyboard())
        .await;
}

/// Newest live Selectel front IP (the address `publish_front_ip` would push),
/// or None if no active front exists yet.
async fn current_front_ip(pool: &SqlitePool) -> Option<String> {
    db::get_all_active_servers(pool).await.ok()?
        .into_iter()
        .filter(|s| s.provider_type == "selectel" && s.ipv4.is_some())
        .max_by_key(|s| s.id)
        .and_then(|s| s.ipv4)
}

// ── Mail template (IP-change notification body) ──
//
// Plain-text screens — templates are free-form and full of MarkdownV2 specials.

fn mail_tpl_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![
        vec![
            InlineKeyboardButton::callback("✏️ Subject", btn::CB_MAIL_TPL_EDIT_SUBJECT),
            InlineKeyboardButton::callback("✏️ Body", btn::CB_MAIL_TPL_EDIT_BODY),
        ],
        vec![InlineKeyboardButton::callback("♻️ Reset to default (Turkmen)", btn::CB_MAIL_TPL_RESET)],
        vec![InlineKeyboardButton::callback("📧 Emails", btn::CB_ADMIN_EMAILS)],
        vec![InlineKeyboardButton::callback("⬅️ Back to Menu", btn::CB_ADMIN_BACK)],
    ])
}

async fn handle_mail_tpl_screen(bot: &Bot, cq: &CallbackQuery, pool: &SqlitePool) {
    let Some(msg) = &cq.message else { return };
    let (subject, body) = db::get_mail_template(pool).await;

    let mut keys = String::new();
    for (k, desc) in crate::mailer::MAIL_TEMPLATE_KEYS {
        keys.push_str(&format!("  {} — {}\n", k, desc));
    }

    // Readable text preview (the HTML rendered to plain text) with the current
    // front IP, or a sample if there's no live front.
    let ip = current_front_ip(pool).await.unwrap_or_else(|| "203.0.113.7".to_string());
    let date = crate::time_utils::now_local_string();
    let vars = [("IP", ip.as_str()), ("Date", date.as_str())];
    let preview = crate::mailer::html_to_text(&crate::mailer::render_template(&body, &vars));

    let text = format!(
        "📝 Mail template (HTML)\n\n\
         Sent to the recipient list whenever the front IP changes.\n\n\
         Available keys (replaced on send):\n{}\n\
         ── Subject ──\n{}\n\n\
         ── Preview, ip={} (HTML rendered to text) ──\n{}\n\n\
         Tap ✏️ Body to view/edit the HTML source, or ✉️ Send test email to see it in your inbox.",
        keys, subject, ip, preview,
    );

    let _ = bot.edit_message_text(msg.chat().id, msg.id(), text)
        .reply_markup(mail_tpl_keyboard())
        .await;
}

async fn handle_mail_tpl_edit_start(
    bot: &Bot, cq: &CallbackQuery, pool: &SqlitePool, telegram_id: i64, is_subject: bool,
) {
    let Some(msg) = &cq.message else { return };
    let state = if is_subject {
        AdminState::WaitingForMailTplSubject
    } else {
        AdminState::WaitingForMailTplBody
    };
    ADMIN_STATE.lock().await.insert(telegram_id, state);

    let (subject, body) = db::get_mail_template(pool).await;
    let mut keys = String::new();
    for (k, desc) in crate::mailer::MAIL_TEMPLATE_KEYS {
        keys.push_str(&format!("  {} — {}\n", k, desc));
    }
    let (field, current, hint) = if is_subject {
        ("subject", subject, "")
    } else {
        ("body (HTML)", body,
         "\n\nThe body is HTML. Copy the current source below, edit it, and send it back. \
          ♻️ Reset restores the default Turkmen card.")
    };
    let _ = bot.edit_message_text(msg.chat().id, msg.id(), format!(
        "✏️ Send the new mail {}.\n\n\
         Keys (replaced on send):\n{}{}\n\n\
         ── Current ──\n{}\n\n\
         Send /start to cancel.",
        field, keys, hint, current,
    ))
        .reply_markup(back_keyboard())
        .await;
}

async fn handle_mail_tpl_input(
    bot: &Bot, msg: &Message, pool: &SqlitePool, telegram_id: i64, text: &str, is_subject: bool,
) {
    let value = text.trim();
    if value.is_empty() {
        let state = if is_subject { AdminState::WaitingForMailTplSubject } else { AdminState::WaitingForMailTplBody };
        ADMIN_STATE.lock().await.insert(telegram_id, state);
        let _ = bot.send_message(msg.chat.id, "Value can't be empty. Send it again, or /start to cancel.")
            .reply_markup(back_keyboard())
            .await;
        return;
    }
    // The body MUST contain {{IP}} — otherwise recipients never learn the new
    // address. The subject has no required key. ({{Date}} is always optional.)
    if !is_subject && !crate::mailer::template_has_required_key(value) {
        ADMIN_STATE.lock().await.insert(telegram_id, AdminState::WaitingForMailTplBody);
        let _ = bot.send_message(msg.chat.id,
            "⚠️ The body must include the {{IP}} key so recipients get the new address.\n\n\
             Add {{IP}} somewhere and send it again, or /start to cancel.")
            .reply_markup(back_keyboard())
            .await;
        return;
    }
    let key = if is_subject { db::MAIL_TPL_SUBJECT_KEY } else { db::MAIL_TPL_BODY_KEY };
    let field = if is_subject { "subject" } else { "body" };
    let reply = match db::set_bot_setting(pool, key, value).await {
        Ok(()) => format!("✅ Mail {} saved. Use ✉️ Send test email to preview it.", field),
        Err(e) => format!("DB error, not saved: {}", e),
    };
    let _ = bot.send_message(msg.chat.id, reply)
        .reply_markup(InlineKeyboardMarkup::new(vec![
            vec![InlineKeyboardButton::callback("📝 Mail template", btn::CB_MAIL_TPL)],
            vec![InlineKeyboardButton::callback("⬅️ Back to Menu", btn::CB_ADMIN_BACK)],
        ]))
        .await;
}

async fn handle_mail_tpl_reset(bot: &Bot, cq: &CallbackQuery, pool: &SqlitePool) {
    let _ = db::reset_mail_template(pool).await;
    handle_mail_tpl_screen(bot, cq, pool).await;
}

async fn handle_emails_add_start(bot: &Bot, cq: &CallbackQuery, telegram_id: i64) {
    let Some(msg) = &cq.message else { return };

    ADMIN_STATE.lock().await.insert(telegram_id, AdminState::WaitingForBulkEmails);

    let _ = bot.edit_message_text(msg.chat().id, msg.id(),
        "➕ Add emails\n\n\
         Send a list of emails — separate by space, comma, semicolon, or newline. \
         Duplicates and invalid entries are ignored.\n\n\
         Send /start to cancel.")
        .reply_markup(back_keyboard())
        .await;
}

async fn handle_email_delete_start(bot: &Bot, cq: &CallbackQuery, telegram_id: i64) {
    let Some(msg) = &cq.message else { return };

    ADMIN_STATE.lock().await.insert(telegram_id, AdminState::WaitingForEmailIdToDelete);

    let _ = bot.edit_message_text(msg.chat().id, msg.id(),
        "🗑 Delete email\n\nSend the id of the email to delete (see the list).\n\nSend /start to cancel.")
        .reply_markup(back_keyboard())
        .await;
}

async fn handle_email_delete(bot: &Bot, cq: &CallbackQuery, pool: &SqlitePool, id: i64) {
    let _ = db::delete_email(pool, id).await;
    handle_emails_list(bot, cq, pool).await;
}

async fn handle_bulk_emails_input(bot: &Bot, msg: &Message, pool: &SqlitePool, telegram_id: i64, text: &str) {
    let parsed = crate::mailer::parse_email_blob(text);
    if parsed.is_empty() {
        ADMIN_STATE.lock().await.insert(telegram_id, AdminState::WaitingForBulkEmails);
        let _ = bot.send_message(msg.chat.id, "No valid emails found. Try again, or send /start to cancel.")
            .reply_markup(back_keyboard())
            .await;
        return;
    }
    let mut added = 0u32;
    let mut skipped = 0u32;
    for e in &parsed {
        match db::insert_email(pool, e).await {
            Ok(true) => added += 1,
            Ok(false) => skipped += 1,
            Err(err) => {
                log::warn!("insert_email({}) failed: {}", e, err);
                skipped += 1;
            }
        }
    }
    let _ = bot.send_message(msg.chat.id,
        format!("✅ Added {} new, skipped {} (duplicates/invalid).", added, skipped))
        .reply_markup(emails_done_keyboard())
        .await;
}

async fn handle_email_id_to_delete_input(bot: &Bot, msg: &Message, pool: &SqlitePool, telegram_id: i64, text: &str) {
    let Some(id) = text.trim().parse::<i64>().ok() else {
        ADMIN_STATE.lock().await.insert(telegram_id, AdminState::WaitingForEmailIdToDelete);
        let _ = bot.send_message(msg.chat.id, "Not a number. Send the id, or /start to cancel.")
            .reply_markup(back_keyboard())
            .await;
        return;
    };
    let reply = match db::delete_email(pool, id).await {
        Ok(true) => format!("🗑 Deleted email #{}.", id),
        Ok(false) => format!("No email with id {}.", id),
        Err(e) => {
            log::warn!("delete_email failed: {}", e);
            format!("DB error: {}", e)
        }
    };
    let _ = bot.send_message(msg.chat.id, reply)
        .reply_markup(emails_done_keyboard())
        .await;
}

fn emails_done_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![
        vec![InlineKeyboardButton::callback("📧 Emails", btn::CB_ADMIN_EMAILS)],
        vec![InlineKeyboardButton::callback("⬅️ Back to Menu", btn::CB_ADMIN_BACK)],
    ])
}

// ── Main Server (OVPN-AS backend) handlers ──
//
// Field-by-field credential editing, stored in `bot_settings` with `.env` as
// fallback (see `db::get_ovpn_backend`). Screens are plain text (no
// MarkdownV2) — IPs and usernames are full of MarkdownV2 specials.

impl MainSrvField {
    fn kv_key(self) -> &'static str {
        match self {
            MainSrvField::Ip => db::OVPN_KEY_IP,
            MainSrvField::Port => db::OVPN_KEY_PORT,
            MainSrvField::User => db::OVPN_KEY_USER,
            MainSrvField::Pass => db::OVPN_KEY_PASS,
        }
    }

    fn label(self) -> &'static str {
        match self {
            MainSrvField::Ip => "IP",
            MainSrvField::Port => "API port",
            MainSrvField::User => "API user",
            MainSrvField::Pass => "API password",
        }
    }
}

/// Which layer supplies a backend field right now: the bot's KV override,
/// the `.env` fallback, or nothing.
async fn mainsrv_source(pool: &SqlitePool, key: &str, env_is_set: bool) -> &'static str {
    match db::get_bot_setting(pool, key).await {
        Ok(Some(v)) if !v.trim().is_empty() => "[bot]",
        _ if env_is_set => "[env]",
        _ => "[not set]",
    }
}

async fn mainsrv_screen_text(pool: &SqlitePool) -> String {
    let b = db::get_ovpn_backend(pool).await;
    let c = get_config();

    let ip_src = mainsrv_source(pool, db::OVPN_KEY_IP, !c.openvpn_backend_ip.is_empty()).await;
    let port_src = mainsrv_source(pool, db::OVPN_KEY_PORT, true).await; // env always defaults to 943
    let user_src = mainsrv_source(pool, db::OVPN_KEY_USER, !c.openvpn_api_user.is_empty()).await;
    let pass_src = mainsrv_source(pool, db::OVPN_KEY_PASS, !c.openvpn_api_pass.is_empty()).await;

    let ip_disp = if b.ip.is_empty() { "—".to_string() } else { b.ip.clone() };
    let user_disp = if b.user.is_empty() { "—".to_string() } else { b.user.clone() };
    let pass_disp = if b.pass.is_empty() { "—" } else { "••••••••" };

    // Current front + publish state (newest live selectel server, same
    // selection as publish_front_ip).
    let front_line = match db::get_all_active_servers(pool).await {
        Ok(servers) => servers.iter()
            .filter(|s| s.provider_type == "selectel" && s.ipv4.is_some())
            .max_by_key(|s| s.id)
            .map(|s| format!(
                "Active front: {} ({})",
                s.ipv4.as_deref().unwrap_or("?"),
                if s.front_published { "published ✅" } else { "publish pending ⏳" },
            ))
            .unwrap_or_else(|| "Active front: none yet".to_string()),
        Err(_) => "Active front: ?".to_string(),
    };

    format!(
        "🖥 Main Server (OpenVPN-AS backend)\n\n\
         The fixed exit every front DNATs to. The bot logs into its REST API to \
         point host.name at the active front IP.\n\n\
         IP: {} {}\n\
         API port: {} {}\n\
         API user: {} {}\n\
         API password: {} {}\n\n\
         {}\n\n\
         Changing the IP automatically reboots the front and re-applies DNAT to \
         the new backend.",
        ip_disp, ip_src,
        b.port, port_src,
        user_disp, user_src,
        pass_disp, pass_src,
        front_line,
    )
}

fn mainsrv_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![
        vec![
            InlineKeyboardButton::callback("✏️ IP", btn::CB_MAINSRV_SET_IP),
            InlineKeyboardButton::callback("✏️ Port", btn::CB_MAINSRV_SET_PORT),
        ],
        vec![
            InlineKeyboardButton::callback("✏️ User", btn::CB_MAINSRV_SET_USER),
            InlineKeyboardButton::callback("✏️ Password", btn::CB_MAINSRV_SET_PASS),
        ],
        vec![InlineKeyboardButton::callback("🔌 Test connection", btn::CB_MAINSRV_TEST)],
        vec![InlineKeyboardButton::callback("🔁 Reset DNAT (reboot front)", btn::CB_MAINSRV_RESET_DNAT)],
        vec![InlineKeyboardButton::callback("⬅️ Back", btn::CB_ADMIN_BACK)],
    ])
}

fn mainsrv_done_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![
        vec![InlineKeyboardButton::callback("🖥 Main Server", btn::CB_ADMIN_MAINSRV)],
        vec![InlineKeyboardButton::callback("⬅️ Back to Menu", btn::CB_ADMIN_BACK)],
    ])
}

async fn handle_mainsrv_screen(bot: &Bot, cq: &CallbackQuery, pool: &SqlitePool) {
    let Some(msg) = &cq.message else { return };
    let text = mainsrv_screen_text(pool).await;
    let _ = bot.edit_message_text(msg.chat().id, msg.id(), text)
        .reply_markup(mainsrv_keyboard())
        .await;
}

async fn handle_mainsrv_set_start(
    bot: &Bot, cq: &CallbackQuery, pool: &SqlitePool, telegram_id: i64, field: MainSrvField,
) {
    let Some(msg) = &cq.message else { return };

    ADMIN_STATE.lock().await.insert(telegram_id, AdminState::WaitingForMainSrvValue { field });

    let b = db::get_ovpn_backend(pool).await;
    let current = match field {
        MainSrvField::Ip => if b.ip.is_empty() { "—".to_string() } else { b.ip },
        MainSrvField::Port => b.port.to_string(),
        MainSrvField::User => if b.user.is_empty() { "—".to_string() } else { b.user },
        MainSrvField::Pass => if b.pass.is_empty() { "— (not set)".to_string() } else { "•••••••• (set)".to_string() },
    };
    let extra = match field {
        MainSrvField::Ip => "\n\nChanging the IP reboots the front and re-applies DNAT automatically.",
        MainSrvField::Pass => "\n\nTip: delete your message afterwards so the password doesn't stay in the chat history.",
        _ => "",
    };

    let _ = bot.edit_message_text(msg.chat().id, msg.id(), format!(
        "✏️ Set backend {}\n\nCurrent: {}\n\nSend the new value.{}\n\nSend /start to cancel.",
        field.label(), current, extra,
    ))
        .reply_markup(back_keyboard())
        .await;
}

async fn handle_mainsrv_value_input(
    bot: &Bot, msg: &Message, pool: &SqlitePool, telegram_id: i64, text: &str, field: MainSrvField,
) {
    let value = text.trim();

    // Validate per field; on failure re-arm the state so the admin can retry.
    let invalid: Option<&str> = match field {
        MainSrvField::Ip if value.parse::<std::net::Ipv4Addr>().is_err() =>
            Some("Not a valid IPv4 address (e.g. 203.0.113.7)."),
        MainSrvField::Port if value.parse::<u16>().map(|p| p == 0).unwrap_or(true) =>
            Some("Not a valid port (1–65535)."),
        MainSrvField::User | MainSrvField::Pass if value.is_empty() =>
            Some("Value cannot be empty."),
        _ => None,
    };
    if let Some(err) = invalid {
        ADMIN_STATE.lock().await.insert(telegram_id, AdminState::WaitingForMainSrvValue { field });
        let _ = bot.send_message(msg.chat.id, format!("{} Try again, or send /start to cancel.", err))
            .reply_markup(back_keyboard())
            .await;
        return;
    }

    // Effective IP before the change — decides whether to auto-reset DNAT.
    let old_ip = db::get_ovpn_backend(pool).await.ip;

    if let Err(e) = db::set_bot_setting(pool, field.kv_key(), value).await {
        let _ = bot.send_message(msg.chat.id, format!("DB error, not saved: {}", e))
            .reply_markup(mainsrv_done_keyboard())
            .await;
        return;
    }

    let mut reply = format!("✅ Backend {} saved.", field.label());

    if field == MainSrvField::Ip && old_ip != value {
        let lines = crate::provisioner::reset_front_dnat(pool).await;
        if lines.is_empty() {
            reply.push_str("\n\nNo deployed front yet — the new backend takes effect on the next deploy.");
        } else {
            reply.push_str("\n\n🔁 DNAT reset triggered:\n");
            reply.push_str(&lines.join("\n"));
            reply.push_str("\n\nThe front IP is re-published to the new backend automatically. If the new backend uses different API credentials, set them too, then use 🔌 Test connection.");
        }
    }

    let _ = bot.send_message(msg.chat.id, reply)
        .reply_markup(mainsrv_done_keyboard())
        .await;
}

async fn handle_mainsrv_test(bot: &Bot, cq: &CallbackQuery, pool: &SqlitePool) {
    let Some(msg) = &cq.message else { return };

    let b = db::get_ovpn_backend(pool).await;
    let target = format!("https://{}:{}", b.ip, b.port);
    let result = match crate::openvpn::test_login(&b).await {
        Ok(()) => "✅ REST login OK — IP/port reachable, credentials accepted.".to_string(),
        Err(e) => format!("❌ {}", e),
    };

    let _ = bot.edit_message_text(msg.chat().id, msg.id(), format!(
        "🔌 Test connection\n\nTarget: {}\nUser: {}\n\n{}",
        target, if b.user.is_empty() { "—" } else { &b.user }, result,
    ))
        .reply_markup(mainsrv_done_keyboard())
        .await;
}

async fn handle_mainsrv_reset_prompt(bot: &Bot, cq: &CallbackQuery) {
    let Some(msg) = &cq.message else { return };
    let _ = bot.edit_message_text(msg.chat().id, msg.id(),
        "🔁 Reset DNAT\n\n\
         This REBOOTS the front server (≈1–2 min VPN outage), then re-applies the \
         DNAT chain against the current backend IP, restores the warm-pool rules, \
         and republishes the front to the backend — all automatically once the box \
         is back.\n\nContinue?")
        .reply_markup(InlineKeyboardMarkup::new(vec![
            vec![InlineKeyboardButton::callback("✅ Reboot & reset", btn::CB_MAINSRV_RESET_DNAT_CONFIRM)],
            vec![InlineKeyboardButton::callback("⬅️ Back", btn::CB_ADMIN_MAINSRV)],
        ]))
        .await;
}

async fn handle_mainsrv_reset_apply(bot: &Bot, cq: &CallbackQuery, pool: &SqlitePool) {
    let Some(msg) = &cq.message else { return };

    let lines = crate::provisioner::reset_front_dnat(pool).await;
    let text = if lines.is_empty() {
        "No deployed front to reset — nothing done.".to_string()
    } else {
        format!("🔁 Reset started:\n\n{}", lines.join("\n"))
    };

    let _ = bot.edit_message_text(msg.chat().id, msg.id(), text)
        .reply_markup(mainsrv_done_keyboard())
        .await;
}

