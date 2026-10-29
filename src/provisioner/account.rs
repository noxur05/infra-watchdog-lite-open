//! `AccountProvisioner` — the per-(provider, account) state machine. One task
//! per active account; spawned by the manager loop in `super::manager`.
//!
//! State machine over the single active `servers` row:
//!   `none → creating → installing → on (not deployed) → on (deployed) → monitoring`
//!
//! Branches into `provision_new_server`, `poll_server_status`, `test_and_deploy`,
//! `monitor_server`, `delete_and_cleanup`. The error-notification dedup is
//! in-memory only (lost on restart); the Selectel warm-pool state lives in the
//! `server_floating_ips` table and survives restarts. The `CountBased`
//! rate-limit cooldown is restart-safe via `db::get_cooldown_remaining`.

use std::sync::Arc;
use std::time::Duration;

use sqlx::SqlitePool;
use teloxide::prelude::*;
use tokio::sync::{watch, Notify};

use crate::config::get_config;
use crate::db;
use crate::notify::{notify_admin, notify_admin_with_kb};
use crate::provider::{CloudProvider, RateLimitTracking};
use crate::ssh::{
    fetch_test_urls, harden_server, setup_dnat_with_timeout, setup_ip_forwarding,
    ssh_curl_test, ssh_get_tx_bytes,
};
use crate::time_utils::minutes_since;
use crate::utils::{escape_md, ssh_user_for};

use super::errors::{categorize_provisioning_error, retry_keyboard};
use super::openvpn_sync::publish_front_ip;

// ── Per-Account Provisioner ──

pub(super) struct AccountProvisioner {
    provider_type: String,
    account_id: i64,
    account_name: String,
    location: String,
    provider: Box<dyn CloudProvider>,
    pool: SqlitePool,
    bot: Bot,
    shutdown: watch::Receiver<bool>,
    /// Admin-pressed "Retry now" signal; fires through `request_retry`.
    retry_notify: Arc<Notify>,
    /// Last actionable error category we notified the admin about, plus when.
    /// Used to dedupe notifications: same category does not re-notify until
    /// 24h elapses or the category changes.
    last_notified_error_category: Option<String>,
    last_notified_error_at: Option<chrono::DateTime<chrono::Utc>>,
}

const POLL_PENDING_SECS: u64 = 30;

impl AccountProvisioner {
    /// Sibling-only constructor — `manager.rs` is the only caller.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        provider_type: String,
        account_id: i64,
        account_name: String,
        location: String,
        provider: Box<dyn CloudProvider>,
        pool: SqlitePool,
        bot: Bot,
        shutdown: watch::Receiver<bool>,
        retry_notify: Arc<Notify>,
    ) -> Self {
        Self {
            provider_type,
            account_id,
            account_name,
            location,
            provider,
            pool,
            bot,
            shutdown,
            retry_notify,
            last_notified_error_category: None,
            last_notified_error_at: None,
        }
    }

    pub(super) async fn run(mut self) {
        log::info!("[{}/{}] provisioner started ({})",
            self.provider_type, self.account_id, self.account_name);

        // None = normal polling cadence. Some(d) = currently in rate-limit
        // cooldown, sleep for d before next cycle. Set by `provision_new_server`
        // when CountBased rate-limit cap is hit (or by the "no-IPv4" fallback
        // and TimeWeb's daily-IP-limit branch). Reset to None on every entry
        // to `run_cycle` so a successful cycle can resume the normal cadence.
        let mut cooldown_remaining: Option<Duration> = None;

        // Reconcile orphaned servers for this account on startup
        if let Err(e) = self.reconcile_servers().await {
            log::error!("[{}/{}] reconciliation error: {}",
                self.provider_type, self.account_id, e);
        }

        // Run first cycle immediately
        let r = self.run_cycle(&mut cooldown_remaining).await;
        self.handle_cycle_result(r).await;

        loop {
            let interval = match cooldown_remaining {
                Some(d) => d,
                None => self.next_interval().await,
            };

            tokio::select! {
                _ = tokio::time::sleep(interval) => {}
                _ = self.shutdown.changed() => {
                    log::info!("[{}/{}] provisioner received stop signal",
                        self.provider_type, self.account_id);
                    break;
                }
                _ = self.retry_notify.notified() => {
                    log::info!("{} retry-now signal received, breaking sleep + clearing cooldown",
                        self.log_prefix());
                    cooldown_remaining = None;
                }
            }

            let r = self.run_cycle(&mut cooldown_remaining).await;
            self.handle_cycle_result(r).await;
        }

        log::info!("[{}/{}] provisioner stopped ({})",
            self.provider_type, self.account_id, self.account_name);
    }

    fn log_prefix(&self) -> String {
        format!("[{}/{}]", self.provider_type, self.account_id)
    }

    /// Process a run_cycle outcome: log, then notify admin on actionable
    /// errors. Re-notify the same category only after 24h, or immediately
    /// if the error category changes. On a successful cycle, clear the
    /// "last notified" state and ping admin once that we recovered.
    async fn handle_cycle_result(&mut self, result: Result<(), String>) {
        match result {
            Ok(()) => {
                // Persist successful state for the admin UI's diagnose helper.
                if let Err(e) = db::record_account_success(
                    &self.pool, &self.provider_type, self.account_id,
                ).await {
                    log::warn!("{} record_account_success: {}", self.log_prefix(), e);
                }
                if let Some(prev) = self.last_notified_error_category.take() {
                    self.last_notified_error_at = None;
                    log::info!("{} recovered from previous error ({})", self.log_prefix(), prev);
                    notify_admin(&self.bot, &format!(
                        "✅ {} account {} \\({}\\) recovered from `{}`\\.",
                        escape_md(&self.provider_type),
                        self.account_id,
                        escape_md(&self.account_name),
                        escape_md(&prev),
                    )).await;
                }
            }
            Err(e) => {
                log::error!("{} cycle error: {}", self.log_prefix(), e);
                let Some((cat, label)) = categorize_provisioning_error(&e) else {
                    return;
                };
                // Persist before any notification dedup so the admin UI sees
                // every failure even if we don't ping for it.
                if let Err(db_err) = db::record_account_error(
                    &self.pool, &self.provider_type, self.account_id, cat, &e,
                ).await {
                    log::warn!("{} record_account_error: {}", self.log_prefix(), db_err);
                }
                let now = chrono::Utc::now();
                let should_notify = match (&self.last_notified_error_category, self.last_notified_error_at) {
                    (Some(prev), Some(t)) if prev == cat => {
                        (now - t).num_hours() >= 24
                    }
                    _ => true,
                };
                if should_notify {
                    let text = format!(
                        "⚠️ *{} account {} \\({}\\)* — {}\n\nError: `{}`\n\nFix the issue on the provider side, then tap *🔄 Retry now* below to skip the cooldown\\. Otherwise the bot will keep retrying on its own \\(re\\-notification only after 24h or on a different error\\)\\.",
                        escape_md(&self.provider_type),
                        self.account_id,
                        escape_md(&self.account_name),
                        escape_md(label),
                        escape_md(&e),
                    );
                    let kb = retry_keyboard(&self.provider_type, self.account_id);
                    notify_admin_with_kb(&self.bot, &text, kb).await;
                    self.last_notified_error_category = Some(cat.to_string());
                    self.last_notified_error_at = Some(now);
                }
            }
        }
    }

    /// Sleep that wakes early on the admin's "🔄 Retry now" signal or on
    /// graceful shutdown. Use this instead of bare `tokio::time::sleep`
    /// for any wait inside `test_and_deploy` / `monitor_server` etc. that
    /// lasts longer than ~30s — otherwise the admin can't unstick the
    /// account without restarting the whole bot.
    ///
    /// `&self` (not `&mut`) so the `&self` callers (`test_and_deploy`)
    /// can still use it; the watch receiver is cloned per call.
    async fn interruptible_sleep(&self, dur: Duration) {
        let mut shutdown = self.shutdown.clone();
        tokio::select! {
            _ = tokio::time::sleep(dur) => {}
            _ = self.retry_notify.notified() => {
                log::info!("{} sleep interrupted by retry-now signal", self.log_prefix());
            }
            _ = shutdown.changed() => {
                log::info!("{} sleep interrupted by shutdown signal", self.log_prefix());
            }
        }
    }

    async fn next_interval(&self) -> Duration {
        let config = get_config();
        if let Ok(Some(srv)) = db::get_active_server_for_account(
            &self.pool, &self.provider_type, self.account_id
        ).await {
            match srv.status.as_str() {
                "creating" | "installing" => return Duration::from_secs(POLL_PENDING_SECS),
                "on" if !srv.script_deployed => return Duration::from_secs(POLL_PENDING_SECS),
                _ => {}
            }
        }
        Duration::from_secs(config.check_interval_secs)
    }

    fn is_selectel(&self) -> bool {
        self.provider_type == "selectel"
    }

    /// SSH login user for this account's servers. Most providers' Ubuntu
    /// images allow root login; Gcore's image refuses root and the cloud-init
    /// password is for the `ubuntu` user.
    fn ssh_user(&self) -> &'static str {
        ssh_user_for(&self.provider_type)
    }

    /// Common SSH-test preamble: the server row's connection facts plus the
    /// current test-URL list. Errors when the row has no ipv4/root_pass yet
    /// (still provisioning).
    async fn ssh_preamble<'a>(
        &self, srv: &'a db::Server,
    ) -> Result<(&'a str, &'a str, &'static str, Vec<String>), String> {
        let ipv4 = srv.ipv4.as_deref().ok_or("no ipv4")?;
        let root_pass = srv.root_pass.as_deref().ok_or("no root_pass")?;
        Ok((ipv4, root_pass, self.ssh_user(), fetch_test_urls(&self.pool).await))
    }

    async fn run_cycle(&mut self, cooldown_remaining: &mut Option<Duration>) -> Result<(), String> {
        // Reset cooldown on every cycle. Only paths that actively hit the
        // rate-limit (or "server has no IPv4") set a Some(d) below. Without
        // this reset, a sticky cooldown from a previous cap-hit would keep the
        // outer loop sleeping forever even after a successful create.
        *cooldown_remaining = None;

        let server = db::get_active_server_for_account(
            &self.pool, &self.provider_type, self.account_id
        ).await.map_err(|e| e.to_string())?;

        match server {
            None => {
                if !db::get_ovpn_backend(&self.pool).await.is_configured() {
                    log::debug!("{} OpenVPN backend not configured (env or bot), skipping", self.log_prefix());
                    return Ok(());
                }
                log::info!("{} no active server, starting provisioning...", self.log_prefix());
                self.provision_new_server(cooldown_remaining).await?;
            }
            Some(srv) => {

                // If a previous remote-delete attempt failed, the row was kept
                // alive and the admin was pinged. Skip this server until they
                // act (or auto-retry after 24h, in case the failure was transient).
                if let Some(failed_at) = &srv.delete_failed_at {
                    if minutes_since(failed_at) < 24 * 60 {
                        log::debug!("{} server {} pending admin cleanup (delete failed at {}), skipping",
                            self.log_prefix(), srv.provider_server_id, failed_at);
                        return Ok(());
                    }
                    log::info!("{} retrying remote delete on {} after 24h",
                        self.log_prefix(), srv.provider_server_id);
                    let _ = self.delete_and_cleanup(&srv).await;
                    return Ok(());
                }

                match srv.status.as_str() {
                    "creating" | "installing" => {
                        self.poll_server_status(&srv).await?;
                    }
                    "on" if !srv.script_deployed => {
                        if srv.ipv4.is_none() {
                            log::warn!("{} server {} has no IPv4, deleting",
                                self.log_prefix(), srv.provider_server_id);
                            self.delete_and_cleanup(&srv).await?;
                            let rl = self.provider.rate_limit_config();
                            *cooldown_remaining =
                                Some(Duration::from_secs(rl.cooldown_hours as u64 * 3600));
                            return Ok(());
                        }
                        self.test_and_deploy(&srv).await?;
                    }
                    "on" if srv.script_deployed => {
                        self.monitor_server(&srv).await?;
                    }
                    "error" | "off" => {
                        log::warn!("{} server {} is {}, deleting",
                            self.log_prefix(), srv.provider_server_id, srv.status);
                        self.delete_and_cleanup(&srv).await?;
                    }
                    // Provider created (and is billing for) the server, but
                    // flagged it as unpaid/blocked. Without this arm it would
                    // sit in the catch-all forever — a billed orphan. Delete
                    // it to stop the charge, alert the admin with a retry
                    // button, and back off so we don't mint a fresh orphan
                    // every cycle while the balance is still zero.
                    "no_paid" | "blocked" | "permanent_blocked" => {
                        log::error!("{} server {} status='{}' (account unpaid/blocked), deleting + backing off",
                            self.log_prefix(), srv.provider_server_id, srv.status);
                        self.delete_and_cleanup(&srv).await?;
                        notify_admin_with_kb(&self.bot, &format!(
                            "⚠️ *{} account {} \\({}\\)* — server status `{}`\\.\n\n\
                             The server was created \\(and billed\\) but the provider flagged it \
                             *unpaid/blocked*\\. It's been deleted to stop the charge\\.\n\n\
                             Top up the account balance, then tap *🔄 Retry now*\\.",
                            escape_md(&self.provider_type), self.account_id,
                            escape_md(&self.account_name), escape_md(&srv.status),
                        ), retry_keyboard(&self.provider_type, self.account_id)).await;
                        *cooldown_remaining = Some(Duration::from_secs(6 * 3600));
                        return Ok(());
                    }
                    _ => {
                        log::debug!("{} server {} in state {}",
                            self.log_prefix(), srv.provider_server_id, srv.status);
                    }
                }
            }
        }

        Ok(())
    }

    /// Before creating a new server, reap any server the provider is hosting
    /// (and billing for) that the bot isn't tracking. This closes the leak
    /// where a prior create returned `no_paid`/errored *after* the provider
    /// had already provisioned the VM + IP: the bot never recorded it, so the
    /// next cycle would stack a fresh billed server on top of the orphan.
    ///
    /// Scoped to single-server-per-account providers (TimeWeb, VDSina, …) whose
    /// `list_servers(prefix)` is account-global and unambiguous. Multi-region
    /// providers (Vultr/Selectel/Gcore) share one API key across regions, so a
    /// blind prefix sweep here could reap a sibling region's server; they rely
    /// on the startup `reconcile_servers` (which has the region guard) instead.
    ///
    /// Best-effort: a failed list, or a delete that 404s (server already gone),
    /// is logged and we proceed to create anyway — the goal is "don't leave a
    /// leak behind", not "block provisioning on a flaky cleanup".
    async fn provision_new_server(&self, cooldown_remaining: &mut Option<Duration>) -> Result<(), String> {
        let rl = self.provider.rate_limit_config();

        // For CountBased tracking, check the rate-limit cooldown BEFORE creating.
        // `get_cooldown_remaining` derives the remaining time from the DB:
        // `oldest_trial_in_window + cooldown_hours - now`. This is restart-safe
        // — a process restart mid-cooldown resumes the same end time instead
        // of starting a fresh `cooldown_hours` sleep, which had been letting
        // a deploy effectively reset the cooldown and burn extra trials.
        if let RateLimitTracking::CountBased = rl.tracking {
            if let Some(remaining) = db::get_cooldown_remaining(
                &self.pool, &self.provider_type, self.account_id,
                rl.cooldown_hours, rl.max_free_trials, rl.cooldown_anchor,
            ).await.map_err(|e| e.to_string())? {
                let total_mins = remaining.as_secs() / 60;
                let h = total_mins / 60;
                let m = total_mins % 60;
                log::warn!("{} hit {} trials cap, cooldown remaining {}h{:02}m (DB-derived)",
                    self.log_prefix(), rl.max_free_trials, h, m);
                notify_admin(&self.bot, &format!(
                    "{} account {} \\({}\\) at {} trial cap\\. Cooldown remaining: {}h{:02}m\\.",
                    escape_md(&self.provider_type), self.account_id,
                    escape_md(&self.account_name), rl.max_free_trials, h, m,
                )).await;
                *cooldown_remaining = Some(remaining);
                return Ok(());
            }
        }

        let result = match self.provider.create_server(&self.location).await {
            Ok(r) => r,
            Err(e) if e.is_auth() => {
                notify_admin(&self.bot, &format!(
                    "{} auth error for account {} \\({}\\)\\!\nCredentials may be invalid\\.",
                    escape_md(&self.provider_type), self.account_id,
                    escape_md(&self.account_name)
                )).await;
                return Err(e.to_string());
            }
            // Provider signalled a daily-quota / rate-limit at create time.
            // Only ApiSignal providers (TimeWeb, Hostman) take the silent
            // back-off here — CountBased pool
            // providers (Vultr/UpCloud) return RateLimit on a transient HTTP
            // 429 and MUST keep ticking so deployed pool members stay
            // monitored, so they fall through to the generic Err path instead.
            // Recorded (not notified), like the TimeWeb ApiSignal path: a
            // self-clearing quota pause shouldn't carry a "fix it + retry" alert.
            Err(e) if e.is_rate_limit()
                && matches!(rl.tracking, RateLimitTracking::ApiSignal) => {
                log::warn!("{} create rate-limited, backing off {}h: {}",
                    self.log_prefix(), rl.cooldown_hours, e);
                if let Err(e2) = db::record_account_error(
                    &self.pool, &self.provider_type, self.account_id,
                    "ip_daily_limit",
                    "Provider IP/quota limit reached at create",
                ).await {
                    log::warn!("{} record_account_error(ip_daily_limit): {}",
                        self.log_prefix(), e2);
                }
                *cooldown_remaining =
                    Some(Duration::from_secs(rl.cooldown_hours as u64 * 3600));
                return Ok(());
            }
            Err(e) => return Err(e.to_string()),
        };

        log::info!("{} created server {}", self.log_prefix(), result.server_id);

        // For ApiSignal tracking, check warning after creation
        if let RateLimitTracking::ApiSignal = rl.tracking {
            if let Some(w) = &result.warning {
                if w.contains("floating_ip_daily_limit_exceeded") {
                    log::warn!("{} IP daily limit exceeded, deleting server {}",
                        self.log_prefix(), result.server_id);
                    if let Err(e) = self.provider.delete_server(&result.server_id).await {
                        log::error!("Failed to delete no-IP server {}: {}", result.server_id, e);
                    }
                    // ApiSignal cap-hit (TimeWeb): no DB-derived "trial window"
                    // exists for this provider, so fall back to the configured
                    // cooldown_hours. This isn't restart-safe like CountBased,
                    // but daily-IP-limit cap-hits are rare and the worst case
                    // is one extra cooldown sleep after a deploy — not extra
                    // billed trials.
                    *cooldown_remaining =
                        Some(Duration::from_secs(rl.cooldown_hours as u64 * 3600));
                    // Persist the cap-hit as a recorded reason so the admin
                    // server list shows "Last cycle failed Xm ago — IP daily
                    // limit exhausted on provider" instead of a bare
                    // "⚪ No server". `diagnose::diagnose_no_server` reads this.
                    if let Err(e) = db::record_account_error(
                        &self.pool, &self.provider_type, self.account_id,
                        "ip_daily_limit",
                        "TimeWeb floating-IP daily limit exhausted",
                    ).await {
                        log::warn!("{} record_account_error(ip_daily_limit): {}",
                            self.log_prefix(), e);
                    }
                    return Ok(());
                }
            }
        }

        let db_id = db::create_server(
            &self.pool, &self.provider_type, self.account_id, &result.server_id,
        ).await.map_err(|e| e.to_string())?;

        // Stash the region/location used to create this server. Sync time
        // looks this up via `region_to_country` to label the user-facing host
        // remark — without this, every node would publish as "🌐 Unknown".
        // Non-fatal: a failed write just degrades the user-facing label, the
        // server itself is fine.
        if let Err(e) = db::set_server_region(&self.pool, db_id, &self.location).await {
            log::warn!("{} set_server_region({}, {:?}) failed: {}",
                self.log_prefix(), db_id, self.location, e);
        }

        // If the provider returned a password at creation time (Beget), store it immediately
        if let Some(pass) = &result.root_pass {
            db::update_server_info(&self.pool, db_id, None, Some(pass), "creating")
                .await.map_err(|e| e.to_string())?;
        }

        Ok(())
    }

    async fn poll_server_status(&self, srv: &db::Server) -> Result<(), String> {
        // Stuck-create guard: if the provider's API has kept the server in
        // `creating`/`installing` for >30 min from row creation, assume the
        // provider's orchestrator is wedged (we've seen this on TimeWeb)
        // and force a recycle. Without this, `next_interval` polls every
        // POLL_PENDING_SECS=30s forever and the server slot is held open.
        if matches!(srv.status.as_str(), "creating" | "installing")
            && minutes_since(&srv.created_at) > 30
        {
            log::warn!("{} server {} stuck in '{}' for >30 min, force-recycling",
                self.log_prefix(), srv.provider_server_id, srv.status);
            notify_admin(&self.bot, &format!(
                "Server `{}` stuck in `{}` for >30 min on {} account {} \\({}\\)\\. Recycling\\.",
                escape_md(&srv.provider_server_id), escape_md(&srv.status),
                escape_md(&self.provider_type), self.account_id,
                escape_md(&self.account_name),
            )).await;
            self.delete_and_cleanup(srv).await?;
            return Ok(());
        }

        let info = match self.provider.get_server(&srv.provider_server_id).await {
            Ok(i) => i,
            Err(e) if e.is_auth() => {
                notify_admin(&self.bot, &format!(
                    "{} auth error for account {} \\({}\\)\\!",
                    escape_md(&self.provider_type), self.account_id,
                    escape_md(&self.account_name)
                )).await;
                return Err(e.to_string());
            }
            Err(e) => return Err(e.to_string()),
        };

        log::debug!("{} server {} status: {}, ipv4: {:?}",
            self.log_prefix(), srv.provider_server_id, info.status, info.ipv4);

        db::update_server_info(
            &self.pool,
            srv.id,
            info.ipv4.as_deref(),
            info.root_pass.as_deref(),
            &info.status,
        ).await.map_err(|e| e.to_string())?;

        // Selectel: build the warm floating-IP pool once the server is ACTIVE.
        // This allocates POOL_SIZE floating IPs (one per fixed IP on the port)
        // and points servers.ipv4/floating_ip_id at the active one, giving the
        // server a public IP so SSH/deploy can proceed. Per-IP curl testing +
        // failover happen later in test_and_deploy / monitor_server.
        if self.is_selectel() && info.status == "on" && srv.floating_ip_id.is_none() {
            let ctx = self.selectel_ctx();
            crate::providers::selectel::hooks::setup_pool(&ctx, srv).await?;
        }

        Ok(())
    }

    /// Apply first-contact hardening (UFW + fail2ban kill, sshd MaxStartups,
    /// BBR) once per server. Skipped if already hardened. Failure here is
    /// non-fatal — log and let the caller proceed. The caller's own SSH ops
    /// may also fail (driving the natural recycle path) if the server is
    /// genuinely unreachable.
    async fn ensure_hardened(&self, srv: &db::Server) {
        if srv.hardened { return; }
        let Some(ipv4) = srv.ipv4.as_deref() else { return };
        let Some(root_pass) = srv.root_pass.as_deref() else { return };
        let ssh_user = self.ssh_user();
        match harden_server(ipv4, root_pass, ssh_user).await {
            Ok(()) => {
                if let Err(e) = db::mark_server_hardened(&self.pool, srv.id).await {
                    log::warn!("{} mark_server_hardened({}): {}",
                        self.log_prefix(), srv.id, e);
                } else {
                    log::info!("{} server {} hardened (UFW/fail2ban killed, MaxStartups+BBR set)",
                        self.log_prefix(), srv.provider_server_id);
                }
            }
            Err(e) => {
                // Don't mark hardened — next cycle retries. Avoid spamming
                // admin notifications; the failure is self-recovering.
                log::warn!("{} harden_server({}) failed (will retry next cycle): {}",
                    self.log_prefix(), srv.provider_server_id, e);
            }
        }
    }

    async fn test_and_deploy(&mut self, srv: &db::Server) -> Result<(), String> {
        // If the active floating IP is unreachable from the bot host (Selectel
        // per-IP routing gaps), switch active to a reachable pool member and let
        // the next cycle deploy through it. Not published yet, so no republish.
        if self.is_selectel() {
            let ctx = self.selectel_ctx();
            match crate::providers::selectel::hooks::ensure_active_ip_reachable(&ctx, srv).await {
                Ok(true) => return Ok(()),
                Ok(false) => {}
                Err(e) => log::warn!("{} ensure_active_ip_reachable: {}", self.log_prefix(), e),
            }
        }

        let (ipv4, root_pass, ssh_user, test_urls) = self.ssh_preamble(srv).await?;

        // Run first-contact hardening BEFORE the first real SSH workload.
        // Skipped if already done; failure leaves hardened=0 for retry next cycle.
        self.ensure_hardened(srv).await;

        match ssh_curl_test(ipv4, root_pass, ssh_user, &test_urls).await {
            Ok(true) => {
                db::reset_ssh_fails(&self.pool, srv.id).await.ok();
                log::info!("{} server {} reachable from {}",
                    self.log_prefix(), srv.provider_server_id, ipv4);
            }
            Ok(false) => {
                log::warn!("{} server {} curl test failed, deleting",
                    self.log_prefix(), srv.provider_server_id);
                self.delete_and_cleanup(srv).await?;
                return Ok(());
            }
            Err(e) => {
                // Selectel: cloud-init needs time to set password after ACTIVE.
                // Don't delete on first SSH failure — let the next cycle retry.
                if self.is_selectel() {
                    log::info!("{} server {} SSH not ready yet (cloud-init?): {}, will retry",
                        self.log_prefix(), srv.provider_server_id, e);
                    return Ok(());
                }
                log::warn!("{} server {} SSH/curl error: {}, deleting",
                    self.log_prefix(), srv.provider_server_id, e);
                self.delete_and_cleanup(srv).await?;
                return Ok(());
            }
        }

        // Blanket DNAT: forward ALL ports (except SSH 22) to the fixed
        // OpenVPN-AS backend, port-preserving. setup_ip_forwarding enables
        // ip_forward + MASQUERADE + installs iptables-persistent; the DNAT
        // step adds the catch-all PREROUTING rules and saves them.
        // Resolved fresh each deploy so a backend IP change in the bot takes
        // effect on the next (re-)deploy without a restart.
        let backend = db::get_ovpn_backend(&self.pool).await.ip;
        if backend.is_empty() {
            return Err("OpenVPN backend IP not set (env or 🖥 Main Server screen) — cannot deploy DNAT".into());
        }
        setup_ip_forwarding(ipv4, root_pass, ssh_user).await?;
        db::mark_server_ip_setup(&self.pool, srv.id).await.map_err(|e| e.to_string())?;
        setup_dnat_with_timeout(ipv4, root_pass, ssh_user, &backend).await?;

        db::mark_server_deployed(&self.pool, srv.id).await.map_err(|e| e.to_string())?;
        log::info!("{} server {} deployed", self.log_prefix(), srv.provider_server_id);

        // Selectel: do the first warm-pool pass now — configure the extra fixed
        // IPs on the NIC, curl-test each pool member, and (if the freshly-picked
        // active turns out blocked) fail over to a good spare. The publish below
        // then points the backend at whatever ended up active in the DB.
        if self.is_selectel() {
            let ctx = self.selectel_ctx();
            if let Err(e) = crate::providers::selectel::hooks::manage_pool(&ctx, srv, &test_urls).await {
                log::warn!("{} initial pool pass: {}", self.log_prefix(), e);
            }
        }

        // Publish the new front IP to the OpenVPN-AS backend (point host.name at
        // it + restart). Mark synced on success; monitor_server retries on fail.
        match publish_front_ip(&self.pool).await {
            Ok(()) => {
                db::mark_server_published(&self.pool, srv.id).await.ok();
            }
            Err(e) => {
                log::error!("OpenVPN publish failed after deploy ({}): {}", self.log_prefix(), e);
                notify_admin(&self.bot, &format!(
                    "OpenVPN publish failed after deploy\\!\nAccount: {} \\({}\\)\nError: {}",
                    self.account_id, escape_md(&self.account_name), escape_md(&e.to_string())
                )).await;
            }
        }

        notify_admin(&self.bot, &format!(
            "Server {} deployed\\.\nProvider: {}\nAccount: {} \\({}\\)\nIP: `{}`\nDNAT → {}",
            escape_md(&srv.provider_server_id), escape_md(&self.provider_type),
            self.account_id, escape_md(&self.account_name),
            ipv4, escape_md(&backend),
        )).await;

        Ok(())
    }

    async fn monitor_server(&mut self, srv: &db::Server) -> Result<(), String> {
        // Post-deploy ingress failover: if the active IP became unreachable from
        // the bot host, switch to a reachable pool member and republish (so the
        // OVPN-AS backend points at an IP we can actually manage). Next cycle
        // proceeds with the fresh row.
        if self.is_selectel() {
            let ctx = self.selectel_ctx();
            match crate::providers::selectel::hooks::ensure_active_ip_reachable(&ctx, srv).await {
                Ok(true) => {
                    db::unmark_server_published(&self.pool, srv.id).await.ok();
                    return Ok(());
                }
                Ok(false) => {}
                Err(e) => log::warn!("{} ensure_active_ip_reachable: {}", self.log_prefix(), e),
            }
        }

        let (ipv4, root_pass, ssh_user, test_urls) = self.ssh_preamble(srv).await?;

        // Backfill hardening for legacy servers (provisioned before migration
        // 034). Idempotent + cheap when hardened=1 (early return).
        self.ensure_hardened(srv).await;

        // Selectel: check outgoing traffic (3TB free limit, delete at 2.9TB).
        if self.is_selectel() && srv.script_deployed {
            match ssh_get_tx_bytes(ipv4, root_pass, ssh_user).await {
                Ok(tx_total) => {
                    let ctx = self.selectel_ctx();
                    match crate::providers::selectel::hooks::check_traffic(&ctx, srv, tx_total).await {
                        Ok(true) => {
                            drop(ctx);
                            self.delete_and_cleanup(srv).await?;
                            let wait_secs = crate::time_utils::seconds_until_next_month();
                            log::info!("{} waiting {}h until next month", self.log_prefix(), wait_secs / 3600);
                            self.interruptible_sleep(Duration::from_secs(wait_secs)).await;
                            return Ok(());
                        }
                        Ok(false) => {}
                        Err(e) => log::warn!("{} selectel traffic check: {}", self.log_prefix(), e),
                    }
                }
                Err(e) => log::debug!("{} could not read tx_bytes: {}", self.log_prefix(), e),
            }
        }

        // Retry the OpenVPN publish if it failed during deploy (front_published
        // is reused as the "front IP published to the backend" flag).
        if !srv.front_published {
            log::info!("{} front IP not published for server {}, retrying...",
                self.log_prefix(), srv.provider_server_id);
            match publish_front_ip(&self.pool).await {
                Ok(()) => {
                    db::mark_server_published(&self.pool, srv.id).await.ok();
                    log::info!("{} OpenVPN publish succeeded", self.log_prefix());
                }
                Err(e) => {
                    log::error!("{} OpenVPN publish retry failed: {}", self.log_prefix(), e);
                    notify_admin(&self.bot, &format!(
                        "OpenVPN publish failed\\!\nAccount: {} \\({}\\)\nError: {}",
                        self.account_id, escape_md(&self.account_name), escape_md(&e.to_string())
                    )).await;
                }
            }
        }

        // Selectel: per-IP warm-pool management (test every pool IP, replace
        // blocked spares, fail over the active, recycle the batch). This
        // supersedes the single-IP curl-test/rotate path below.
        if self.is_selectel() {
            return self.monitor_selectel_pool(srv, &test_urls).await;
        }

        // SSH + curl test
        match ssh_curl_test(ipv4, root_pass, ssh_user, &test_urls).await {
            Ok(true) => {
                db::reset_curl_fails(&self.pool, srv.id).await.ok();
                db::reset_ssh_fails(&self.pool, srv.id).await.ok();
            }
            Ok(false) => {
                let fails = db::increment_curl_fails(&self.pool, srv.id).await.map_err(|e| e.to_string())?;
                log::warn!("{} server {} curl fail #{}",
                    self.log_prefix(), srv.provider_server_id, fails);

                if fails >= 3 {
                    // NOTE: Selectel is handled entirely by monitor_selectel_pool
                    // (early return above), so this generic path never runs for
                    // it — and must NOT call the legacy single-IP rotate, which
                    // would conflict with the warm pool's role tracking.
                    log::error!("{} server {} 3 curl fails, replacing",
                        self.log_prefix(), srv.provider_server_id);
                    notify_admin(&self.bot, &format!(
                        "Server {} dead \\(3 curl fails\\)\\. Replacing\\.\\.\\.\nAccount: {} \\({}\\)",
                        escape_md(&srv.provider_server_id), self.account_id, escape_md(&self.account_name)
                    )).await;
                    self.delete_and_cleanup(srv).await?;
                }
            }
            Err(e) => {
                let fails = db::increment_ssh_fails(&self.pool, srv.id).await.map_err(|e2| e2.to_string())?;
                log::warn!("{} server {} SSH fail #{}: {}",
                    self.log_prefix(), srv.provider_server_id, fails, e);

                if fails >= 3 && srv.ssh_fail_notified_at.is_none() {
                    let msg = format!(
                        "SSH failed 3x on server {}\\!\nProvider: {}\nAccount: {} \\({}\\)\nIP: `{}`\nError: {}\n\nWaiting 20 min\\.\\.\\.",
                        escape_md(&srv.provider_server_id), escape_md(&self.provider_type),
                        self.account_id, escape_md(&self.account_name),
                        ipv4, escape_md(&e)
                    );
                    notify_admin(&self.bot, &msg).await;
                    db::set_ssh_fail_notified(&self.pool, srv.id).await.ok();
                }

                if fails >= 3 {
                    if let Some(notified_at) = &srv.ssh_fail_notified_at {
                        if minutes_since(notified_at) >= 20 {
                            log::error!("{} server {} SSH dead 20+ min, replacing",
                                self.log_prefix(), srv.provider_server_id);
                            notify_admin(&self.bot, &format!(
                                "No response for server {} after 20 min\\. Replacing\\.\nAccount: {} \\({}\\)",
                                escape_md(&srv.provider_server_id), self.account_id, escape_md(&self.account_name)
                            )).await;
                            self.delete_and_cleanup(srv).await?;
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Selectel monitor path: run one warm-pool management pass and act on the
    /// outcome. `manage_pool` does all per-IP testing + slot replacement +
    /// failover/recycle internally; here we only (a) republish the front when
    /// the active IP changed and (b) escalate via the shared SSH-failure path
    /// when the server is unreachable.
    async fn monitor_selectel_pool(&mut self, srv: &db::Server, test_urls: &[String]) -> Result<(), String> {
        use crate::providers::selectel::hooks::{self, PoolOutcome};

        let ctx = self.selectel_ctx();
        let result = hooks::manage_pool(&ctx, srv, test_urls).await;
        drop(ctx);

        match result {
            Ok(outcome) => {
                db::reset_ssh_fails(&self.pool, srv.id).await.ok();
                match outcome {
                    PoolOutcome::Healthy => {}
                    PoolOutcome::FailedOver | PoolOutcome::Recycled => {
                        self.republish_front(srv).await;
                    }
                    PoolOutcome::Stuck => {
                        notify_admin(&self.bot, &format!(
                            "Selectel pool stuck for server {} — could not recover a working IP\\.\nAccount: {} \\({}\\)",
                            escape_md(&srv.provider_server_id),
                            self.account_id, escape_md(&self.account_name),
                        )).await;
                    }
                }
                Ok(())
            }
            // manage_pool only errors when EVERY pool test SSH-failed → treat as
            // the server being unreachable and run the standard SSH escalation.
            Err(e) => self.handle_ssh_failure(srv, &e).await,
        }
    }

    /// Republish the current active front IP to the OpenVPN-AS backend and track
    /// the published flag. Called after the warm pool changes the active IP.
    async fn republish_front(&self, srv: &db::Server) {
        match publish_front_ip(&self.pool).await {
            Ok(()) => {
                db::mark_server_published(&self.pool, srv.id).await.ok();
                log::info!("{} republished front IP after pool change", self.log_prefix());
            }
            Err(e) => {
                log::error!("{} republish after pool change failed: {}", self.log_prefix(), e);
                db::unmark_server_published(&self.pool, srv.id).await.ok();
                notify_admin(&self.bot, &format!(
                    "OpenVPN publish failed after IP change\\!\nAccount: {} \\({}\\)\nError: {}",
                    self.account_id, escape_md(&self.account_name), escape_md(&e.to_string())
                )).await;
            }
        }
    }

    /// Shared SSH-failure escalation: count consecutive SSH failures, alert
    /// once at 3, and replace the server if it stays dead for 20+ minutes.
    /// Mirrors the non-Selectel `Err` arm of the curl test.
    async fn handle_ssh_failure(&self, srv: &db::Server, err: &str) -> Result<(), String> {
        let fails = db::increment_ssh_fails(&self.pool, srv.id).await.map_err(|e| e.to_string())?;
        log::warn!("{} server {} SSH fail #{}: {}",
            self.log_prefix(), srv.provider_server_id, fails, err);

        if fails >= 3 && srv.ssh_fail_notified_at.is_none() {
            let msg = format!(
                "SSH failed 3x on server {}\\!\nProvider: {}\nAccount: {} \\({}\\)\nIP: `{}`\nError: {}\n\nWaiting 20 min\\.\\.\\.",
                escape_md(&srv.provider_server_id), escape_md(&self.provider_type),
                self.account_id, escape_md(&self.account_name),
                srv.ipv4.as_deref().unwrap_or("?"), escape_md(err)
            );
            notify_admin(&self.bot, &msg).await;
            db::set_ssh_fail_notified(&self.pool, srv.id).await.ok();
        }

        if fails >= 3 {
            if let Some(notified_at) = &srv.ssh_fail_notified_at {
                if minutes_since(notified_at) >= 20 {
                    log::error!("{} server {} SSH dead 20+ min, replacing",
                        self.log_prefix(), srv.provider_server_id);
                    notify_admin(&self.bot, &format!(
                        "No response for server {} after 20 min\\. Replacing\\.\nAccount: {} \\({}\\)",
                        escape_md(&srv.provider_server_id), self.account_id, escape_md(&self.account_name)
                    )).await;
                    self.delete_and_cleanup(srv).await?;
                }
            }
        }
        Ok(())
    }

    /// Build the context passed to Selectel hook functions.
    fn selectel_ctx(&self) -> crate::providers::selectel::hooks::HookCtx {
        crate::providers::selectel::hooks::HookCtx {
            pool: self.pool.clone(),
            bot: self.bot.clone(),
            account_id: self.account_id,
            account_name: self.account_name.clone(),
            log_prefix: self.log_prefix(),
        }
    }

    async fn delete_and_cleanup(&self, srv: &db::Server) -> Result<(), String> {
        // Selectel: release floating IP before deleting server
        if self.is_selectel() {
            let ctx = self.selectel_ctx();
            crate::providers::selectel::hooks::release_floating_ip(&ctx, srv).await;
        }

        // Try to delete on the provider. If the API call fails for any reason
        // OTHER than auth, leave the DB row intact so the bot stops trying to
        // re-delete on every cycle (which spams the admin and burns API quota)
        // and let an admin manually clean up via the deactivate+delete flow.
        if let Err(e) = self.provider.delete_server(&srv.provider_server_id).await {
            log::error!("Failed to delete {} server {}: {}",
                self.provider_type, srv.provider_server_id, e);

            // Stash the failure timestamp so monitor_server skips this row
            // until an admin intervenes (or, after 24h, the bot retries once).
            if let Err(e2) = db::set_delete_failed_at_now(&self.pool, srv.id).await {
                log::warn!("Could not set delete_failed_at on {}: {}", srv.id, e2);
            }

            let kind = if e.is_auth() { "auth error" } else { "API/network error" };
            notify_admin(&self.bot, &format!(
                "⚠️ Could not delete {} server `{}` \\({})\\.\n\
                 Account: {} \\({}\\)\n\
                 IP: `{}`\n\
                 The server may be orphaned on the provider\\. To clean up: open the {} account in admin, *Deactivate \\+ Delete servers*, then *Activate*\\.\n\
                 Bot will skip this row in the meantime\\.",
                escape_md(&self.provider_type),
                escape_md(&srv.provider_server_id),
                escape_md(kind),
                self.account_id,
                escape_md(&self.account_name),
                srv.ipv4.as_deref().unwrap_or("?"),
                escape_md(&self.provider_type),
            )).await;

            // Bubble up so the caller doesn't proceed as if the delete succeeded.
            return Err(format!("provider delete_server failed: {}", e));
        }

        db::soft_delete_server(&self.pool, srv.id).await.map_err(|e| e.to_string())?;

        if let Err(e) = cleanup_server(&self.pool, srv).await {
            log::error!("Publish cleanup after server delete failed: {}", e);
            notify_admin(&self.bot, &format!(
                "Publish cleanup failed after server delete\\!\nError: {}",
                escape_md(&e)
            )).await;
        }

        log::info!("{} server {} deleted and cleaned up",
            self.log_prefix(), srv.provider_server_id);
        Ok(())
    }

    async fn reconcile_servers(&self) -> Result<(), String> {
        let prefix = self.provider.server_name_prefix();
        let remote_servers = match self.provider.list_servers(Some(prefix)).await {
            Ok(s) => s,
            Err(e) if e.is_auth() => {
                notify_admin(&self.bot, &format!(
                    "{} auth error for account {} \\({}\\)\\!",
                    escape_md(&self.provider_type), self.account_id,
                    escape_md(&self.account_name)
                )).await;
                return Err(e.to_string());
            }
            Err(e) => return Err(e.to_string()),
        };

        let remote_ids: std::collections::HashSet<String> =
            remote_servers.iter().map(|s| s.id.clone()).collect();

        let db_servers = db::get_active_servers_for_account(
            &self.pool, &self.provider_type, self.account_id,
        ).await.map_err(|e| e.to_string())?;
        let db_provider_ids: std::collections::HashSet<String> =
            db_servers.iter().map(|s| s.provider_server_id.clone()).collect();

        // Remote servers not in DB → delete them (orphaned on this account)
        // For Vultr multi-region: skip servers whose region doesn't match ours
        // (they belong to another region's provisioner sharing the same API key)
        for rs in &remote_servers {
            if !db_provider_ids.contains(&rs.id) {
                if let Some(ref rs_region) = rs.region {
                    if rs_region != &self.location {
                        log::debug!("{} skipping orphan {} in region {} (ours: {})",
                            self.log_prefix(), rs.id, rs_region, self.location);
                        continue;
                    }
                }
                log::warn!("{} orphaned server {} ({}), deleting",
                    self.log_prefix(), rs.id, rs.name);
                if let Err(e) = self.provider.delete_server(&rs.id).await {
                    log::error!("Failed to delete orphaned server {}: {}", rs.id, e);
                }
            }
        }

        // DB servers whose provider ID is gone → soft-delete in DB and
        // republish so the OVPN-AS backend stops pointing at the dead IP.
        for ds in &db_servers {
            if !remote_ids.contains(&ds.provider_server_id) {
                log::warn!("{} DB server {} (provider_id={}) not on provider, soft-deleting",
                    self.log_prefix(), ds.id, ds.provider_server_id);
                db::soft_delete_server(&self.pool, ds.id).await.ok();
                if let Err(e) = cleanup_server(&self.pool, ds).await {
                    log::warn!("{} reconcile publish cleanup for server {}: {}",
                        self.log_prefix(), ds.id, e);
                }
            }
        }

        // Clean up orphaned resources (floating IPs for TimeWeb, no-op for others)
        let active_provider_ids: Vec<String> = remote_ids.intersection(&db_provider_ids)
            .cloned().collect();
        match self.provider.cleanup_resources(&active_provider_ids).await {
            Ok(n) if n > 0 => log::info!("{} cleaned up {} orphaned resources", self.log_prefix(), n),
            Ok(_) => {}
            Err(e) => log::warn!("{} failed to clean up resources: {}", self.log_prefix(), e),
        }

        log::info!("{} reconciliation done: {} remote, {} in DB",
            self.log_prefix(), remote_servers.len(), db_servers.len());
        Ok(())
    }
}

// ── Account server cleanup ──

/// Delete all active servers for a provider account from the provider API and DB.
/// Returns (deleted_count, failed_count).
/// If any deletions fail, returns early so the caller can decide whether to proceed.
pub async fn delete_account_servers(
    pool: &SqlitePool,
    provider_type: &str,
    account_id: i64,
    client: &dyn CloudProvider,
) -> (u32, u32) {
    let servers = match db::get_active_servers_for_account(pool, provider_type, account_id).await {
        Ok(s) => s,
        Err(e) => {
            log::error!("Failed to list servers for {}/{}: {}", provider_type, account_id, e);
            return (0, 0);
        }
    };

    let mut deleted = 0u32;
    let mut failed = 0u32;
    for srv in &servers {
        match client.delete_server(&srv.provider_server_id).await {
            Ok(()) => {
                db::soft_delete_server(pool, srv.id).await.ok();
                if let Err(e) = cleanup_server(pool, srv).await {
                    log::warn!("Publish cleanup after bulk delete (server {}): {}", srv.id, e);
                }
                deleted += 1;
                log::info!("Deleted {} server {} for account {}", provider_type, srv.provider_server_id, account_id);
            }
            Err(e) => {
                log::error!("Failed to delete {} server {}: {}", provider_type, srv.provider_server_id, e);
                failed += 1;
            }
        }
    }
    (deleted, failed)
}

// ── Shared post-delete cleanup ──

/// Detach a deleted server from publishing: re-publish the current front IP to
/// the OpenVPN-AS backend so it stops pointing at the dead front (or no-ops if
/// no front remains).
///
/// Idempotent. Call this from every code path that soft-deletes a server
/// (admin reject, monitor-fail recycle, bulk deactivate, reconcile reaping a
/// vanished provider row).
pub async fn cleanup_server(
    pool: &SqlitePool,
    _srv: &db::Server,
) -> Result<(), String> {
    publish_front_ip(pool).await
        .map_err(|e| format!("publish_front_ip: {}", e))?;
    Ok(())
}

