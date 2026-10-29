//! The provisioner manager — a 30s loop that enumerates every active account
//! across every provider and spawns/stops `AccountProvisioner` tasks accordingly.
//!
//! Also owns the `RETRY_NOTIFIES` registry that backs the admin-clickable
//! "🔄 Retry now" inline button — `request_retry` looks up the per-account
//! `Notify` and fires it; the `AccountProvisioner::run` loop wakes from sleep
//! and skips the cooldown for one cycle.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::LazyLock;
use std::time::Duration;

use sqlx::SqlitePool;
use teloxide::prelude::*;
use tokio::sync::{watch, Notify};

use crate::provider::CloudProvider;
use crate::providers;

use super::account::AccountProvisioner;

// ── Per-account retry-now signal registry ──
//
// Admin clicks the "🔄 Retry now" button → main bot routes the callback to
// `provisioner::request_retry(provider_type, account_id)` → that function
// looks up the account's Notify and fires it → the AccountProvisioner's run
// loop breaks out of its sleep, clears any rate-limit cooldown, and runs a
// fresh cycle immediately.
static RETRY_NOTIFIES: LazyLock<tokio::sync::Mutex<HashMap<(String, i64), Arc<Notify>>>> =
    LazyLock::new(|| tokio::sync::Mutex::new(HashMap::new()));

/// Public — fire the retry signal for an account. Returns true if a running
/// AccountProvisioner accepted the signal, false if no such account is alive.
pub async fn request_retry(provider_type: &str, account_id: i64) -> bool {
    let key = (provider_type.to_string(), account_id);
    let guard = RETRY_NOTIFIES.lock().await;
    if let Some(n) = guard.get(&key) {
        n.notify_one();
        true
    } else {
        false
    }
}

/// Spawn the provisioner manager. Manages per-account provisioner loops.
pub fn spawn_provisioner(bot: Bot, pool: SqlitePool, shutdown: tokio::sync::watch::Receiver<bool>) {
    tokio::spawn(provisioner_manager(bot, pool, shutdown));
}

/// Key for tracking provisioner instances: (provider_type, account_id)
type ProvisionerKey = (String, i64);

async fn provisioner_manager(bot: Bot, pool: SqlitePool, mut shutdown: watch::Receiver<bool>) {
    log::info!("Provisioner manager started");

    // Track each running provisioner with the location it was spawned with so
    // we can detect region/location changes and respawn with the new client.
    let mut account_handles: HashMap<ProvisionerKey, (tokio::task::JoinHandle<()>, watch::Sender<bool>, String)> = HashMap::new();

    loop {
        // Collect all active accounts from all providers
        let mut desired: HashMap<ProvisionerKey, (String, String, Box<dyn CloudProvider>)> = HashMap::new();

        for module in providers::all() {
            if let Ok(accounts) = module.list_active_accounts(&pool).await {
                for (acct, provider) in accounts {
                    desired.insert(
                        (module.provider_type().to_string(), acct.id),
                        (acct.name, acct.location, provider),
                    );
                }
            }
        }

        // Stop provisioners for deactivated accounts OR accounts whose location changed
        let to_remove: Vec<ProvisionerKey> = account_handles.iter()
            .filter(|(key, (_, _, current_loc))| {
                match desired.get(*key) {
                    None => true, // deactivated
                    Some((_, new_loc, _)) => new_loc != current_loc, // region changed
                }
            })
            .map(|(k, _)| k.clone())
            .collect();
        for key in to_remove {
            if let Some((handle, stop_tx, old_loc)) = account_handles.remove(&key) {
                let reason = match desired.get(&key) {
                    Some((_, new_loc, _)) => format!("region change {} → {}", old_loc, new_loc),
                    None => "deactivated".to_string(),
                };
                log::info!("Stopping provisioner for {:?}: {}", key, reason);
                stop_tx.send(true).ok();
                handle.abort();
                RETRY_NOTIFIES.lock().await.remove(&key);
            }
        }

        // Spawn provisioners for new accounts (and respawned ones)
        for (key, (name, location, provider)) in desired {
            if !account_handles.contains_key(&key) {
                log::info!("Spawning provisioner for {} account {} ({}, region={})",
                    key.0, key.1, name, location);
                let (stop_tx, stop_rx) = watch::channel(false);
                let location_clone = location.clone();
                let retry_notify = Arc::new(Notify::new());
                {
                    let mut g = RETRY_NOTIFIES.lock().await;
                    g.insert(key.clone(), retry_notify.clone());
                }
                let ap = AccountProvisioner::new(
                    key.0.clone(),
                    key.1,
                    name,
                    location,
                    provider,
                    pool.clone(),
                    bot.clone(),
                    stop_rx,
                    retry_notify,
                );
                let handle = tokio::spawn(ap.run());
                account_handles.insert(key, (handle, stop_tx, location_clone));
            }
        }

        if account_handles.is_empty() {
            log::debug!("No active accounts, provisioner manager waiting...");
        }

        // Wait 30s or shutdown
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(30)) => {}
            _ = shutdown.changed() => {
                log::info!("Provisioner manager received shutdown signal");
                break;
            }
        }
    }

    // Stop all account provisioners
    for (key, (handle, stop_tx, _loc)) in account_handles {
        log::info!("Stopping provisioner for {:?} (shutdown)", key);
        stop_tx.send(true).ok();
        handle.abort();
    }

    log::info!("Provisioner manager stopped");
}
