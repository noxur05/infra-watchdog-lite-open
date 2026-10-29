//! Admin Telegram notification helpers — send a message (with or without an
//! inline keyboard) to every configured admin chat. No-op if no admin IDs are
//! set (`ADMIN_TELEGRAM_ID` empty).
//!
//! Lives at the crate root because basically every operational subsystem
//! (provisioner, manual monitor, manual setup) sends admin alerts.

use teloxide::prelude::*;
use teloxide::types::{InlineKeyboardMarkup, ParseMode};

use crate::config::get_config;

/// Send a plain notification to all admins.
pub(crate) async fn notify_admin(bot: &Bot, text: &str) {
    send_to_admins(bot, text, None).await;
}

/// Send an admin notification with an inline keyboard attached, to all admins.
pub(crate) async fn notify_admin_with_kb(bot: &Bot, text: &str, kb: InlineKeyboardMarkup) {
    send_to_admins(bot, text, Some(kb)).await;
}

/// Fan a MarkdownV2 message out to every configured admin chat. A failure to
/// one admin is logged but never blocks delivery to the others.
async fn send_to_admins(bot: &Bot, text: &str, kb: Option<InlineKeyboardMarkup>) {
    let config = get_config();
    if config.admin_telegram_ids.is_empty() {
        log::warn!("No admin telegram IDs set, skipping notification");
        return;
    }
    for &id in &config.admin_telegram_ids {
        let res = match &kb {
            Some(kb) => {
                bot.send_message(ChatId(id), text)
                    .parse_mode(ParseMode::MarkdownV2)
                    .reply_markup(kb.clone())
                    .await
            }
            None => {
                bot.send_message(ChatId(id), text)
                    .parse_mode(ParseMode::MarkdownV2)
                    .await
            }
        };
        if let Err(e) = res {
            log::error!("Failed to notify admin {}: {}", id, e);
        }
    }
}
