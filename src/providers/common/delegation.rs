//! `provider_module!` — generates a provider's entire `#[async_trait] impl
//! ProviderModule` block.
//!
//! Why the whole block and not just the repeated methods: `#[async_trait]`
//! rewrites every `async fn` in the impl it is attached to, but it does NOT
//! expand bang-macros inside the block first — so a macro that only emits the
//! shared methods inside a hand-written `#[async_trait] impl` never gets
//! desugared and fails to compile. Emitting the full annotated impl sidesteps
//! that: the macro expands first, then `#[async_trait]` processes its output.
//!
//! The six admin-delegation methods (`matches_callback`, `handle_callback`,
//! `handle_text_input`, `clear_state`, `active_account_count`,
//! `render_server_status`) are identical in every provider and are emitted by
//! the macro, resolving against the invoking module's own `admin::` (and
//! `buttons::` via the `menu_button` expr). The genuinely provider-specific
//! parts — the `list_active_accounts` and `get_provider_client` bodies — are
//! passed in as blocks, with their parameter idents supplied by the caller so
//! macro hygiene lets the body refer to them.
//!
//! `count_fn` exists because `manual`'s admin module names its counter
//! `active_count` while every other provider uses `active_account_count`.
macro_rules! provider_module {
    (
        $struct_:ident {
            provider_type: $pt:literal,
            display_name: $dn:literal,
            menu_button: ($label:literal, $cb:expr),
            count_fn: $count_fn:ident,
            list_active_accounts($lpool:ident) $list:block
            get_provider_client($gpool:ident, $acct:ident) $get:block
        }
    ) => {
        #[async_trait]
        impl ProviderModule for $struct_ {
            fn provider_type(&self) -> &'static str {
                $pt
            }

            fn display_name(&self) -> &'static str {
                $dn
            }

            async fn list_active_accounts(&self, $lpool: &SqlitePool)
                -> Result<Vec<(ProviderAccount, Box<dyn CloudProvider>)>, sqlx::Error>
            $list

            fn menu_button(&self) -> InlineKeyboardButton {
                InlineKeyboardButton::callback($label, $cb)
            }

            fn matches_callback(&self, data: &str) -> bool {
                admin::matches_callback(data)
            }

            async fn handle_callback(&self, bot: &Bot, cq: &CallbackQuery, pool: &SqlitePool, data: &str, telegram_id: i64) {
                admin::handle_callback(bot, cq, pool, data, telegram_id).await;
            }

            async fn handle_text_input(&self, bot: &Bot, msg: &Message, pool: &SqlitePool, telegram_id: i64) -> bool {
                admin::handle_text_input(bot, msg, pool, telegram_id).await
            }

            async fn clear_state(&self, telegram_id: i64) {
                admin::clear_state(telegram_id).await;
            }

            async fn active_account_count(&self, pool: &SqlitePool) -> usize {
                admin::$count_fn(pool).await
            }

            async fn render_server_status(&self, pool: &SqlitePool) -> String {
                admin::render_server_status(pool).await
            }

            async fn get_provider_client(&self, $gpool: &SqlitePool, $acct: i64)
                -> Result<Box<dyn CloudProvider>, String>
            $get
        }
    };
}

pub(crate) use provider_module;
