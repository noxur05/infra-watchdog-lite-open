use async_trait::async_trait;
use sqlx::SqlitePool;
use teloxide::prelude::*;
use teloxide::types::{CallbackQuery, InlineKeyboardButton, Message};

use crate::provider::CloudProvider;

pub mod common;
pub mod selectel;

pub struct ProviderAccount {
    pub id: i64,
    pub name: String,
    pub location: String,
}

#[async_trait]
pub trait ProviderModule: Send + Sync {
    fn provider_type(&self) -> &'static str;
    fn display_name(&self) -> &'static str;

    // Provisioner
    async fn list_active_accounts(&self, pool: &SqlitePool)
        -> Result<Vec<(ProviderAccount, Box<dyn CloudProvider>)>, sqlx::Error>;

    // Admin UI
    fn menu_button(&self) -> InlineKeyboardButton;
    fn matches_callback(&self, data: &str) -> bool;
    async fn handle_callback(&self, bot: &Bot, cq: &CallbackQuery, pool: &SqlitePool, data: &str, telegram_id: i64);
    async fn handle_text_input(&self, bot: &Bot, msg: &Message, pool: &SqlitePool, telegram_id: i64) -> bool;
    async fn clear_state(&self, telegram_id: i64);
    async fn active_account_count(&self, pool: &SqlitePool) -> usize;
    async fn render_server_status(&self, pool: &SqlitePool) -> String;

    /// Build a provider client for a specific account, so shared code can call delete_server() etc.
    async fn get_provider_client(&self, pool: &SqlitePool, account_id: i64)
        -> Result<Box<dyn CloudProvider>, String>;
}

pub fn all() -> Vec<Box<dyn ProviderModule>> {
    vec![
        Box::new(selectel::SelectelProvider),
    ]
}
