//! Publish the current front IP to the OpenVPN-AS backend.
//!
//! The bot's only publish path. There is a single active Selectel
//! front server; whenever it's freshly deployed or its floating IP rotates, we
//! point the fixed OVPN-AS backend's `host.name` at that IP (and restart). A
//! lock serializes publishes so two provisioner events can't race the AS
//! restart.
//!
//! Side effect: after a successful publish, if the active IP actually changed,
//! BCC the saved email list (📧 Emails admin screen) the new address. This is
//! best-effort — a mail failure never fails the publish.

use std::sync::{Arc, LazyLock};

use sqlx::SqlitePool;

use crate::config::get_config;
use crate::db;
use crate::mailer::{Mailer, MailerError};

/// Serializes OVPN-AS publishes across the (single) provisioner task and any
/// retry paths so two events can't race the `host.name` set + restart.
pub static PUBLISH_LOCK: LazyLock<Arc<tokio::sync::Mutex<()>>> =
    LazyLock::new(|| Arc::new(tokio::sync::Mutex::new(())));

/// KV key tracking the last IP we BCC'd about, so a same-IP republish (deploy
/// retry, idempotent re-assert) doesn't re-spam the list.
const LAST_EMAILED_KEY: &str = "last_emailed_front_ip";

/// Find the single live front IP and publish it to the OVPN-AS backend.
/// No-op `Ok` if there's no active server with an IP yet — OpenVPN simply has
/// no working remote until one comes up (or comes back after a park/rotation).
pub async fn publish_front_ip(
    pool: &SqlitePool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Serialize only the OVPN-AS mutation (set host.name + restart). The BCC
    // notification runs AFTER the lock is released: SMTP can stall for its full
    // connect timeout, and holding PUBLISH_LOCK across it would block every
    // other provisioner publish.
    let published_ip: Option<String> = {
        let _lock = PUBLISH_LOCK.lock().await;

        let ip: Option<String> = sqlx::query_scalar(
            "SELECT ipv4 FROM servers \
             WHERE provider_type = 'selectel' AND deleted_at IS NULL AND ipv4 IS NOT NULL \
             ORDER BY id DESC LIMIT 1",
        )
        .fetch_optional(pool)
        .await?;

        match ip {
            Some(ip) => {
                let backend = db::get_ovpn_backend(pool).await;
                crate::openvpn::publish_front(&backend, &ip).await?;
                Some(ip)
            }
            None => {
                log::info!("openvpn publish: no active front server, nothing to publish");
                None
            }
        }
    };

    // Best-effort BCC, outside the lock; only fires when the IP actually changed.
    if let Some(ip) = published_ip {
        notify_ip_change(pool, &ip).await;
    }
    Ok(())
}

/// BCC the email list the new front IP — but only when it differs from the last
/// IP we emailed about (deduped via the `last_emailed_front_ip` KV). Silent
/// no-op when SMTP isn't configured or the list is empty. Never propagates an
/// error: the IP publish already succeeded and mail must not block the cycle.
async fn notify_ip_change(pool: &SqlitePool, ip: &str) {
    // Dedupe: skip if we've already emailed this exact IP.
    match db::get_bot_setting(pool, LAST_EMAILED_KEY).await {
        Ok(Some(prev)) if prev == ip => return,
        Ok(_) => {}
        Err(e) => {
            log::warn!("publish: could not read {}: {} — emailing anyway", LAST_EMAILED_KEY, e);
        }
    }

    let c = get_config();
    let mailer = match Mailer::new(
        &c.smtp_host, c.smtp_port, &c.smtp_username, &c.smtp_password, &c.smtp_from,
    ) {
        Ok(m) => m,
        Err(MailerError::NotConfigured) => {
            log::debug!("publish: SMTP not configured, skipping IP-change email");
            return;
        }
        Err(e) => {
            log::warn!("publish: mailer build failed: {}", e);
            return;
        }
    };

    let recipients: Vec<String> = match db::list_emails(pool).await {
        Ok(list) => list.into_iter().map(|e| e.email).collect(),
        Err(e) => {
            log::warn!("publish: list_emails failed: {}", e);
            return;
        }
    };
    if recipients.is_empty() {
        // Nothing sent → leave the dedupe key untouched so newly-added
        // recipients get notified on the next change.
        return;
    }

    // Render the (admin-editable, Turkmen-by-default) template. Keys: {{IP}}
    // (required) and {{Date}} (optional).
    let (subject_tpl, body_tpl) = db::get_mail_template(pool).await;
    let date = crate::time_utils::now_local_string();
    let vars = [("IP", ip), ("Date", date.as_str())];
    let subject = crate::mailer::render_template(&subject_tpl, &vars);
    let body = crate::mailer::render_template(&body_tpl, &vars);

    match mailer.send_bcc(&recipients, &subject, &body).await {
        Ok(()) => log::info!("publish: BCC'd new front IP {} to {} recipient(s)", ip, recipients.len()),
        Err(e) => log::warn!("publish: send_bcc failed for {}: {}", ip, e),
    }

    // Set the dedupe key regardless of send success — a transient SMTP failure
    // must not re-spam the list on the next same-IP republish.
    if let Err(e) = db::set_bot_setting(pool, LAST_EMAILED_KEY, ip).await {
        log::warn!("publish: could not persist {}: {}", LAST_EMAILED_KEY, e);
    }
}
