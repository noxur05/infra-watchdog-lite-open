pub mod client;
pub mod db;
pub mod admin;
pub mod buttons;
pub mod hooks;

use async_trait::async_trait;
use sqlx::SqlitePool;
use teloxide::prelude::*;
use teloxide::types::{CallbackQuery, InlineKeyboardButton, Message};

use crate::provider::CloudProvider;
use crate::providers::common::delegation::provider_module;
use super::{ProviderAccount, ProviderModule};

pub struct SelectelProvider;

provider_module! {
    SelectelProvider {
        provider_type: "selectel",
        display_name: "Selectel",
        menu_button: ("☁️ Selectel Accounts", buttons::CB_SELECTEL_ACCOUNTS),
        count_fn: active_account_count,
        list_active_accounts(pool) {
            let accounts = db::list_active_accounts(pool).await?;
            Ok(accounts.into_iter().map(|a| {
                let provider = client::SelectelClient::new(
                    a.username.clone(),
                    a.password.clone(),
                    a.account_id.clone(),
                    a.project_id.clone(),
                    a.region.clone(),
                );
                (
                    ProviderAccount { id: a.id, name: a.name, location: a.region },
                    Box::new(provider) as Box<dyn CloudProvider>,
                )
            }).collect())
        }
        get_provider_client(pool, account_id) {
            let account = db::get_account(pool, account_id).await
                .map_err(|e| e.to_string())?
                .ok_or_else(|| "Selectel account not found".to_string())?;
            Ok(Box::new(client::SelectelClient::new(
                account.username, account.password,
                account.account_id, account.project_id, account.region,
            )))
        }
    }
}
