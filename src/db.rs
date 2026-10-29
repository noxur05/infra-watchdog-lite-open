use sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions, SqliteSynchronous,
};
use std::path::Path;
use std::str::FromStr;

pub async fn init_pool(database_url: &str) -> Result<SqlitePool, sqlx::Error> {
    // synchronous=FULL set on the *connect options* so it applies to every
    // connection the pool opens — `synchronous` is a per-connection PRAGMA
    // that is NOT persisted in the DB file (unlike journal_mode), so setting
    // it only on the migration connection would leave the runtime pool
    // connections on the default. FULL fsyncs every commit to disk before
    // returning, so a committed row survives any restart via WAL replay —
    // closing the "lost the most recent write" window that cost us a
    // freshly-added manual server when the host disk threw I/O errors mid-day.
    // The write volume here is a few small rows per minute, so the per-commit
    // fsync cost is negligible. (journal_mode set here too, the canonical
    // place; the redundant PRAGMA in run_migrations is harmless.)
    let options = SqliteConnectOptions::from_str(database_url)?
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Full);

    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(options)
        .await?;

    run_migrations(&pool).await?;
    Ok(pool)
}

/// Fold the WAL back into the main DB file and truncate it. Run on graceful
/// shutdown so a deploy leaves a checkpointed main file with an empty WAL,
/// rather than relying on WAL replay across the restart. Best-effort: a
/// failure is logged, not fatal.
pub async fn checkpoint_wal(pool: &SqlitePool) {
    match sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)").execute(pool).await {
        Ok(_) => log::info!("WAL checkpointed (TRUNCATE) on shutdown"),
        Err(e) => log::warn!("WAL checkpoint on shutdown failed: {}", e),
    }
}

/// Run `PRAGMA integrity_check` — the thorough, index-deep variant.
/// (`quick_check` skips btree-page verification and missed real corruption in
/// prod, so use the full check.) Returns `Ok(())` only when the DB reports a
/// single "ok"; any problem rows, or a query that itself fails (a malformed DB
/// errors mid-step), are surfaced as `Err(description)`. Cheap on our small DB.
pub async fn integrity_check(pool: &SqlitePool) -> Result<(), String> {
    match sqlx::query_scalar::<_, String>("PRAGMA integrity_check")
        .fetch_all(pool)
        .await
    {
        Ok(rows) if rows.len() == 1 && rows[0] == "ok" => Ok(()),
        Ok(rows) => Err(rows.join("; ")),
        Err(e) => Err(format!("integrity_check query failed (DB likely corrupt): {}", e)),
    }
}

async fn run_migrations(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    // Use a single connection for all migrations so PRAGMA settings persist
    let mut conn = pool.acquire().await?;

    sqlx::query("PRAGMA journal_mode=WAL").execute(&mut *conn).await?;

    // Disable FK checks during migrations (some migrations recreate tables with FK refs)
    sqlx::query("PRAGMA foreign_keys=OFF").execute(&mut *conn).await?;

    let migration_dir = Path::new("migrations");
    let entries = match std::fs::read_dir(migration_dir) {
        Ok(e) => e,
        Err(e) => {
            log::error!("migrations/ directory not found: {}", e);
            return Err(sqlx::Error::Configuration(
                format!("migrations/ directory not found: {}", e).into(),
            ));
        }
    };

    let mut files: Vec<_> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map(|x| x == "sql").unwrap_or(false))
        .collect();
    files.sort_by_key(|e| e.file_name());

    for entry in files {
        let sql = match std::fs::read_to_string(entry.path()) {
            Ok(s) => s,
            Err(e) => {
                log::error!("Failed to read {:?}: {}", entry.path(), e);
                return Err(sqlx::Error::Configuration(
                    format!("Failed to read migration {:?}: {}", entry.path(), e).into(),
                ));
            }
        };
        // Strip `--` line comments before splitting on `;` — otherwise a
        // semicolon inside a comment ("foo; bar") gets split as a statement
        // and SQLite chokes parsing the comment text. Block comments and
        // string literals containing `;` are not handled here; keep
        // migrations simple and avoid both.
        let stripped: String = sql.lines()
            .map(|l| match l.find("--") { Some(i) => &l[..i], None => l })
            .collect::<Vec<&str>>()
            .join("\n");
        for statement in stripped.split(';') {
            let stmt = statement.trim();
            if !stmt.is_empty() {
                match sqlx::query(stmt).execute(&mut *conn).await {
                    Ok(_) => {}
                    Err(e) => {
                        let err_msg = e.to_string();
                        // Ignore "duplicate column" errors from already-applied ALTER TABLE migrations
                        if err_msg.contains("duplicate column name") {
                            log::debug!("Column already exists, skipping: {}", err_msg);
                            continue;
                        }
                        return Err(e);
                    }
                }
            }
        }
        log::info!("Applied migration: {:?}", entry.file_name());
    }

    // Re-enable FK checks after migrations
    sqlx::query("PRAGMA foreign_keys=ON").execute(&mut *conn).await?;

    Ok(())
}

// ── Server CRUD (shared `servers` table) ──

#[derive(Debug, Clone, sqlx::FromRow)]
#[allow(dead_code)] // some columns are admin-UI / future-use only
pub struct Server {
    pub id: i64,
    pub provider_type: String,
    pub provider_account_id: i64,
    pub provider_server_id: String,
    pub ipv4: Option<String>,
    pub root_pass: Option<String>,
    pub status: String,
    pub script_deployed: bool,
    pub ip_setup_done: bool,
    pub consecutive_curl_fails: i64,
    pub consecutive_ssh_fails: i64,
    pub ssh_fail_notified_at: Option<String>,
    pub front_published: bool,
    pub created_at: String,
    pub deleted_at: Option<String>,
    pub floating_ip_id: Option<String>,
    pub tx_baseline_bytes: Option<i64>,
    pub tx_baseline_month: Option<String>,
    pub delete_failed_at: Option<String>,
    pub region: Option<String>,
    pub hardened: bool,
}

const SERVER_COLUMNS: &str =
    "id, provider_type, provider_account_id, provider_server_id, ipv4, root_pass, status, \
     script_deployed, ip_setup_done, consecutive_curl_fails, consecutive_ssh_fails, \
     ssh_fail_notified_at, front_published, created_at, deleted_at, floating_ip_id, \
     tx_baseline_bytes, tx_baseline_month, delete_failed_at, region, hardened";

pub async fn create_server(
    pool: &SqlitePool, provider_type: &str, provider_account_id: i64, provider_server_id: &str,
) -> Result<i64, sqlx::Error> {
    let row: (i64,) = sqlx::query_as(
        "INSERT INTO servers (provider_type, provider_account_id, provider_server_id) VALUES (?, ?, ?) RETURNING id"
    )
    .bind(provider_type)
    .bind(provider_account_id)
    .bind(provider_server_id)
    .fetch_one(pool)
    .await?;
    Ok(row.0)
}

pub async fn get_server_by_id(pool: &SqlitePool, id: i64) -> Result<Option<Server>, sqlx::Error> {
    let sql = format!(
        "SELECT {} FROM servers WHERE id = ? AND deleted_at IS NULL",
        SERVER_COLUMNS
    );
    sqlx::query_as::<_, Server>(&sql)
        .bind(id)
        .fetch_optional(pool)
        .await
}

pub async fn get_active_server_for_account(
    pool: &SqlitePool, provider_type: &str, account_id: i64,
) -> Result<Option<Server>, sqlx::Error> {
    let sql = format!(
        "SELECT {} FROM servers WHERE deleted_at IS NULL AND provider_type = ? AND provider_account_id = ? ORDER BY id DESC LIMIT 1",
        SERVER_COLUMNS
    );
    sqlx::query_as::<_, Server>(&sql)
        .bind(provider_type)
        .bind(account_id)
        .fetch_optional(pool)
        .await
}


/// Best-effort: if `new_ip` differs from the server's current non-empty IP,
/// record the OLD ip in `server_ip_history`. IP-rotation providers (beget,
/// selectel) overwrite `servers.ipv4` in place, so without this the old IP
/// vanishes from the DB entirely — and since host ownership is keyed on
/// `list_owned_host_ips` (current ipv4 only), the old IP's hosts could never be
/// pruned and leaked forever. History keeps every rotated-away IP "owned" so
/// the next sync strips its orphaned hosts. Never fails the caller's update.
/// Record an IP the bot has held into `server_ip_history` so
/// `list_owned_host_ips` keeps recognising its hosts as bot-owned (and
/// can therefore strip them once the bot moves off the IP). An IP only needs to
/// appear once — the list folds history into a set — so the INSERT is deduped on
/// `ipv4`, bounding table growth to the count of distinct IPs ever held.
async fn insert_ip_history(pool: &SqlitePool, id: i64, ip: &str) {
    if ip.is_empty() {
        return;
    }
    if let Err(e) = sqlx::query(
        "INSERT INTO server_ip_history (server_id, ipv4) \
         SELECT ?, ? WHERE NOT EXISTS (SELECT 1 FROM server_ip_history WHERE ipv4 = ?)"
    )
    .bind(id)
    .bind(ip)
    .bind(ip)
    .execute(pool)
    .await
    {
        log::warn!("insert_ip_history(server {}, {}): {}", id, ip, e);
    }
}

async fn record_ip_rotation(pool: &SqlitePool, id: i64, new_ip: &str) {
    let current = match sqlx::query_as::<_, (Option<String>,)>(
        "SELECT ipv4 FROM servers WHERE id = ?"
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    {
        Ok(row) => row,
        // Don't fail the caller's update, but don't silently drop it either —
        // a swallowed read here means the rotated-away IP never gets recorded
        // and its hosts leak, which is exactly what this function prevents.
        Err(e) => {
            log::warn!("record_ip_rotation: read current ipv4 for server {}: {}", id, e);
            return;
        }
    };
    let Some((Some(old),)) = current else { return };
    if old == new_ip {
        return;
    }
    insert_ip_history(pool, id, &old).await;
}


pub async fn update_server_info(
    pool: &SqlitePool, id: i64, ipv4: Option<&str>, root_pass: Option<&str>, status: &str,
) -> Result<(), sqlx::Error> {
    if let Some(new_ip) = ipv4 {
        record_ip_rotation(pool, id, new_ip).await;
    }
    sqlx::query(
        "UPDATE servers SET ipv4 = COALESCE(?, ipv4), root_pass = COALESCE(?, root_pass), status = ? WHERE id = ?"
    )
    .bind(ipv4)
    .bind(root_pass)
    .bind(status)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn update_server_floating_ip(
    pool: &SqlitePool, id: i64, ipv4: &str, floating_ip_id: &str,
) -> Result<(), sqlx::Error> {
    record_ip_rotation(pool, id, ipv4).await;
    sqlx::query(
        "UPDATE servers SET ipv4 = ?, floating_ip_id = ? WHERE id = ?"
    )
    .bind(ipv4)
    .bind(floating_ip_id)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

// ── Floating-IP warm pool (Selectel) ──
//
// The single active Selectel server keeps a small pool of floating IPs (1
// active + N spares), each bound to its own fixed IP on the server's one
// Neutron port. `servers.ipv4`/`floating_ip_id` always track the ACTIVE row so
// `openvpn_sync::publish_front_ip` keeps publishing the active one unchanged.

#[derive(Debug, Clone, sqlx::FromRow)]
#[allow(dead_code)] // server_id is a FromRow column, populated but not read back
pub struct FloatingIp {
    pub id: i64,
    pub server_id: i64,
    pub fixed_ip: String,
    pub floating_ip: String,
    pub fip_id: String,
    pub role: String,            // 'active' | 'spare'
    pub status: String,          // 'good' | 'blocked' | 'untested'
    pub consecutive_fails: i64,
}

pub async fn list_floating_ip_pool(
    pool: &SqlitePool, server_id: i64,
) -> Result<Vec<FloatingIp>, sqlx::Error> {
    sqlx::query_as::<_, FloatingIp>(
        "SELECT id, server_id, fixed_ip, floating_ip, fip_id, role, status, consecutive_fails \
         FROM server_floating_ips WHERE server_id = ? ORDER BY id",
    )
    .bind(server_id)
    .fetch_all(pool)
    .await
}

pub async fn insert_floating_ip(
    pool: &SqlitePool, server_id: i64, fixed_ip: &str, floating_ip: &str, fip_id: &str, role: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO server_floating_ips (server_id, fixed_ip, floating_ip, fip_id, role, status) \
         VALUES (?, ?, ?, ?, ?, 'untested')",
    )
    .bind(server_id)
    .bind(fixed_ip)
    .bind(floating_ip)
    .bind(fip_id)
    .bind(role)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn clear_floating_ip_pool(pool: &SqlitePool, server_id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM server_floating_ips WHERE server_id = ?")
        .bind(server_id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn update_floating_ip_status(
    pool: &SqlitePool, id: i64, status: &str, consecutive_fails: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE server_floating_ips SET status = ?, consecutive_fails = ?, tested_at = datetime('now') WHERE id = ?",
    )
    .bind(status)
    .bind(consecutive_fails)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Replace a pool slot's floating IP with a freshly-allocated one (same fixed
/// IP), resetting its test state to untested.
pub async fn update_floating_ip_addr(
    pool: &SqlitePool, id: i64, floating_ip: &str, fip_id: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE server_floating_ips SET floating_ip = ?, fip_id = ?, status = 'untested', \
         consecutive_fails = 0, tested_at = NULL WHERE id = ?",
    )
    .bind(floating_ip)
    .bind(fip_id)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn set_floating_ip_role(
    pool: &SqlitePool, id: i64, role: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE server_floating_ips SET role = ? WHERE id = ?")
        .bind(role)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Set the monthly TX baseline for a server. Used for traffic monitoring.
/// `month_yyyymm` is like "2026-04" — when this changes, baseline is reset.
pub async fn set_tx_baseline(
    pool: &SqlitePool, id: i64, tx_bytes: i64, month_yyyymm: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE servers SET tx_baseline_bytes = ?, tx_baseline_month = ? WHERE id = ?"
    )
    .bind(tx_bytes)
    .bind(month_yyyymm)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Mark that a remote-delete attempt failed for this server. Used by the
/// monitor loop to skip the server (and avoid notification spam) until an
/// admin manually intervenes via deactivate-with-provider-delete.
/// (After successful admin re-delete, soft_delete_server hides the row from
/// active queries — we don't need to explicitly clear delete_failed_at.)
pub async fn set_delete_failed_at_now(pool: &SqlitePool, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE servers SET delete_failed_at = datetime('now') WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn mark_server_deployed(pool: &SqlitePool, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE servers SET script_deployed = 1 WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn mark_server_ip_setup(pool: &SqlitePool, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE servers SET ip_setup_done = 1 WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Record that the bot has run first-contact hardening on this server
/// (UFW + fail2ban killed, sshd MaxStartups raised, BBR enabled). The bot
/// gates the hardening call on this flag so it only runs once per server.
pub async fn mark_server_hardened(pool: &SqlitePool, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE servers SET hardened = 1 WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn increment_curl_fails(pool: &SqlitePool, id: i64) -> Result<i64, sqlx::Error> {
    sqlx::query("UPDATE servers SET consecutive_curl_fails = consecutive_curl_fails + 1 WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    let row: (i64,) = sqlx::query_as("SELECT consecutive_curl_fails FROM servers WHERE id = ?")
        .bind(id)
        .fetch_one(pool)
        .await?;
    Ok(row.0)
}

pub async fn reset_curl_fails(pool: &SqlitePool, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE servers SET consecutive_curl_fails = 0 WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn increment_ssh_fails(pool: &SqlitePool, id: i64) -> Result<i64, sqlx::Error> {
    sqlx::query("UPDATE servers SET consecutive_ssh_fails = consecutive_ssh_fails + 1 WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    let row: (i64,) = sqlx::query_as("SELECT consecutive_ssh_fails FROM servers WHERE id = ?")
        .bind(id)
        .fetch_one(pool)
        .await?;
    Ok(row.0)
}

pub async fn reset_ssh_fails(pool: &SqlitePool, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE servers SET consecutive_ssh_fails = 0, ssh_fail_notified_at = NULL WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn set_ssh_fail_notified(pool: &SqlitePool, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE servers SET ssh_fail_notified_at = datetime('now') WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn mark_server_published(pool: &SqlitePool, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE servers SET front_published = 1 WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn unmark_server_published(pool: &SqlitePool, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE servers SET front_published = 0 WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Clear the deployed flag so the state machine re-runs `test_and_deploy`
/// (re-applies the DNAT script from scratch and republishes). Used by the
/// admin "Reset DNAT" flow and the automatic re-deploy on backend IP change.
pub async fn unmark_server_deployed(pool: &SqlitePool, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE servers SET script_deployed = 0, front_published = 0 WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}


pub async fn get_all_active_servers(pool: &SqlitePool) -> Result<Vec<Server>, sqlx::Error> {
    let sql = format!(
        "SELECT {} FROM servers WHERE deleted_at IS NULL ORDER BY id",
        SERVER_COLUMNS
    );
    sqlx::query_as::<_, Server>(&sql)
        .fetch_all(pool)
        .await
}

pub async fn get_active_servers_for_account(
    pool: &SqlitePool, provider_type: &str, account_id: i64,
) -> Result<Vec<Server>, sqlx::Error> {
    let sql = format!(
        "SELECT {} FROM servers WHERE deleted_at IS NULL AND provider_type = ? AND provider_account_id = ? ORDER BY id",
        SERVER_COLUMNS
    );
    sqlx::query_as::<_, Server>(&sql)
        .bind(provider_type)
        .bind(account_id)
        .fetch_all(pool)
        .await
}

pub async fn soft_delete_server(pool: &SqlitePool, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE servers SET deleted_at = datetime('now'), status = 'deleted' WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Flip a provider account's `is_active` flag. `table` is always a code
/// literal (e.g. "tw_accounts"), never user input, so the `format!` is safe.
/// Multi-region providers (Vultr, Gcore) cascade their region tables in their
/// own wrappers — this helper only touches the parent row.
pub async fn set_account_active(
    pool: &SqlitePool, table: &'static str, id: i64, active: bool,
) -> Result<bool, sqlx::Error> {
    let sql = format!("UPDATE {} SET is_active = ? WHERE id = ?", table);
    let result = sqlx::query(&sql)
        .bind(active as i64)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

/// Hard-delete a deactivated provider account row. Refuses on active accounts
/// (`is_active = 0` guard) so the admin can't bypass the deactivate-first
/// safety gate. `table` is always a code literal, never user input.
pub async fn delete_deactivated_account(
    pool: &SqlitePool, table: &'static str, id: i64,
) -> Result<bool, sqlx::Error> {
    let sql = format!("DELETE FROM {} WHERE id = ? AND is_active = 0", table);
    let result = sqlx::query(&sql)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

/// Count the live (not soft-deleted) servers owned by a single-region
/// provider+account. Shared by the per-account safety gate in every
/// single-region provider's admin UI. Multi-region providers (Vultr, Gcore)
/// count across their region rows and keep their own `*_across_regions` query.
pub async fn count_active_servers(
    pool: &SqlitePool, provider_type: &str, account_id: i64,
) -> Result<i64, sqlx::Error> {
    let row: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM servers
         WHERE provider_type = ? AND provider_account_id = ? AND deleted_at IS NULL"
    )
    .bind(provider_type)
    .bind(account_id)
    .fetch_one(pool)
    .await?;
    Ok(row.0)
}

/// How long until the rate-limit cooldown for this account expires, if it is
/// currently in cooldown. Restart-safe: the calculation is derived from the
/// DB's `created_at` timestamps (UTC, as stored by SQLite's `datetime('now')`),
/// so a process restart mid-cooldown resumes the same end time instead of
/// starting a fresh sleep.
///
/// Anchor controls which timestamp the cooldown is measured from:
/// * `Oldest` (default for most providers) — sliding window: cooldown ends
///   when the oldest in-window trial slides out. Best for search-mode loops
///   like Vultr that want to resume creates as soon as a slot reopens.
/// * `Latest` — cooldown ends a flat `window_hours` after the most recent
///   trial. Best for cost-sensitive providers like Beget where each capped
///   attempt is real money: hitting the cap freezes creates for the full
///   window instead of letting a 4th create slip in mid-window.
///
/// Semantics:
/// * `Ok(None)`  — count of trials in last `window_hours` is **below** `max_trials`,
///                 caller is allowed to provision. Also returned if the
///                 aggregate timestamp is NULL (defensive) or malformed
///                 (logged at WARN).
/// * `Ok(Some(d))` — at-or-above cap. `d` is `anchor_trial + window_hours - now`,
///                  always >= 1s (anything 0 or negative collapses to None).
///
/// Edge cases handled:
/// * `max_trials == 0`: every trial puts the account at-or-above cap.
/// * `window_hours == 0`: no rows match the WHERE clause, returns None.
/// * Clock skew where the anchor is "in the future": cap at `window_hours`
///   so we don't sleep absurd durations.
#[derive(Debug, Clone, Copy)]
pub enum CooldownAnchor {
    Oldest,
    #[allow(dead_code)] // abstraction variant; Selectel uses Oldest (Latest is exercised in tests)
    Latest,
}

pub async fn get_cooldown_remaining(
    pool: &SqlitePool,
    provider_type: &str,
    account_id: i64,
    window_hours: u32,
    max_trials: u32,
    anchor: CooldownAnchor,
) -> Result<Option<std::time::Duration>, sqlx::Error> {
    let aggregator = match anchor {
        CooldownAnchor::Oldest => "MIN(created_at)",
        CooldownAnchor::Latest => "MAX(created_at)",
    };
    let sql = format!(
        "SELECT COUNT(*), {} FROM servers
         WHERE provider_type = ? AND provider_account_id = ?
           AND created_at > datetime('now', '-' || ? || ' hours')",
        aggregator
    );
    let row: (i64, Option<String>) = sqlx::query_as(&sql)
        .bind(provider_type)
        .bind(account_id)
        .bind(window_hours)
        .fetch_one(pool)
        .await?;

    let (count, anchor_str) = row;
    if count < max_trials as i64 {
        return Ok(None);
    }

    let Some(anchor_str) = anchor_str else {
        // Defensive — count > 0 should always have an aggregate, but if SQLite
        // ever surprises us, treat as "not in cooldown" rather than blocking forever.
        return Ok(None);
    };

    // SQLite `datetime('now')` returns 'YYYY-MM-DD HH:MM:SS' in UTC.
    let anchor_dt = match chrono::NaiveDateTime::parse_from_str(&anchor_str, "%Y-%m-%d %H:%M:%S") {
        Ok(d) => chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(d, chrono::Utc),
        Err(e) => {
            log::warn!("get_cooldown_remaining: bad created_at {:?}: {}", anchor_str, e);
            return Ok(None);
        }
    };

    let window = chrono::Duration::hours(window_hours as i64);
    let cooldown_end = anchor_dt + window;
    let now = chrono::Utc::now();

    let remaining_chrono = cooldown_end - now;
    if remaining_chrono <= chrono::Duration::zero() {
        return Ok(None);
    }

    // Cap at the window so a wildly-future created_at (clock skew, manual
    // edit) can't park the bot for centuries.
    let max_secs = window_hours as i64 * 3600;
    let secs = remaining_chrono.num_seconds().min(max_secs).max(1) as u64;
    Ok(Some(std::time::Duration::from_secs(secs)))
}

// ── Test URL CRUD ──

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct TestUrl {
    pub id: i64,
    pub url: String,
    pub is_active: bool,
    pub created_at: String,
}

pub async fn list_test_urls(pool: &SqlitePool) -> Result<Vec<TestUrl>, sqlx::Error> {
    let rows: Vec<(i64, String, bool, String)> = sqlx::query_as(
        "SELECT id, url, is_active, created_at FROM test_urls ORDER BY id"
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|r| TestUrl {
        id: r.0, url: r.1, is_active: r.2, created_at: r.3,
    }).collect())
}

// ── Bot settings ──

pub async fn get_bot_setting(pool: &SqlitePool, key: &str) -> Result<Option<String>, sqlx::Error> {
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT value FROM bot_settings WHERE key = ?"
    )
    .bind(key)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| r.0))
}

pub async fn set_bot_setting(pool: &SqlitePool, key: &str, value: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO bot_settings (key, value) VALUES (?, ?)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value"
    )
    .bind(key)
    .bind(value)
    .execute(pool)
    .await?;
    Ok(())
}

// ── OpenVPN-AS backend ("main server") credentials ──
//
// Managed from the "🖥 Main Server" admin screen and stored per-field in
// `bot_settings`. Every read goes through `get_ovpn_backend`, which falls back
// to the `.env` values (`OPENVPN_BACKEND_IP` etc.) for any field not yet set
// in the bot — so pre-existing env-only deployments keep working and the bot
// becomes the source of truth once a field is saved there.

pub const OVPN_KEY_IP: &str = "ovpn_backend_ip";
pub const OVPN_KEY_USER: &str = "ovpn_api_user";
pub const OVPN_KEY_PASS: &str = "ovpn_api_pass";
pub const OVPN_KEY_PORT: &str = "ovpn_api_port";

#[derive(Debug, Clone)]
pub struct OvpnBackend {
    pub ip: String,
    pub user: String,
    pub pass: String,
    pub port: u16,
}

impl OvpnBackend {
    pub fn is_configured(&self) -> bool {
        !self.ip.is_empty()
    }
}

/// Effective OVPN-AS backend config: per-field `bot_settings` value if set
/// (non-empty), else the `.env` value. DB errors degrade to the env fallback —
/// the provisioner must not wedge on a transient read failure.
pub async fn get_ovpn_backend(pool: &SqlitePool) -> OvpnBackend {
    let c = crate::config::get_config();
    let kv = |key: &'static str| async move {
        match get_bot_setting(pool, key).await {
            Ok(v) => v.filter(|s| !s.trim().is_empty()),
            Err(e) => {
                log::warn!("get_ovpn_backend({}): {} — using env fallback", key, e);
                None
            }
        }
    };
    OvpnBackend {
        ip: kv(OVPN_KEY_IP).await.unwrap_or_else(|| c.openvpn_backend_ip.clone()),
        user: kv(OVPN_KEY_USER).await.unwrap_or_else(|| c.openvpn_api_user.clone()),
        pass: kv(OVPN_KEY_PASS).await.unwrap_or_else(|| c.openvpn_api_pass.clone()),
        port: kv(OVPN_KEY_PORT).await
            .and_then(|s| s.parse().ok())
            .unwrap_or(c.openvpn_api_port),
    }
}

// ── Mail template (IP-change notification) ──
//
// Editable subject/body stored in bot_settings; empty/unset falls back to the
// Turkmen defaults in `crate::mailer`. Both support `{key}` placeholders (see
// `mailer::MAIL_TEMPLATE_KEYS`), rendered by `mailer::render_template`.

pub const MAIL_TPL_SUBJECT_KEY: &str = "mail_tpl_subject";
pub const MAIL_TPL_BODY_KEY: &str = "mail_tpl_body";

/// Effective (subject, body) template. Per-field: the admin-set value if
/// non-empty, else the built-in Turkmen default. DB errors degrade to defaults.
pub async fn get_mail_template(pool: &SqlitePool) -> (String, String) {
    let subject = get_bot_setting(pool, MAIL_TPL_SUBJECT_KEY).await
        .ok().flatten().filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| crate::mailer::DEFAULT_MAIL_SUBJECT.to_string());
    let body = get_bot_setting(pool, MAIL_TPL_BODY_KEY).await
        .ok().flatten().filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| crate::mailer::DEFAULT_MAIL_BODY.to_string());
    (subject, body)
}

/// Revert both template fields to the built-in defaults (store empty = "unset").
pub async fn reset_mail_template(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    set_bot_setting(pool, MAIL_TPL_SUBJECT_KEY, "").await?;
    set_bot_setting(pool, MAIL_TPL_BODY_KEY, "").await?;
    Ok(())
}

// ── Email list CRUD ──
// Recipients BCC'd by `openvpn_sync::publish_front_ip` whenever the active
// front IP changes. Managed from the "📧 Emails" admin screen.

#[derive(Debug, Clone)]
pub struct Email {
    pub id: i64,
    pub email: String,
}

/// Insert one email; returns true if inserted, false if it already existed.
pub async fn insert_email(pool: &SqlitePool, email: &str) -> Result<bool, sqlx::Error> {
    let result = sqlx::query("INSERT OR IGNORE INTO emails (email) VALUES (?)")
        .bind(email)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

pub async fn list_emails(pool: &SqlitePool) -> Result<Vec<Email>, sqlx::Error> {
    let rows: Vec<(i64, String)> = sqlx::query_as("SELECT id, email FROM emails ORDER BY id")
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(|r| Email { id: r.0, email: r.1 }).collect())
}

pub async fn delete_email(pool: &SqlitePool, id: i64) -> Result<bool, sqlx::Error> {
    let result = sqlx::query("DELETE FROM emails WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

// ── Per-(provider, account) operational state ──

/// What the admin UI surfaces below "no server" / "stuck" indicators —
/// "why isn't this account producing a working server right now?"
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ProviderAccountState {
    pub last_error_category: Option<String>,
    pub last_error_message: Option<String>,
    pub last_error_at: Option<String>,
    pub last_success_at: Option<String>,
}

pub async fn get_account_state(
    pool: &SqlitePool, provider_type: &str, account_id: i64,
) -> Result<Option<ProviderAccountState>, sqlx::Error> {
    let row: Option<(Option<String>, Option<String>, Option<String>, Option<String>)> =
        sqlx::query_as(
            "SELECT last_error_category, last_error_message, last_error_at, last_success_at
             FROM provider_account_state
             WHERE provider_type = ? AND account_id = ?"
        )
        .bind(provider_type)
        .bind(account_id)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|r| ProviderAccountState {
        last_error_category: r.0,
        last_error_message: r.1,
        last_error_at: r.2,
        last_success_at: r.3,
    }))
}

/// Record an unsuccessful cycle. Upserts the row.
pub async fn record_account_error(
    pool: &SqlitePool, provider_type: &str, account_id: i64,
    category: &str, message: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO provider_account_state (provider_type, account_id, last_error_category, last_error_message, last_error_at)
         VALUES (?, ?, ?, ?, datetime('now'))
         ON CONFLICT(provider_type, account_id) DO UPDATE SET
           last_error_category = excluded.last_error_category,
           last_error_message  = excluded.last_error_message,
           last_error_at       = excluded.last_error_at"
    )
    .bind(provider_type)
    .bind(account_id)
    .bind(category)
    .bind(message)
    .execute(pool)
    .await?;
    Ok(())
}

/// Record a successful cycle and clear any stale error state.
pub async fn record_account_success(
    pool: &SqlitePool, provider_type: &str, account_id: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO provider_account_state (provider_type, account_id, last_success_at)
         VALUES (?, ?, datetime('now'))
         ON CONFLICT(provider_type, account_id) DO UPDATE SET
           last_success_at     = excluded.last_success_at,
           last_error_category = NULL,
           last_error_message  = NULL,
           last_error_at       = NULL"
    )
    .bind(provider_type)
    .bind(account_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Stash the region/location used to create this server. Used at sync time to
/// derive a user-friendly country label for the host remark — see
/// `crate::utils::region_to_country`.
pub async fn set_server_region(pool: &SqlitePool, id: i64, region: &str) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE servers SET region = ? WHERE id = ?")
        .bind(region)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn list_active_test_urls(pool: &SqlitePool) -> Result<Vec<String>, sqlx::Error> {
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT url FROM test_urls WHERE is_active = 1 ORDER BY id"
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|r| r.0).collect())
}

pub async fn add_test_url(pool: &SqlitePool, url: &str) -> Result<i64, sqlx::Error> {
    let row: (i64,) = sqlx::query_as(
        "INSERT INTO test_urls (url) VALUES (?) RETURNING id"
    )
    .bind(url)
    .fetch_one(pool)
    .await?;
    Ok(row.0)
}

pub async fn delete_test_url(pool: &SqlitePool, id: i64) -> Result<bool, sqlx::Error> {
    let result = sqlx::query("DELETE FROM test_urls WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

pub async fn count_active_test_urls(pool: &SqlitePool) -> Result<i64, sqlx::Error> {
    let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM test_urls WHERE is_active = 1")
        .fetch_one(pool)
        .await?;
    Ok(row.0)
}

#[cfg(test)]
mod cooldown_tests {
    //! Tests for `get_cooldown_remaining` — the CountBased rate-limit gate in
    //! front of every real-money `create_server`. A bug here either spins
    //! servers in a loop (the Beget "oscillation" class) or silently never
    //! provisions, so the window/anchor/clamp behavior is pinned here.
    //!
    //! Run against an in-memory SQLite DB with a minimal `servers` table (only
    //! the columns the query touches). Timestamps are seeded *relative to now*
    //! via `datetime('now', <offset>)` so the tests are deterministic without
    //! mocking the clock; assertions use generous ranges to absorb execution
    //! time. NOTE: the table is hand-written, so it won't catch drift from the
    //! real `migrations/` schema — it's scoped to this function's query.

    use super::{get_cooldown_remaining, CooldownAnchor};
    use sqlx::sqlite::SqlitePoolOptions;
    use sqlx::SqlitePool;

    const H: u64 = 3600;

    async fn mem_pool() -> SqlitePool {
        // max+min connections = 1 so the single in-memory DB persists across
        // queries (a fresh connection would see an empty DB).
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .min_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE servers (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                provider_type TEXT NOT NULL,
                provider_account_id INTEGER NOT NULL,
                created_at TEXT NOT NULL,
                deleted_at TEXT
            )",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool
    }

    /// Insert a trial whose `created_at` is `offset` relative to now,
    /// e.g. "-2 hours", "-90 minutes", "+100 hours".
    async fn insert_trial(pool: &SqlitePool, provider: &str, account_id: i64, offset: &str) {
        sqlx::query(
            "INSERT INTO servers (provider_type, provider_account_id, created_at)
             VALUES (?, ?, datetime('now', ?))",
        )
        .bind(provider)
        .bind(account_id)
        .bind(offset)
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn below_cap_returns_none() {
        let pool = mem_pool().await;
        insert_trial(&pool, "beget", 1, "-5 minutes").await;
        insert_trial(&pool, "beget", 1, "-10 minutes").await;
        let r = get_cooldown_remaining(&pool, "beget", 1, 24, 3, CooldownAnchor::Oldest)
            .await
            .unwrap();
        assert!(r.is_none(), "2 trials below cap 3 → not in cooldown");
    }

    #[tokio::test]
    async fn at_cap_returns_remaining_near_window() {
        let pool = mem_pool().await;
        for _ in 0..3 {
            insert_trial(&pool, "beget", 1, "-1 minutes").await;
        }
        let s = get_cooldown_remaining(&pool, "beget", 1, 24, 3, CooldownAnchor::Oldest)
            .await
            .unwrap()
            .expect("at cap → Some")
            .as_secs();
        // oldest ≈1 min ago, + 24h window − now ≈ just under 24h.
        assert!(s > 24 * H - 600 && s <= 24 * H, "expected ~24h, got {s}s");
    }

    #[tokio::test]
    async fn oldest_and_latest_anchors_differ() {
        let pool = mem_pool().await;
        insert_trial(&pool, "selectel", 7, "-10 hours").await;
        insert_trial(&pool, "selectel", 7, "-1 hours").await;
        let oldest = get_cooldown_remaining(&pool, "selectel", 7, 24, 2, CooldownAnchor::Oldest)
            .await
            .unwrap()
            .unwrap()
            .as_secs();
        let latest = get_cooldown_remaining(&pool, "selectel", 7, 24, 2, CooldownAnchor::Latest)
            .await
            .unwrap()
            .unwrap()
            .as_secs();
        // Oldest: (−10h)+24h ≈ 14h remaining. Latest: (−1h)+24h ≈ 23h remaining.
        assert!((13 * H..15 * H).contains(&oldest), "oldest ~14h, got {oldest}s");
        assert!((22 * H..24 * H).contains(&latest), "latest ~23h, got {latest}s");
        assert!(latest > oldest, "latest anchor must wait longer than oldest");
    }

    #[tokio::test]
    async fn trial_outside_window_is_not_counted() {
        // The anti-oscillation invariant: a trial older than the window rolls
        // off, so the count reflects only the current window.
        let pool = mem_pool().await;
        insert_trial(&pool, "vultr", 2, "-25 hours").await; // outside 24h
        insert_trial(&pool, "vultr", 2, "-1 hours").await; // inside
        let r = get_cooldown_remaining(&pool, "vultr", 2, 24, 2, CooldownAnchor::Oldest)
            .await
            .unwrap();
        assert!(r.is_none(), "only 1 trial inside the window → below cap of 2");
    }

    #[tokio::test]
    async fn count_is_isolated_by_provider_and_account() {
        let pool = mem_pool().await;
        for _ in 0..3 {
            insert_trial(&pool, "beget", 1, "-1 minutes").await;
        }
        // Same provider, different account.
        assert!(get_cooldown_remaining(&pool, "beget", 2, 24, 3, CooldownAnchor::Oldest)
            .await
            .unwrap()
            .is_none());
        // Different provider, same account id.
        assert!(get_cooldown_remaining(&pool, "vultr", 1, 24, 3, CooldownAnchor::Oldest)
            .await
            .unwrap()
            .is_none());
        // The actual (provider, account) is in cooldown.
        assert!(get_cooldown_remaining(&pool, "beget", 1, 24, 3, CooldownAnchor::Oldest)
            .await
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn future_timestamp_clamps_to_window() {
        let pool = mem_pool().await;
        insert_trial(&pool, "vdsina", 5, "+100 hours").await; // clock skew
        let r = get_cooldown_remaining(&pool, "vdsina", 5, 24, 1, CooldownAnchor::Latest)
            .await
            .unwrap();
        // Raw remaining would be ~124h; capped at the 24h window.
        assert_eq!(r.map(|d| d.as_secs()), Some(24 * H));
    }

    #[tokio::test]
    async fn max_trials_zero_means_any_trial_is_at_cap() {
        let pool = mem_pool().await;
        insert_trial(&pool, "upcloud", 3, "-1 minutes").await;
        assert!(
            get_cooldown_remaining(&pool, "upcloud", 3, 24, 0, CooldownAnchor::Oldest)
                .await
                .unwrap()
                .is_some(),
            "max_trials=0 → a single trial is at-or-above cap"
        );
        // ...but with zero trials there's nothing to anchor → None.
        assert!(
            get_cooldown_remaining(&pool, "upcloud", 99, 24, 0, CooldownAnchor::Oldest)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn window_zero_matches_no_rows() {
        let pool = mem_pool().await;
        insert_trial(&pool, "gcore", 4, "-1 minutes").await;
        // window 0 → WHERE created_at > now → past rows excluded → count 0.
        let r = get_cooldown_remaining(&pool, "gcore", 4, 0, 1, CooldownAnchor::Oldest)
            .await
            .unwrap();
        assert!(r.is_none());
    }

    #[tokio::test]
    async fn deleted_trials_still_count_toward_the_limit() {
        // A burned-then-deleted IP is still a trial; the query intentionally
        // has no deleted_at filter.
        let pool = mem_pool().await;
        sqlx::query(
            "INSERT INTO servers (provider_type, provider_account_id, created_at, deleted_at)
             VALUES ('beget', 8, datetime('now','-1 minutes'), datetime('now'))",
        )
        .execute(&pool)
        .await
        .unwrap();
        let r = get_cooldown_remaining(&pool, "beget", 8, 24, 1, CooldownAnchor::Oldest)
            .await
            .unwrap();
        assert!(r.is_some(), "deleted trials must still count");
    }

    #[tokio::test]
    async fn malformed_anchor_timestamp_is_defensive_none() {
        // A non-date string sorts after any real 'YYYY-…' value, so it passes
        // the WHERE bound and becomes the anchor; parsing it fails and the
        // function returns None rather than blocking the account forever.
        let pool = mem_pool().await;
        sqlx::query(
            "INSERT INTO servers (provider_type, provider_account_id, created_at)
             VALUES ('beget', 9, 'zzzz-not-a-date')",
        )
        .execute(&pool)
        .await
        .unwrap();
        let r = get_cooldown_remaining(&pool, "beget", 9, 24, 1, CooldownAnchor::Oldest)
            .await
            .unwrap();
        assert!(r.is_none(), "unparseable anchor → defensive None");
    }
}
