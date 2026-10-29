//! Provisioner subsystem — the per-account lifecycle manager for cloud-rented
//! VPN exit servers.
//!
//! Layout:
//!   * `manager`      — 30s loop that fans out per-account `AccountProvisioner` tasks
//!   * `account`      — the per-(provider, account) state machine
//!   * `openvpn_sync` — publishes the current front IP to the OpenVPN-AS backend
//!   * `errors`       — error categorization for admin-notification dedup
//!
//! The SSH/sshpass helpers and admin-notification sender live at the crate root
//! (`crate::ssh`, `crate::notify`) — general infrastructure used by the
//! provisioner.

mod account;
mod errors;
mod manager;
mod openvpn_sync;

pub use account::{cleanup_server, delete_account_servers};
pub use manager::{request_retry, spawn_provisioner};
pub use openvpn_sync::publish_front_ip;

use sqlx::SqlitePool;

use crate::db;
use crate::utils::ssh_user_for;

/// Reset the DNAT path on every live deployed front: reboot the VM (clears any
/// runtime iptables/OS state), clear its deployed+published flags, and wake the
/// account's provisioner. Once SSH is back, the state machine re-runs
/// `test_and_deploy`: the DNAT script flushes and re-applies against the
/// CURRENT backend IP, the warm-pool pass restores the per-source MASQUERADE
/// exemptions, and the front is republished to the backend.
///
/// Used by the admin "🔁 Reset DNAT" button and automatically when the backend
/// IP is changed in the bot. Best-effort per server: a failed reboot (SSH
/// already dead, box hung) still clears the flags, so the re-deploy fires as
/// soon as the server answers again.
///
/// Returns one human-readable summary line per acted-on front (empty when no
/// deployed front exists).
pub async fn reset_front_dnat(pool: &SqlitePool) -> Vec<String> {
    let servers = match db::get_all_active_servers(pool).await {
        Ok(s) => s,
        Err(e) => {
            log::error!("reset_front_dnat: could not list servers: {}", e);
            return vec![format!("DB error listing servers: {}", e)];
        }
    };

    let mut lines = Vec::new();
    for srv in servers.iter().filter(|s| s.script_deployed) {
        let (Some(ip), Some(pass)) = (srv.ipv4.as_deref(), srv.root_pass.as_deref()) else {
            continue;
        };

        let reboot_note = match crate::ssh::reboot_server(ip, pass, ssh_user_for(&srv.provider_type)).await {
            Ok(()) => "rebooting".to_string(),
            Err(e) => {
                log::warn!("reset_front_dnat: reboot {} failed: {}", ip, e);
                format!("reboot failed ({})", e)
            }
        };

        // Clear the flags even if the reboot command failed — the whole point
        // is to force a full DNAT re-deploy on the next reachable cycle.
        if let Err(e) = db::unmark_server_deployed(pool, srv.id).await {
            log::error!("reset_front_dnat: unmark_server_deployed({}): {}", srv.id, e);
            lines.push(format!("front {} (#{}): DB error, NOT reset: {}", ip, srv.id, e));
            continue;
        }
        request_retry(&srv.provider_type, srv.provider_account_id).await;

        log::info!("reset_front_dnat: front {} (#{}) {} — DNAT re-deploy queued", ip, srv.id, reboot_note);
        lines.push(format!(
            "front {} (#{}): {}, DNAT + publish re-apply automatically once it's back (~1–2 min)",
            ip, srv.id, reboot_note,
        ));
    }
    lines
}
