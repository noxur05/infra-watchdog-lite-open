// Pedantic clippy lints this simplified single-provider bot deliberately ignores:
// sqlx row tuples (type_complexity), the WaitingFor* state prefix (enum_variant_names),
// the spawn-then-insert provisioner map (map_entry — entry() would hold a borrow across
// the spawn .await), wide admin handlers (too_many_arguments), and doc-list formatting.
#![allow(clippy::type_complexity, clippy::enum_variant_names, clippy::map_entry, clippy::too_many_arguments, clippy::doc_overindented_list_items)]

mod backup;
mod config;
mod db;
mod diagnose;
mod commands;
mod utils;
mod handlers;
mod provider;
mod providers;
mod openvpn;
mod mailer;
mod notify;
mod provisioner;
mod ssh;
mod time_utils;

use config::get_config;
use commands::{parse_command, Command};
use handlers::admin;

use sqlx::SqlitePool;
use teloxide::prelude::*;
use teloxide::types::{BotCommand, Message, Update};


/// Safely extract telegram user from message. Returns None for channel posts / anonymous messages.
pub fn get_user(msg: &Message) -> Option<&teloxide::types::User> {
    msg.from.as_ref().filter(|u| !u.is_bot)
}

async fn handle_message(bot: Bot, msg: Message, pool: SqlitePool) -> ResponseResult<()> {
    let Some(user) = get_user(&msg) else { return Ok(()) };
    let telegram_id = user.id.0 as i64;

    log::info!("Message from user {} (id={})", user.first_name, telegram_id);

    // Only respond to admin
    if !admin::is_admin_id(telegram_id) {
        log::info!("Ignoring non-admin user {}", telegram_id);
        return Ok(());
    }

    let text = match msg.text() {
        Some(t) => t,
        None => return Ok(()),
    };

    log::info!("Admin text: {}", text);

    // Try parsing as command
    if let Some(cmd) = parse_command(text) {
        match cmd {
            Command::Start => {
                admin::handle_admin_command(&bot, &msg, &pool).await?;
            }
        }
        return Ok(());
    }

    // Check if this is admin text input (multi-step flow)
    if admin::handle_admin_text_input(&bot, &msg, &pool).await {
        return Ok(());
    }

    Ok(())
}

async fn handle_callback(bot: Bot, q: teloxide::types::CallbackQuery, pool: SqlitePool) -> ResponseResult<()> {
    let telegram_id = q.from.id.0 as i64;
    if !admin::is_admin_id(telegram_id) {
        return Ok(());
    }

    let data = match &q.data {
        Some(d) => d.clone(),
        None => return Ok(()),
    };

    // All callbacks go through admin handler
    if data.starts_with("adm_") {
        admin::handle_admin_callback(&bot, &q, &pool, &data).await?;
    }

    Ok(())
}

#[tokio::main]
async fn main() {
    env_logger::init();
    let config = get_config();

    let pool = match db::init_pool(&config.database_url).await {
        Ok(p) => {
            log::info!("Database connected");
            p
        }
        Err(e) => {
            log::error!("Database connection failed: {}", e);
            return;
        }
    };

    log::info!("Admin telegram IDs: {:?}", config.admin_telegram_ids);

    // Ensure the bot's SSH keypair exists + is materialized to a 0600 file
    // (stored in bot_settings). Manual servers get this public key installed so
    // key auth survives provider password resets (e.g. Beget on reboot). On
    // failure, SSH falls back to password auth as before.
    match ssh::ensure_bot_ssh_key(&pool).await {
        Ok(_) => log::info!("Bot SSH key ready"),
        Err(e) => log::warn!("Bot SSH key unavailable ({}); SSH falls back to password", e),
    }

    let bot = Bot::new(&config.telegram_bot_api);

    // Startup DB integrity check. `quick_check` misses btree-page corruption,
    // so use the thorough check. On failure, alert the admin and keep running
    // — do NOT auto-recover: `.recover` can silently wipe the DB, so a human
    // decides (restore from backup or rebuild offline).
    match db::integrity_check(&pool).await {
        Ok(()) => log::info!("DB integrity check: ok"),
        Err(detail) => {
            log::error!("DB INTEGRITY CHECK FAILED at startup: {}", detail);
            let bot_c = bot.clone();
            tokio::spawn(async move {
                notify::notify_admin(&bot_c, &format!(
                    "🚨 *DB integrity check failed at startup*\n\n{}\n\nThe bot is still running, but the database is corrupt\\. Don't trust `.recover` \\(it can wipe the DB\\) — restore from a backup or rebuild offline\\.",
                    utils::escape_md(&detail),
                )).await;
            });
        }
    }

    // Set bot menu commands
    let commands = vec![
        BotCommand::new("start", "Open admin panel"),
    ];
    if let Err(e) = bot.set_my_commands(commands).await {
        log::warn!("Failed to set bot commands: {}", e);
    }

    // Shutdown signal
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    // Daily encrypted SQLite backup → admin chat at 00:00 Ashgabat.
    backup::spawn_backup(bot.clone(), pool.clone(), shutdown_rx.clone());

    // Start provisioner (server lifecycle management)
    provisioner::spawn_provisioner(bot.clone(), pool.clone(), shutdown_rx);

    log::info!("Bot is running...");

    let handler = dptree::entry()
        .branch(
            Update::filter_message()
                .endpoint(handle_message)
        )
        .branch(
            Update::filter_callback_query()
                .endpoint(handle_callback)
        );

    let mut dispatcher = Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![pool.clone()])
        .build();

    // Graceful shutdown on SIGINT (Ctrl-C) OR SIGTERM. The previous code only
    // handled Ctrl-C via teloxide's enable_ctrlc_handler, but `systemctl stop`
    // — and therefore every deploy — sends SIGTERM, which hard-killed the
    // process mid-flight. Now both signals tell background tasks to wind down
    // (shutdown_tx) and stop the dispatcher so `dispatch()` returns and we
    // reach the WAL checkpoint below, leaving a clean DB for the next boot.
    let token = dispatcher.shutdown_token();
    {
        let shutdown_tx = shutdown_tx.clone();
        tokio::spawn(async move {
            let mut sigterm = match tokio::signal::unix::signal(
                tokio::signal::unix::SignalKind::terminate(),
            ) {
                Ok(s) => s,
                Err(e) => { log::error!("failed to install SIGTERM handler: {}", e); return; }
            };
            tokio::select! {
                _ = tokio::signal::ctrl_c() => log::info!("SIGINT received, shutting down…"),
                _ = sigterm.recv() => log::info!("SIGTERM received, shutting down…"),
            }
            shutdown_tx.send(true).ok();
            let _ = token.shutdown();
        });
    }

    dispatcher.dispatch().await;

    // Graceful path reached. Flush the WAL into the main DB file before exit so
    // the next boot starts from a checkpointed file with an empty WAL.
    shutdown_tx.send(true).ok();
    db::checkpoint_wal(&pool).await;
}
