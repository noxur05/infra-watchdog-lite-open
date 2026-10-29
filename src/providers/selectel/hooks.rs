//! Provisioner hooks for Selectel. Called from the generic provisioner loop
//! to handle Selectel-specific behaviors (floating IP assignment, IP rotation,
//! traffic quota check).
//!
//! Each hook takes a `HookCtx` with the shared state it needs — the provisioner
//! passes pool/bot/account info and any mutable per-account state (same-IP
//! counter). Keeps Selectel logic out of `provisioner.rs`.

use sqlx::SqlitePool;
use teloxide::prelude::*;

use crate::db;
use crate::time_utils::month_key;
use crate::utils::escape_md;

use super::client::SelectelClient;
use super::db as sel_db;

/// Shared read-only context passed to each Selectel hook.
/// Owns all fields (pool/bot are cheap Arc clones) so callers can still mutably
/// borrow other fields of `AccountProvisioner` (e.g. rotation state) alongside it.
pub struct HookCtx {
    pub pool: SqlitePool,
    pub bot: Bot,
    pub account_id: i64,
    pub account_name: String,
    pub log_prefix: String,
}

/// Build a SelectelClient for the given account, or return an error string.
async fn build_client(pool: &SqlitePool, account_id: i64) -> Result<(SelectelClient, String), String> {
    let acct = sel_db::get_account(pool, account_id).await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "selectel account not found".to_string())?;
    let region = acct.region.clone();
    Ok((
        SelectelClient::new(acct.username, acct.password, acct.account_id, acct.project_id, acct.region),
        region,
    ))
}

// ── Warm-pool floating IPs ──

/// Number of floating IPs kept warm on the single front: 1 active + 2 spares.
pub const POOL_SIZE: usize = 3;
/// A pool member is marked `blocked` after this many consecutive failed tests.
const FAIL_THRESHOLD: i64 = 2;

/// Outcome of a `manage_pool` cycle. Tells the caller whether the active IP
/// changed (→ republish the new front to the OVPN-AS backend).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolOutcome {
    /// Active IP still good (spares may have been silently replaced). No publish.
    Healthy,
    /// Active was blocked; failed over to a tested-good spare. Caller republishes.
    FailedOver,
    /// All IPs blocked; the whole batch was recycled into fresh ones. Caller
    /// republishes (the new active is untested — re-tested next cycle).
    Recycled,
    /// Could not recover a working pool (allocation/API failure). Caller may
    /// notify / escalate.
    Stuck,
}

/// Build the warm pool for a freshly-ACTIVE server: allocate POOL_SIZE floating
/// IPs (one per fixed IP on the port), persist them, and point
/// `servers.ipv4`/`floating_ip_id` at the active one. Idempotent — reconciles
/// the DB rows to the live pool. Runs once at ACTIVE (the caller gates it on
/// `floating_ip_id.is_none()`).
pub async fn setup_pool(ctx: &HookCtx, srv: &db::Server) -> Result<(), String> {
    let (client, _) = build_client(&ctx.pool, ctx.account_id).await?;
    let pool = client.build_pool(&srv.provider_server_id, POOL_SIZE)
        .await.map_err(|e| e.to_string())?;
    if pool.is_empty() {
        return Err("build_pool returned no floating IPs".into());
    }

    // Live API is the source of truth — clear and re-insert the DB rows.
    db::clear_floating_ip_pool(&ctx.pool, srv.id).await.map_err(|e| e.to_string())?;
    // Active = the FIP already in servers.floating_ip_id if present in the pool,
    // else the first one.
    let active_idx = srv.floating_ip_id.as_deref()
        .and_then(|fid| pool.iter().position(|(_, _, id)| id == fid))
        .unwrap_or(0);
    for (i, (fixed, addr, fip_id)) in pool.iter().enumerate() {
        let role = if i == active_idx { "active" } else { "spare" };
        db::insert_floating_ip(&ctx.pool, srv.id, fixed, addr, fip_id, role)
            .await.map_err(|e| e.to_string())?;
    }
    let (_, active_addr, active_fip) = &pool[active_idx];
    db::update_server_floating_ip(&ctx.pool, srv.id, active_addr, active_fip)
        .await.map_err(|e| e.to_string())?;

    log::info!("{} warm pool ready ({} IPs), active {}",
        ctx.log_prefix, pool.len(), active_addr);
    Ok(())
}

/// Ingress failover. The warm pool tests the front's *egress* (front→internet),
/// but if a floating IP is unreachable *from the bot host* (Selectel per-IP
/// routing gaps), SSH to it dies with "No route to host" and the bot can neither
/// deploy nor manage the front — the egress tests never catch it. Probe SSH to
/// the active IP; if it's unreachable, probe the spares and promote the first
/// reachable one to active. Returns `Ok(true)` if the active IP was changed (the
/// caller should let the next cycle proceed and republish).
pub async fn ensure_active_ip_reachable(ctx: &HookCtx, srv: &db::Server) -> Result<bool, String> {
    let Some(root_pass) = srv.root_pass.as_deref() else { return Ok(false) };
    let ssh_user = "root";

    let rows = db::list_floating_ip_pool(&ctx.pool, srv.id).await.map_err(|e| e.to_string())?;
    if rows.len() < 2 {
        return Ok(false); // no spare to fail over to
    }

    let active = rows.iter().find(|r| r.role == "active");
    if let Some(a) = active {
        if crate::ssh::probe_ssh(&a.floating_ip, root_pass, ssh_user).await {
            return Ok(false); // active reachable — normal path
        }
        log::warn!("{} active IP {} unreachable from bot host — probing pool for a reachable member",
            ctx.log_prefix, a.floating_ip);
    }

    for row in rows.iter().filter(|r| r.role != "active") {
        if crate::ssh::probe_ssh(&row.floating_ip, root_pass, ssh_user).await {
            if let Some(a) = active {
                db::set_floating_ip_role(&ctx.pool, a.id, "spare").await.ok();
            }
            db::set_floating_ip_role(&ctx.pool, row.id, "active").await.ok();
            db::update_server_floating_ip(&ctx.pool, srv.id, &row.floating_ip, &row.fip_id)
                .await.map_err(|e| e.to_string())?;
            log::info!("{} bot-host ingress failover: active IP → reachable pool member {}",
                ctx.log_prefix, row.floating_ip);
            notify_admin(&ctx.bot, &format!(
                "Selectel: active front IP was unreachable from the bot host — switched active to `{}`\\.\nServer: {}\nAccount: {} \\({}\\)",
                escape_md(&row.floating_ip), escape_md(&srv.provider_server_id),
                ctx.account_id, escape_md(&ctx.account_name),
            )).await;
            return Ok(true);
        }
    }

    log::warn!("{} no pool IP reachable from bot host yet — will retry next cycle", ctx.log_prefix);
    Ok(false)
}

/// Test every pool IP individually and handle the three block events:
///   1. a SPARE blocked      → replace just that slot (release + reallocate),
///                             re-tested next cycle. NO publish.
///   2. the ACTIVE blocked    → fail over to a tested-good spare + publish.
///   3. ALL blocked / no good → recycle the whole batch (release + reallocate
///                             all), publish the new active. NO region failover.
///
/// Returns Err only on an SSH-level failure that prevented ANY test (the caller
/// treats that as the server being unreachable and escalates separately).
pub async fn manage_pool(
    ctx: &HookCtx, srv: &db::Server, test_urls: &[String],
) -> Result<PoolOutcome, String> {
    let (client, _) = build_client(&ctx.pool, ctx.account_id).await?;
    let mgmt_ip = srv.ipv4.as_deref()
        .ok_or_else(|| "selectel server has no active IP".to_string())?;
    let root_pass = srv.root_pass.as_deref()
        .ok_or_else(|| "selectel server has no root password".to_string())?;
    let ssh_user = "root";

    // Make sure the pool exists (post-deploy it always should).
    let mut rows = db::list_floating_ip_pool(&ctx.pool, srv.id).await.map_err(|e| e.to_string())?;
    if rows.is_empty() {
        setup_pool(ctx, srv).await?;
        rows = db::list_floating_ip_pool(&ctx.pool, srv.id).await.map_err(|e| e.to_string())?;
    }
    if rows.is_empty() {
        return Ok(PoolOutcome::Stuck);
    }

    // 1. Ensure the guest has every pool fixed IP on its NIC (idempotent) so
    //    `curl --interface` can source from each.
    let fixed_ips: Vec<String> = rows.iter().map(|r| r.fixed_ip.clone()).collect();
    if let Err(e) = crate::ssh::configure_extra_ips(mgmt_ip, root_pass, ssh_user, &fixed_ips).await {
        log::warn!("{} configure_extra_ips: {}", ctx.log_prefix, e);
    }

    // 2. Test each IP. Track SSH errors separately — if EVERY test SSH-failed,
    //    the server is unreachable; bubble that up to the caller.
    //
    // Per-IP source egress: each test sources from a pool member's fixed IP
    // (`curl --interface <fixed_ip>`) expecting Neutron's 1:1 fixed↔floating
    // NAT to send it out via that member's floating IP. The deploy DNAT
    // script's blanket MASQUERADE would collapse those sources onto the
    // primary address, so `configure_extra_ips` (step 1 above) also inserts a
    // per-source `-j RETURN` exemption ahead of the MASQUERADE rules for every
    // pool fixed IP. LIVE-VALIDATION: confirm on the live spike that each
    // test really egresses via its own FIP (e.g. `curl --interface <fixed_ip>
    // ifconfig.me` from the front shows that member's floating IP).
    let mut ssh_errors = 0usize;
    for row in &mut rows {
        match crate::ssh::ssh_curl_test_from(mgmt_ip, root_pass, ssh_user, test_urls, &row.fixed_ip).await {
            Ok(true) => {
                row.status = "good".to_string();
                row.consecutive_fails = 0;
            }
            Ok(false) => {
                row.consecutive_fails += 1;
                if row.consecutive_fails >= FAIL_THRESHOLD {
                    row.status = "blocked".to_string();
                }
            }
            Err(e) => {
                ssh_errors += 1;
                log::debug!("{} pool test SSH err on {}: {}", ctx.log_prefix, row.floating_ip, e);
                continue;
            }
        }
        db::update_floating_ip_status(&ctx.pool, row.id, &row.status, row.consecutive_fails).await.ok();
    }
    if ssh_errors == rows.len() {
        return Err(format!("all {} pool IP tests SSH-failed", rows.len()));
    }

    let port_id = client.get_port_id(&srv.provider_server_id).await.map_err(|e| e.to_string())?;

    // 3. Event 1: replace blocked SPARES in place (keep the pool warm). No publish.
    for row in &mut rows {
        if row.role == "spare" && row.status == "blocked" {
            if let Err(e) = client.release_floating_ip(&row.fip_id).await {
                log::warn!("{} release blocked spare {}: {}", ctx.log_prefix, row.fip_id, e);
            }
            match client.allocate_fip_for_fixed(&port_id, &row.fixed_ip).await {
                Ok((addr, id)) => {
                    if let Err(e) = db::update_floating_ip_addr(&ctx.pool, row.id, &addr, &id).await {
                        // Persisting the new FIP failed — release it so we don't
                        // leak a billable, untracked floating IP. The DB still
                        // holds the old (already-released) fip_id and retries
                        // cleanly next cycle.
                        log::error!("{} persist replaced spare {} failed: {} — releasing to avoid leak",
                            ctx.log_prefix, addr, e);
                        let _ = client.release_floating_ip(&id).await;
                    } else {
                        log::info!("{} replaced blocked spare on {} → {}", ctx.log_prefix, row.fixed_ip, addr);
                        row.floating_ip = addr;
                        row.fip_id = id;
                        row.status = "untested".to_string();
                        row.consecutive_fails = 0;
                    }
                }
                Err(e) => log::warn!("{} reallocate spare on {}: {}", ctx.log_prefix, row.fixed_ip, e),
            }
        }
    }

    // 4. Is the active blocked?
    let active_blocked = rows.iter()
        .find(|r| r.role == "active")
        .map(|a| a.status == "blocked")
        .unwrap_or(false);
    if !active_blocked {
        return Ok(PoolOutcome::Healthy);
    }
    let old_active_id = rows.iter().find(|r| r.role == "active").map(|a| a.id);

    // Event 2: active blocked → fail over to a tested-good spare.
    let good_spare = rows.iter()
        .find(|r| r.role == "spare" && r.status == "good")
        .map(|r| (r.id, r.floating_ip.clone(), r.fip_id.clone()));
    if let Some((good_id, good_ip, good_fip)) = good_spare {
        if let Some(oid) = old_active_id {
            db::set_floating_ip_role(&ctx.pool, oid, "spare").await.ok();
        }
        db::set_floating_ip_role(&ctx.pool, good_id, "active").await.ok();
        db::update_server_floating_ip(&ctx.pool, srv.id, &good_ip, &good_fip)
            .await.map_err(|e| e.to_string())?;
        log::info!("{} active IP blocked → failover to spare {}", ctx.log_prefix, good_ip);
        notify_admin(&ctx.bot, &format!(
            "Selectel: active IP blocked, failed over to spare `{}`\\.\nServer: {}\nAccount: {} \\({}\\)",
            escape_md(&good_ip), escape_md(&srv.provider_server_id),
            ctx.account_id, escape_md(&ctx.account_name),
        )).await;
        return Ok(PoolOutcome::FailedOver);
    }

    // Event 3: all blocked, no good spare → recycle the whole batch.
    log::warn!("{} all pool IPs blocked → recycling batch", ctx.log_prefix);
    for row in &rows {
        if let Err(e) = client.release_floating_ip(&row.fip_id).await {
            log::warn!("{} release {} during recycle: {}", ctx.log_prefix, row.fip_id, e);
        }
    }
    db::clear_floating_ip_pool(&ctx.pool, srv.id).await.ok();
    match client.build_pool(&srv.provider_server_id, POOL_SIZE).await {
        Ok(pool) if !pool.is_empty() => {
            for (i, (fixed, addr, fip_id)) in pool.iter().enumerate() {
                let role = if i == 0 { "active" } else { "spare" };
                if let Err(e) = db::insert_floating_ip(&ctx.pool, srv.id, fixed, addr, fip_id, role).await {
                    // Don't leak: if we can't track this fresh FIP, release it.
                    log::error!("{} persist recycled pool IP {} failed: {} — releasing to avoid leak",
                        ctx.log_prefix, addr, e);
                    let _ = client.release_floating_ip(fip_id).await;
                }
            }
            let (_, addr, fip_id) = &pool[0];
            db::update_server_floating_ip(&ctx.pool, srv.id, addr, fip_id)
                .await.map_err(|e| e.to_string())?;
            notify_admin(&ctx.bot, &format!(
                "Selectel: all pool IPs blocked, recycled the batch \\({} fresh IPs\\)\\.\nNew active `{}`\nServer: {}\nAccount: {} \\({}\\)",
                pool.len(), escape_md(addr), escape_md(&srv.provider_server_id),
                ctx.account_id, escape_md(&ctx.account_name),
            )).await;
            Ok(PoolOutcome::Recycled)
        }
        Ok(_) => Ok(PoolOutcome::Stuck),
        Err(e) => {
            log::error!("{} recycle build_pool failed: {}", ctx.log_prefix, e);
            Ok(PoolOutcome::Stuck)
        }
    }
}

/// Check this month's outgoing traffic. Delete + sleep until next month if ≥ 2.9 TB.
/// Returns Ok(true) when server was deleted (caller should stop the cycle).
pub async fn check_traffic(
    ctx: &HookCtx,
    srv: &db::Server,
    tx_total: i64,
) -> Result<bool, String> {
    let current_month = month_key(chrono::Utc::now());

    // Rebase when first read, month rolled over, or /proc/net/dev reset (server reboot).
    let needs_rebase = match (&srv.tx_baseline_bytes, &srv.tx_baseline_month) {
        (Some(b), Some(m)) => m != &current_month || tx_total < *b,
        _ => true,
    };
    let baseline = if needs_rebase {
        if let Err(e) = db::set_tx_baseline(&ctx.pool, srv.id, tx_total, &current_month).await {
            log::warn!("{} failed to persist traffic baseline: {}", ctx.log_prefix, e);
        }
        log::info!("{} traffic baseline set: {} bytes for {}",
            ctx.log_prefix, tx_total, current_month);
        tx_total
    } else {
        srv.tx_baseline_bytes.unwrap_or(tx_total)
    };

    let month_bytes = tx_total.saturating_sub(baseline);
    let gb = month_bytes as f64 / 1_073_741_824.0;

    if gb >= 2900.0 {
        log::warn!("{} traffic limit reached: {:.1} GB", ctx.log_prefix, gb);
        notify_admin(&ctx.bot, &format!(
            "Selectel traffic limit\\! {:.1} GB used this month \\(limit 3TB\\)\\.\n\
             Deleting server to avoid charges\\.\n\
             Account: {} \\({}\\)\nWaiting until next month\\.",
            gb, ctx.account_id, escape_md(&ctx.account_name),
        )).await;
        // Caller deletes the server, then waits until the next month.
        return Ok(true);
    } else if gb >= 2500.0 {
        log::info!("{} traffic: {:.1} GB / 3000 GB", ctx.log_prefix, gb);
    }
    Ok(false)
}

/// Release ALL of a server's floating IPs (the whole warm pool plus the active
/// one tracked on the `servers` row) and clear the pool table. Called on
/// teardown so no floating IP is leaked. No-op if nothing is associated.
pub async fn release_floating_ip(ctx: &HookCtx, srv: &db::Server) {
    let client = match build_client(&ctx.pool, ctx.account_id).await {
        Ok((c, _)) => c,
        Err(e) => {
            log::warn!("{} cannot build client to release IPs: {}", ctx.log_prefix, e);
            return;
        }
    };

    let mut released: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Every pool member.
    let rows = db::list_floating_ip_pool(&ctx.pool, srv.id).await.unwrap_or_default();
    for row in &rows {
        if released.insert(row.fip_id.clone()) {
            if let Err(e) = client.release_floating_ip(&row.fip_id).await {
                log::warn!("{} release pool fip {}: {}", ctx.log_prefix, row.fip_id, e);
            }
        }
    }

    // The active FIP tracked on the server row (in case it isn't in the pool).
    if let Some(fip_id) = &srv.floating_ip_id {
        if released.insert(fip_id.clone()) {
            if let Err(e) = client.release_floating_ip(fip_id).await {
                log::warn!("{} release active fip {}: {}", ctx.log_prefix, fip_id, e);
            }
        }
    }

    if let Err(e) = db::clear_floating_ip_pool(&ctx.pool, srv.id).await {
        log::warn!("{} clear_floating_ip_pool: {}", ctx.log_prefix, e);
    }
}

async fn notify_admin(bot: &Bot, text: &str) {
    // Delegate to the shared helper so Selectel alerts reach every admin too.
    crate::notify::notify_admin(bot, text).await;
}
