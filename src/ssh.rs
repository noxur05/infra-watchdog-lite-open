//! SSH/sshpass helpers — generic infrastructure for running commands on remote
//! provider-rented servers. The bot itself doesn't run inside Turkmenistan;
//! these helpers SSH (via `sshpass`) into provisioned servers to perform
//! curl-test probes, set up DNAT proxy chains, and read traffic counters.
//!
//! All commands are wrapped via `sudo_wrap` so a non-root login (e.g. Gcore's
//! `ubuntu` user with passwordless sudo) still runs the script as root.
//! Children use `kill_on_drop(true)` so a cancelled future never leaks an
//! orphaned `sshpass` / `ssh` process; an outer `tokio::time::timeout` bounds
//! the total wall-clock so a stuck remote command can't park a Tokio task
//! forever.

use std::sync::OnceLock;
use std::time::Duration;
use sqlx::SqlitePool;
use tokio::process::Command;

use crate::config::get_config;
use crate::db;

/// Path to the bot's materialized private SSH key (set at startup by
/// `ensure_bot_ssh_key`). When present, every SSH offers it FIRST (key auth),
/// falling back to the `sshpass` password — so a server where the bot installed
/// its public key (manual servers) stays reachable even after a provider resets
/// the root password on reboot (e.g. Beget, which doesn't touch authorized_keys).
/// Servers without the key transparently fall back to password auth.
static BOT_SSH_KEY_PATH: OnceLock<String> = OnceLock::new();

pub(crate) fn bot_ssh_key_path() -> Option<&'static str> {
    BOT_SSH_KEY_PATH.get().map(|s| s.as_str())
}

/// Shared `sshpass … ssh …` args up to and including `user@ip` (caller appends
/// the remote command). Adds the bot key (key-first, password fallback) when
/// one is materialized.
pub(crate) fn ssh_base_args(ssh_user: &str, ip: &str) -> Vec<String> {
    let mut a: Vec<String> = vec![
        "-e".into(), "ssh".into(),
        "-o".into(), "StrictHostKeyChecking=no".into(),
        "-o".into(), "UserKnownHostsFile=/dev/null".into(),
        "-o".into(), "ConnectTimeout=15".into(),
    ];
    if let Some(key) = bot_ssh_key_path() {
        // IdentitiesOnly so the bot host's own keys aren't offered (avoids
        // "too many authentication failures"); password still works via sshpass.
        a.push("-i".into());
        a.push(key.to_string());
        a.push("-o".into());
        a.push("IdentitiesOnly=yes".into());
    }
    a.push(format!("{}@{}", ssh_user, ip));
    a
}

/// Generate an ed25519 keypair via ssh-keygen → (private_pem, public_line).
async fn generate_keypair() -> Result<(String, String), String> {
    let base = std::env::temp_dir().join(format!("vpnwd-key-{}", std::process::id()));
    let priv_path = base.to_string_lossy().to_string();
    let pub_path = format!("{}.pub", priv_path);
    let _ = std::fs::remove_file(&priv_path);
    let _ = std::fs::remove_file(&pub_path);
    let out = Command::new("ssh-keygen")
        .args(["-t", "ed25519", "-N", "", "-C", "vpn-watchdog", "-f", &priv_path])
        .output().await.map_err(|e| format!("ssh-keygen spawn: {}", e))?;
    if !out.status.success() {
        return Err(format!("ssh-keygen: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    let priv_key = std::fs::read_to_string(&priv_path).map_err(|e| format!("read priv key: {}", e))?;
    let pub_key = std::fs::read_to_string(&pub_path).map_err(|e| format!("read pub key: {}", e))?;
    let _ = std::fs::remove_file(&priv_path);
    let _ = std::fs::remove_file(&pub_path);
    Ok((priv_key, pub_key.trim().to_string()))
}

/// Ensure the bot has an SSH keypair (generate + persist in `bot_settings` on
/// first use), materialize the private key to a 0600 file, set the global path,
/// and return the public key line. Idempotent.
pub(crate) async fn ensure_bot_ssh_key(pool: &SqlitePool) -> Result<String, String> {
    let pu_existing = db::get_bot_setting(pool, "bot_ssh_public_key").await.map_err(|e| e.to_string())?;
    // Already materialized this run — return the pubkey, skip re-writing the file
    // (avoids the concurrent-write race between startup and a manual-setup call).
    if BOT_SSH_KEY_PATH.get().is_some() {
        if let Some(pu) = pu_existing.as_ref().filter(|s| !s.trim().is_empty()) {
            return Ok(pu.clone());
        }
    }

    let pr = db::get_bot_setting(pool, "bot_ssh_private_key").await.map_err(|e| e.to_string())?;
    let (priv_key, pub_key) = match (pr, pu_existing) {
        (Some(a), Some(b)) if !a.trim().is_empty() && !b.trim().is_empty() => (a, b),
        _ => {
            let (a, b) = generate_keypair().await?;
            db::set_bot_setting(pool, "bot_ssh_private_key", &a).await.map_err(|e| e.to_string())?;
            db::set_bot_setting(pool, "bot_ssh_public_key", &b).await.map_err(|e| e.to_string())?;
            log::info!("generated bot SSH keypair (stored in bot_settings)");
            (a, b)
        }
    };
    let path = std::env::current_dir().map_err(|e| e.to_string())?.join(".bot_ssh_key");
    // Create the private-key file 0600 ATOMICALLY (no world-readable window).
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true).create(true).truncate(true).mode(0o600)
            .open(&path).map_err(|e| format!("open key file: {}", e))?;
        f.write_all(priv_key.as_bytes()).map_err(|e| format!("write key file: {}", e))?;
    }
    #[cfg(not(unix))]
    std::fs::write(&path, priv_key.as_bytes()).map_err(|e| format!("write key file: {}", e))?;
    let _ = BOT_SSH_KEY_PATH.set(path.to_string_lossy().to_string());
    Ok(pub_key)
}

pub(crate) const SSH_TIMEOUT_SECS: u64 = 60;
/// Outer wall for `deploy_destination_with_timeout`. Needs to cover the
/// retry budget inside `ssh_exec_with_retry(.., 3)` — worst case
/// 3 × SSH_TIMEOUT (60s) + backoffs (2s+5s) = 187s — plus headroom.
/// Previously 120s, which cut off attempt 3 on truly lossy paths (the
/// exact scenario the retry was added for).
const DEPLOY_TIMEOUT_SECS: u64 = 240;

/// Wrap a string in single quotes for safe interpolation into a shell script,
/// escaping any embedded single quotes with the standard `'\''` trick.
pub(crate) fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Wrap a script so it runs as root regardless of the SSH login user.
/// For `ssh_user == "root"` this is a no-op; for non-root logins (e.g. Gcore's
/// `ubuntu` with passwordless sudo) the script is wrapped as
/// `sudo bash -c '<script>'`, with embedded single quotes escaped via the
/// standard `'\''` trick. The bot's existing scripts touch /var/lib, run
/// docker/apt-get/iptables, etc. — all need root on the remote.
pub(crate) fn sudo_wrap(ssh_user: &str, script: &str) -> String {
    if ssh_user == "root" {
        script.to_string()
    } else {
        format!("sudo bash -c '{}'", script.replace('\'', "'\\''"))
    }
}

/// Read the active test-probe URLs from the DB. Falls back to the env-config
/// list if the DB has none (defensive — the migration seeds the table, but a
/// runtime hiccup or admin deletion shouldn't kill the bot).
pub(crate) async fn fetch_test_urls(pool: &SqlitePool) -> Vec<String> {
    match db::list_active_test_urls(pool).await {
        Ok(u) if !u.is_empty() => u,
        Ok(_) => get_config().test_urls.clone(),
        Err(e) => {
            log::warn!("test_urls db read failed: {} — falling back to config", e);
            get_config().test_urls.clone()
        }
    }
}

/// Run a curl test from the remote server via SSH.
/// Tries each URL in order; passes if any one succeeds.
/// URLs are passed via shell variable assignment so they can never break out
/// of the script context, even when they originate from admin-editable input.
/// Run an already-wrapped `remote_cmd` on the server via sshpass with the
/// global SSH timeout, returning the raw process Output. The exact
/// "SSH command timed out" wording is load-bearing: `is_transient_ssh_error`
/// matches on it to decide retry-vs-fail.
async fn run_sshpass_output(
    ip: &str, password: &str, ssh_user: &str, remote_cmd: String,
) -> Result<std::process::Output, String> {
    let mut args = ssh_base_args(ssh_user, ip);
    args.push(remote_cmd);
    tokio::time::timeout(
        Duration::from_secs(SSH_TIMEOUT_SECS),
        Command::new("sshpass")
            .env("SSHPASS", password)
            .args(&args)
            .kill_on_drop(true)
            .output()
    ).await
    .map_err(|_| "SSH command timed out".to_string())?
    .map_err(|e| format!("SSH exec error: {}", e))
}

/// Lightweight reachability probe: can the bot SSH in and run a trivial command?
/// Used to detect when a floating IP is unreachable *from the bot host* (Selectel
/// per-IP ingress routing gaps) so the caller can fail the active IP over to a
/// reachable pool member. True only on a successful connect + auth + command.
pub(crate) async fn probe_ssh(ip: &str, password: &str, ssh_user: &str) -> bool {
    match run_sshpass_output(ip, password, ssh_user, "true".to_string()).await {
        Ok(out) => out.status.success(),
        Err(_) => false,
    }
}

pub(crate) async fn ssh_curl_test(ip: &str, password: &str, ssh_user: &str, test_urls: &[String]) -> Result<bool, String> {
    let mut url_checks = String::new();
    for url in test_urls {
        url_checks.push_str(&format!(
            r#"URL={q}
HTTP_CODE=$(curl -s --max-time 8 -o /dev/null -w '%{{http_code}}' "$URL" 2>/dev/null)
if [ "$HTTP_CODE" -ge 200 ] && [ "$HTTP_CODE" -lt 400 ] 2>/dev/null; then
    echo "PASS:$HTTP_CODE:$URL"
    exit 0
fi
"#,
            q = shell_quote(url),
        ));
    }
    url_checks.push_str("echo \"FAIL:all\"\nexit 1\n");

    let script = url_checks;
    let remote_cmd = sudo_wrap(ssh_user, &script);
    let result = run_sshpass_output(ip, password, ssh_user, remote_cmd).await?;

    let stdout = String::from_utf8_lossy(&result.stdout);
    let stderr = String::from_utf8_lossy(&result.stderr);

    if !result.status.success() {
        if stderr.contains("Permission denied") || stderr.contains("Connection refused")
            || stderr.contains("Connection timed out") || stderr.contains("No route to host")
        {
            return Err(format!("SSH connection failed: {}", stderr.trim()));
        }
        log::info!("Curl test result: {}", stdout.trim());
        return Ok(false);
    }

    log::info!("Curl test result: {}", stdout.trim());
    Ok(stdout.trim().starts_with("PASS"))
}

/// Like `ssh_curl_test` but sources each request from a specific local address
/// (`curl --interface <source_ip>`). Used by the Selectel warm-pool to test
/// each floating IP individually: a request sourced from a pool member's fixed
/// IP egresses via that member's floating IP (Neutron 1:1 NAT), so PASS/FAIL
/// reflects whether *that* floating IP is blocked from TM.
///
/// `source_ip` is a Neutron-allocated fixed IP (trusted, IP-shaped) — it is
/// interpolated into the remote script verbatim.
pub(crate) async fn ssh_curl_test_from(
    ip: &str, password: &str, ssh_user: &str, test_urls: &[String], source_ip: &str,
) -> Result<bool, String> {
    let mut url_checks = String::new();
    for url in test_urls {
        url_checks.push_str(&format!(
            r#"URL={q}
HTTP_CODE=$(curl -s --interface {src} --max-time 8 -o /dev/null -w '%{{http_code}}' "$URL" 2>/dev/null)
if [ "$HTTP_CODE" -ge 200 ] && [ "$HTTP_CODE" -lt 400 ] 2>/dev/null; then
    echo "PASS:$HTTP_CODE:$URL"
    exit 0
fi
"#,
            q = shell_quote(url),
            src = source_ip,
        ));
    }
    url_checks.push_str("echo \"FAIL:all\"\nexit 1\n");

    let remote_cmd = sudo_wrap(ssh_user, &url_checks);
    let result = run_sshpass_output(ip, password, ssh_user, remote_cmd).await?;

    let stdout = String::from_utf8_lossy(&result.stdout);
    let stderr = String::from_utf8_lossy(&result.stderr);

    if !result.status.success() {
        if stderr.contains("Permission denied") || stderr.contains("Connection refused")
            || stderr.contains("Connection timed out") || stderr.contains("No route to host")
        {
            return Err(format!("SSH connection failed: {}", stderr.trim()));
        }
        log::info!("Pool curl test ({}): {}", source_ip, stdout.trim());
        return Ok(false);
    }

    log::info!("Pool curl test ({}): {}", source_ip, stdout.trim());
    Ok(stdout.trim().starts_with("PASS"))
}

/// Add the warm-pool's extra fixed IPs to the front's primary NIC so
/// `curl --interface <fixed_ip>` can source from each (the one the server got
/// via DHCP is already present; `ip addr add` of a duplicate is a harmless
/// no-op). The subnet is the bot-created vpn-subnet (/24). Idempotent.
///
/// Also inserts a per-source `-j RETURN` exemption at the TOP of the nat
/// POSTROUTING chain for every pool fixed IP. Without it, the deploy script's
/// blanket MASQUERADE rewrites a locally-generated packet's source to the
/// interface's PRIMARY address, collapsing every `curl --interface <fixed_ip>`
/// test onto the primary floating IP — the per-IP tests would all silently
/// probe the ACTIVE member. The exemption only matches traffic sourced from
/// the pool fixed IPs themselves (our tests + normal primary-IP egress, where
/// skipping MASQUERADE is a behavioral no-op); DNAT-forwarded client traffic
/// keeps its client source and still hits the MASQUERADE rules below.
/// Rules are persisted once (on first insert) so a reboot restores them.
///
/// The extra addresses are ALSO persisted across reboots via a systemd oneshot
/// unit (`watchdog-extra-ips.service`). DHCP only configures the port's primary
/// fixed IP at boot; if the ACTIVE floating IP maps to a secondary fixed IP
/// (post-failover) and the box reboots without this unit, Neutron delivers
/// mgmt SSH to an address the guest no longer carries — the bot is locked out
/// of its own front. The DNAT-reset flow deliberately reboots the front, so
/// this persistence is load-bearing, not a nicety.
///
/// `fixed_ips` are Neutron-allocated addresses (trusted, IP-shaped).
pub(crate) async fn configure_extra_ips(
    ip: &str, password: &str, ssh_user: &str, fixed_ips: &[String],
) -> Result<(), String> {
    let mut adds = String::new();
    for fx in fixed_ips {
        adds.push_str(&format!("ip addr add {fx}/24 dev \"$IFACE\" 2>/dev/null || true\n", fx = fx));
        adds.push_str(&format!(
            "iptables -t nat -C POSTROUTING -s {fx}/32 -j RETURN 2>/dev/null || {{ iptables -t nat -I POSTROUTING -s {fx}/32 -j RETURN; ADDED=1; }}\n",
            fx = fx,
        ));
    }
    // Boot-time script re-adds the same addresses (the systemd unit runs it
    // after network-online). Regenerated every pass so it always reflects the
    // current pool membership.
    let mut boot_adds = String::new();
    for fx in fixed_ips {
        boot_adds.push_str(&format!("ip addr add {fx}/24 dev \"$IFACE\" 2>/dev/null || true\n", fx = fx));
    }
    let script = format!(
        r#"IFACE=$(ip route get 8.8.8.8 | awk '{{print $5; exit}}')
ADDED=0
{adds}cat > /usr/local/sbin/watchdog-extra-ips.sh <<'WEOF'
#!/bin/sh
IFACE=$(ip route get 8.8.8.8 | awk '{{print $5; exit}}')
{boot_adds}WEOF
chmod 0755 /usr/local/sbin/watchdog-extra-ips.sh
if [ ! -f /etc/systemd/system/watchdog-extra-ips.service ]; then
    cat > /etc/systemd/system/watchdog-extra-ips.service <<'WEOF'
[Unit]
Description=Re-add vpn-watchdog warm-pool fixed IPs after boot
After=network-online.target
Wants=network-online.target

[Service]
Type=oneshot
ExecStart=/usr/local/sbin/watchdog-extra-ips.sh

[Install]
WantedBy=multi-user.target
WEOF
    systemctl daemon-reload 2>/dev/null || true
    systemctl enable watchdog-extra-ips.service 2>/dev/null || true
fi
if [ "$ADDED" = "1" ]; then
    mkdir -p /etc/iptables
    iptables-save > /etc/iptables/rules.v4
fi
"#,
        adds = adds,
        boot_adds = boot_adds,
    );
    ssh_exec(ip, password, ssh_user, &script).await
}

/// Classify an SSH error as transient (worth retrying) vs permanent.
///
/// Transient = the kind that can clear on its own within seconds:
///   * `SSH command timed out` — our outer tokio::time::timeout fired
///     (typically because of packet loss on the kex/auth packets)
///   * `Connection reset` / `Connection closed by ... port 22` —
///     sshd's MaxStartups rolling the dice and rejecting the kex
///   * `kex_exchange_identification: Connection closed by remote host` —
///     same MaxStartups behavior, more specific message
///   * `Connection timed out during banner exchange` — TCP succeeded but
///     sshd never sent its version banner within the window (path issue)
///   * `Connection timed out` (generic) — TCP-level connect timed out
///
/// Permanent = config or credential issues that won't fix themselves:
///   * `Permission denied` — wrong password / disabled auth
///   * `Host key verification failed` — host changed
///   * `command not found` — script issue
pub(crate) fn is_transient_ssh_error(msg: &str) -> bool {
    // NOTE: deliberately does NOT match "Connection refused" — that string
    // means sshd is not listening on port 22, which is the permanent state
    // for a decommissioned server or wrong port. Retrying it just burns
    // 3 × SSH_TIMEOUT against a dead box. The one legitimate transient case
    // (post-reboot manual setup) has its own retry loop in
    // `providers/manual/setup.rs` that uses `ssh_exec` directly, not this
    // helper — so removing the match here doesn't break that path.
    msg.contains("timed out")
        || msg.contains("Connection reset")
        || msg.contains("Connection closed")
        || msg.contains("kex_exchange_identification")
        || msg.contains("banner exchange")
}

/// `ssh_exec` with retry on transient SSH failures. Up to `max_attempts`
/// total tries, exponential backoff (2s, 5s, 11s, 23s, ...). Returns
/// success on the first OK attempt; returns the last error if all fail.
///
/// Use this anywhere a single SSH attempt's transient failure would leave
/// the bot in a stuck state for a long time (e.g. `deploy_destination` —
/// the bot's retry path is a 20-minute sleep).
pub(crate) async fn ssh_exec_with_retry(
    ip: &str, password: &str, ssh_user: &str, script: &str, max_attempts: u32,
) -> Result<(), String> {
    let mut last_err = String::new();
    let mut backoff_secs = 2u64;
    for attempt in 1..=max_attempts {
        match ssh_exec(ip, password, ssh_user, script).await {
            Ok(()) => {
                if attempt > 1 {
                    log::info!("ssh_exec_with_retry({}) succeeded on attempt {}/{}",
                        ip, attempt, max_attempts);
                }
                return Ok(());
            }
            Err(e) => {
                if !is_transient_ssh_error(&e) {
                    // Permanent error — don't burn retries.
                    return Err(e);
                }
                last_err = e;
                if attempt < max_attempts {
                    log::debug!("ssh_exec_with_retry({}) attempt {}/{} transient fail: {}, \
                        backing off {}s", ip, attempt, max_attempts, last_err, backoff_secs);
                    tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                    backoff_secs = backoff_secs.saturating_mul(2).saturating_add(1);
                }
            }
        }
    }
    Err(format!("ssh_exec failed after {} attempts: {}", max_attempts, last_err))
}

/// First-contact server hardening — run ONCE per server's lifetime, as the
/// bot's very first SSH operation. Idempotent and best-effort per step.
///
///   1. Kill UFW (persistent) — its default rules block our DNAT MASQUERADE
///      and forwarding, and even a stopped service re-applies on reboot.
///   2. Kill fail2ban (persistent + unban-all) — by default it bans the bot's
///      source IP after the first few parallel SSH attempts and surfaces as
///      `Connection refused` from the bot side.
///   3. Raise sshd MaxStartups from default 10:30:100 → 100:30:200. The bot's
///      parallel subsystems (test_and_deploy + monitor + spawn_node_registration)
///      can burst 5–10 simultaneous SSH attempts to the same target; the default
///      starts probabilistically rejecting at the kex stage past 10 in-flight
///      unauth connections, which surfaces as "Connection reset" on the bot
///      side and leaves DNAT half-configured for hours.
///   4. Enable BBR + fq qdisc (persistent) — meaningful throughput win for the
///      TM-user ↔ proxy ↔ exit chain (high RTT + occasional loss).
///
/// Retries on transient SSH failures (the very first contact often races the
/// throttling we're trying to fix), so a single transient drop doesn't make
/// the whole hardening fail.
pub(crate) async fn harden_server(ip: &str, password: &str, ssh_user: &str) -> Result<(), String> {
    let script = r#"
# 1. UFW — stop + disable so it can't re-fire on boot
systemctl stop ufw 2>/dev/null || true
systemctl disable ufw 2>/dev/null || true
ufw --force disable 2>/dev/null || true

# 2. fail2ban — unban first (in case the bot's own IP is already banned from
#    a previous setup attempt), then stop + disable so the actionstop hook
#    flushes its iptables chain.
fail2ban-client unban --all 2>/dev/null || true
systemctl stop fail2ban 2>/dev/null || true
systemctl disable fail2ban 2>/dev/null || true

# 3. Raise sshd MaxStartups. Only rewrite + reload if the line isn't already
#    present, so a re-run is a no-op (and doesn't bounce sshd needlessly).
if ! grep -q '^MaxStartups 100:30:200' /etc/ssh/sshd_config; then
    sed -i '/^MaxStartups/d' /etc/ssh/sshd_config
    echo 'MaxStartups 100:30:200' >> /etc/ssh/sshd_config
    systemctl reload sshd 2>/dev/null || systemctl reload ssh 2>/dev/null || true
fi

# 4. BBR + fq. modprobe is a no-op on kernels where bbr is built-in.
modprobe tcp_bbr 2>/dev/null || true
sysctl -w net.core.default_qdisc=fq >/dev/null 2>&1 || true
sysctl -w net.ipv4.tcp_congestion_control=bbr >/dev/null 2>&1 || true
grep -q '^net.core.default_qdisc=fq' /etc/sysctl.conf || echo 'net.core.default_qdisc=fq' >> /etc/sysctl.conf
grep -q '^net.ipv4.tcp_congestion_control=bbr' /etc/sysctl.conf || echo 'net.ipv4.tcp_congestion_control=bbr' >> /etc/sysctl.conf

echo "hardened: ufw+fail2ban killed, MaxStartups raised, BBR enabled"
"#;

    // 4 retries — first contact often races the very throttling we're trying
    // to fix (sshd's unauth queue full, or packet loss on the kex packets).
    // Backoff doubles each attempt: 2s, 5s, 11s, 23s — total wait ~41s plus
    // up to 4× SSH_TIMEOUT_SECS for the attempts themselves.
    ssh_exec_with_retry(ip, password, ssh_user, script, 4).await
}

/// Reboot the front VM. The reboot is detached (`nohup … &` + a 2s delay) so
/// the SSH session exits cleanly before the box goes down — a direct `reboot`
/// kills the connection mid-command and reports a spurious error. Callers rely
/// on the provisioner's normal SSH-retry cadence to pick the server back up.
pub(crate) async fn reboot_server(ip: &str, password: &str, ssh_user: &str) -> Result<(), String> {
    ssh_exec(ip, password, ssh_user, "nohup sh -c 'sleep 2; reboot' >/dev/null 2>&1 &").await
}

/// Set up IP forwarding and NAT masquerading on the server (idempotent).
pub(crate) async fn setup_ip_forwarding(ip: &str, password: &str, ssh_user: &str) -> Result<(), String> {
    let script = r#"
grep -q '^net.ipv4.ip_forward=1' /etc/sysctl.conf || echo 'net.ipv4.ip_forward=1' >> /etc/sysctl.conf
sysctl -w net.ipv4.ip_forward=1
IFACE=$(ip route | grep default | awk '{print $5}' | head -1)
iptables -t nat -C POSTROUTING -o "$IFACE" -j MASQUERADE 2>/dev/null || iptables -t nat -A POSTROUTING -o "$IFACE" -j MASQUERADE
DEBIAN_FRONTEND=noninteractive apt-get install -y iptables-persistent < /dev/null > /dev/null 2>&1 || true
"#;

    ssh_exec(ip, password, ssh_user, script).await
}

/// Blanket-forward ALL ports (tcp+udp) to the fixed OpenVPN-AS backend,
/// port-preserving, EXCEPT SSH (22) which stays local so the bot can keep
/// managing the front. Wrapped in the retry helper + a hard timeout, like the
/// old per-destination deploy. Idempotent (`-C ... || ...`).
pub(crate) async fn setup_dnat_with_timeout(ip: &str, password: &str, ssh_user: &str, backend_ip: &str) -> Result<(), String> {
    let script = setup_dnat_script(backend_ip);
    tokio::time::timeout(
        Duration::from_secs(DEPLOY_TIMEOUT_SECS),
        ssh_exec_with_retry(ip, password, ssh_user, &script, 3),
    )
    .await
    .map_err(|_| format!("dnat setup timed out after {}s (frozen)", DEPLOY_TIMEOUT_SECS))?
}

fn setup_dnat_script(backend: &str) -> String {
    // AUTHORITATIVE production script (user-supplied), parameterized by the
    // fixed OpenVPN-AS backend. Applied ONCE at deploy — NOT re-run per monitor
    // cycle (flushing mid-cycle would momentarily drop NAT). Key properties:
    //   * the DNAT is a SINGLE protocol-agnostic catch-all (`-A PREROUTING -j
    //     DNAT --to-destination {backend}`) — no `-p tcp/udp`, no `-i $IFACE` —
    //     so it forwards ALL protocols/ports (incl. ICMP) to the backend, and
    //     it ignores which floating IP traffic arrived on, so every pool IP is
    //     covered automatically (no change needed on IP rotation).
    //   * `-A PREROUTING -p tcp --dport 22 -j RETURN` is appended BEFORE the
    //     catch-all so SSH to the front matches first and stays local (the bot
    //     manages the front over SSH).
    //   * FLUSH first (`-t nat -F` / `-F FORWARD`) then append in order —
    //     deterministic, not the idempotent `-C || -A` pattern.
    //   * dual MASQUERADE + bidirectional FORWARD allow (`-d` and `-s`).
    format!(
        r#"
NET_IFACE=$(ip route get 8.8.8.8 | awk '{{print $5; exit}}')
echo 1 > /proc/sys/net/ipv4/ip_forward
sysctl -w net.ipv4.ip_forward=1
grep -q '^net.ipv4.ip_forward=1' /etc/sysctl.conf || echo 'net.ipv4.ip_forward=1' >> /etc/sysctl.conf
iptables -t nat -F
iptables -F FORWARD
iptables -t nat -A POSTROUTING -o "$NET_IFACE" -j MASQUERADE
iptables -t nat -A PREROUTING -p tcp --dport 22 -j RETURN
iptables -t nat -A PREROUTING -j DNAT --to-destination {backend}
iptables -t nat -A POSTROUTING -j MASQUERADE
iptables -A FORWARD -d {backend} -j ACCEPT
iptables -A FORWARD -s {backend} -j ACCEPT
mkdir -p /etc/iptables
iptables-save > /etc/iptables/rules.v4
"#,
        backend = backend,
    )
}

/// Execute a command via SSH on the remote server.
pub(crate) async fn ssh_exec(ip: &str, password: &str, ssh_user: &str, script: &str) -> Result<(), String> {
    ssh_exec_inner(ip, password, ssh_user, sudo_wrap(ssh_user, script)).await
}

/// Like `ssh_exec` but runs `remote_cmd` verbatim — NO `sudo_wrap`. For commands
/// that must run as the LOGIN user (e.g. writing the login user's own
/// `~/.ssh/authorized_keys`: under `sudo`, `HOME` resets to /root and `~`
/// expands wrong for a non-root login user).
async fn ssh_exec_inner(ip: &str, password: &str, ssh_user: &str, remote_cmd: String) -> Result<(), String> {
    let result = run_sshpass_output(ip, password, ssh_user, remote_cmd).await?;

    if !result.status.success() {
        let stderr = String::from_utf8_lossy(&result.stderr);
        return Err(format!("SSH command failed: {}", stderr.trim()));
    }

    Ok(())
}

/// Get total outgoing bytes from a server via SSH (reads /proc/net/dev).
/// Used by Selectel's monthly traffic quota check.
pub(crate) async fn ssh_get_tx_bytes(ip: &str, password: &str, ssh_user: &str) -> Result<i64, String> {
    let script = r#"awk '/eth0|ens|enp/ { split($0, a, ":"); split(a[2], b); print b[9] }' /proc/net/dev | head -1"#;
    let remote_cmd = sudo_wrap(ssh_user, script);
    // NOTE: error wording changes from the old local "SSH timeout"/"SSH exec:"
    // to the shared strings — sole consumer is a debug log in monitor_server.
    let result = run_sshpass_output(ip, password, ssh_user, remote_cmd).await?;

    if !result.status.success() {
        return Err("SSH failed".to_string());
    }

    let stdout = String::from_utf8_lossy(&result.stdout);
    stdout.trim().parse::<i64>()
        .map_err(|_| format!("parse tx_bytes: '{}'", stdout.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_classifier_catches_real_observed_failure_strings() {
        // Strings copy-pasted from actual bot log lines on the lossy
        // Vultr→TimeWeb path that prompted this retry helper.
        assert!(is_transient_ssh_error("SSH command timed out"));
        assert!(is_transient_ssh_error("SSH connection failed: ssh: connect to host 198.18.1.235 port 22: Connection timed out"));
        assert!(is_transient_ssh_error("Connection to 198.18.2.175 port 22: Connection reset by 198.18.2.175"));
        assert!(is_transient_ssh_error("kex_exchange_identification: Connection closed by remote host"));
        assert!(is_transient_ssh_error("Connection closed by 198.18.1.235 port 22"));
        assert!(is_transient_ssh_error("Connection timed out during banner exchange"));
    }

    #[test]
    fn transient_classifier_does_not_catch_permanent_failures() {
        // Permanent errors — config/credential issues that don't self-heal.
        // Retrying these wastes time and may trigger account lockouts.
        assert!(!is_transient_ssh_error("Permission denied (publickey,password)"));
        assert!(!is_transient_ssh_error("Host key verification failed."));
        assert!(!is_transient_ssh_error("SSH command failed: bash: line 1: someunknowncmd: command not found"));
        assert!(!is_transient_ssh_error("ssh: Could not resolve hostname nonexistent.invalid"));
        assert!(!is_transient_ssh_error(""));
        // Connection refused = sshd not listening = permanent for our retry path.
        // Manual setup's post-reboot wait loop handles this case separately
        // with its own retry, see providers/manual/setup.rs.
        assert!(!is_transient_ssh_error("ssh: connect to host 198.18.3.74 port 22: Connection refused"));
    }
}

