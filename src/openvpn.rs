//! OpenVPN Access Server (OVPN-AS) REST client.
//!
//! The OVPN-AS backend is the "tell clients where to connect" mechanism.
//! The exit is a FIXED OVPN-AS backend; the disposable Selectel front DNATs all
//! ports to it. When the front IP changes, we repoint the backend's `host.name`
//! config item at the new front IP and restart the server, so the profiles it
//! hands out (and OpenVPN Connect auto-updates) point at the fresh front.
//!
//! Credentials come from `db::get_ovpn_backend` (bot-managed, env fallback) —
//! callers resolve them and pass the struct in, so this module stays a pure
//! REST client.
//!
//! API (per the AS REST docs):
//!   POST /api/auth/login/userpassword  {username,password} -> {auth_token}
//!   POST /api/config-items/Default     {"host.name": ip}    (header X-OpenVPN-As-AuthToken)
//!   POST /api/server/restart           {}                   (header X-OpenVPN-As-AuthToken)

use std::time::Duration;

use serde_json::json;

use crate::db::OvpnBackend;

fn base_url(b: &OvpnBackend) -> String {
    format!("https://{}:{}", b.ip, b.port)
}

/// Dedicated reqwest client that accepts the AS's self-signed TLS cert
/// (equivalent to `curl -sk`), with sane timeouts.
fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(60))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// Log in with the admin user/pass and return a fresh `auth_token`. We log in
/// per publish rather than caching — publishes are rare (deploy / IP rotation)
/// and a stale cached token would just force a retry anyway.
async fn login(cl: &reqwest::Client, b: &OvpnBackend) -> Result<String, String> {
    let resp = cl
        .post(format!("{}/api/auth/login/userpassword", base_url(b)))
        .json(&json!({"username": b.user, "password": b.pass}))
        .send()
        .await
        .map_err(|e| format!("ovpn login send: {e}"))?;
    let status = resp.status();
    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("ovpn login json: {e}"))?;
    body.get("auth_token")
        .and_then(|t| t.as_str())
        .map(String::from)
        .ok_or_else(|| format!("ovpn login: no auth_token (status {status}, body {body})"))
}

/// Credential check for the admin "🔌 Test connection" button: just the REST
/// login, no mutation. Ok(()) means IP/port reachable and user/pass accepted.
pub async fn test_login(b: &OvpnBackend) -> Result<(), String> {
    if !b.is_configured() {
        return Err("backend IP not set".into());
    }
    login(&client(), b).await.map(|_| ())
}

/// Point the OVPN-AS backend at `front_ip` and restart it so it takes effect:
/// login -> set `host.name` -> `/server/restart`. Errors if the backend isn't
/// configured or any step fails (the caller logs/notifies + retries next cycle).
pub async fn publish_front(b: &OvpnBackend, front_ip: &str) -> Result<(), String> {
    if !b.is_configured() {
        return Err("OpenVPN backend IP not set (env or 🖥 Main Server screen)".into());
    }
    let cl = client();
    let token = login(&cl, b).await?;

    let r = cl
        .post(format!("{}/api/config-items/Default", base_url(b)))
        .header("X-OpenVPN-As-AuthToken", &token)
        .json(&json!({ "host.name": front_ip }))
        .send()
        .await
        .map_err(|e| format!("ovpn set host.name send: {e}"))?;
    if !r.status().is_success() {
        return Err(format!("ovpn set host.name: status {}", r.status()));
    }

    let r = cl
        .post(format!("{}/api/server/restart", base_url(b)))
        .header("X-OpenVPN-As-AuthToken", &token)
        .json(&json!({}))
        .send()
        .await
        .map_err(|e| format!("ovpn restart send: {e}"))?;
    if !r.status().is_success() {
        return Err(format!("ovpn restart: status {}", r.status()));
    }

    log::info!("OpenVPN-AS host.name -> {} and restarted", front_ip);
    Ok(())
}
