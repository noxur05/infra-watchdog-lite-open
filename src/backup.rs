//! Daily encrypted SQLite backup → admin Telegram chat.
//!
//! Runs once per day at 00:00 Ashgabat (UTC+5). Flushes the WAL into the main
//! database file (`PRAGMA wal_checkpoint(TRUNCATE)`), reads `watchdog.db`,
//! encrypts with ChaCha20-Poly1305 (key derived from `BACKUP_PASSWORD` via
//! argon2id), and sends as a Telegram document to the admin.
//!
//! Output blob layout:
//!   `[16-byte salt][12-byte nonce][ciphertext + 16-byte poly1305 tag]`
//!
//! Decrypt with the `restore-backup` companion binary
//! (`cargo run --bin restore-backup -- <file> <out.db>`).

use std::time::Duration;

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use chrono::{FixedOffset, Utc};
use rand::RngCore;
use sqlx::SqlitePool;
use teloxide::prelude::*;
use teloxide::types::InputFile;
use tokio::sync::watch;

use crate::config::get_config;
use crate::time_utils::{seconds_until_next_ashgabat_midnight, ASHGABAT_OFFSET_SECS};

const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 12;

pub fn spawn_backup(bot: Bot, pool: SqlitePool, shutdown: watch::Receiver<bool>) {
    tokio::spawn(backup_loop(bot, pool, shutdown));
}

async fn backup_loop(bot: Bot, pool: SqlitePool, mut shutdown: watch::Receiver<bool>) {
    log::info!("Backup task started — fires daily at 00:00 Ashgabat (UTC+5)");
    loop {
        let secs = seconds_until_next_ashgabat_midnight(Utc::now());
        log::info!("Next backup in {}h{:02}m", secs / 3600, (secs % 3600) / 60);
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(secs)) => {}
            _ = shutdown.changed() => {
                log::info!("Backup task received shutdown signal");
                break;
            }
        }
        if let Err(e) = run_backup(&bot, &pool).await {
            log::error!("Daily backup failed: {}", e);
            crate::notify::notify_admin(&bot, &format!(
                "⚠️ Daily backup failed: `{}`",
                crate::utils::escape_md(&e),
            )).await;
        }
    }
    log::info!("Backup task stopped");
}

async fn run_backup(bot: &Bot, pool: &SqlitePool) -> Result<(), String> {
    let cfg = get_config();
    if cfg.backup_password.is_empty() {
        return Err("BACKUP_PASSWORD not set — cannot derive encryption key".into());
    }
    if cfg.admin_telegram_ids.is_empty() {
        return Err("ADMIN_TELEGRAM_ID not set — nowhere to send the backup".into());
    }

    // Resolve the on-disk path from the config URL (`sqlite:watchdog.db`).
    let db_path = cfg
        .database_url
        .strip_prefix("sqlite:")
        .unwrap_or(&cfg.database_url);

    // Prefer a transactionally-consistent snapshot via `VACUUM INTO`. A raw
    // file read of a live DB can capture a torn page mid-write (an unrestorable
    // backup); `VACUUM INTO` produces a clean, self-contained, defragmented
    // copy. If VACUUM fails (e.g. the live DB is corrupt), fall back to a
    // checkpoint + raw read so a backup still goes out — flagged UNVERIFIED.
    let tmp = format!("{}.backup-tmp", db_path);
    let _ = tokio::fs::remove_file(&tmp).await;
    let tmp_sql = tmp.replace('\'', "''");
    let (plaintext, method) = match sqlx::query(&format!("VACUUM INTO '{}'", tmp_sql))
        .execute(pool)
        .await
    {
        Ok(_) => {
            let data = tokio::fs::read(&tmp)
                .await
                .map_err(|e| format!("read vacuum snapshot {}: {}", tmp, e))?;
            let _ = tokio::fs::remove_file(&tmp).await;
            (data, "VACUUM INTO (consistent)")
        }
        Err(e) => {
            log::warn!("Backup: VACUUM INTO failed ({}); falling back to raw read \
                        (may be inconsistent — DB likely corrupt)", e);
            let _ = tokio::fs::remove_file(&tmp).await;
            let _ = sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)").execute(pool).await;
            let data = tokio::fs::read(db_path)
                .await
                .map_err(|e| format!("read {}: {}", db_path, e))?;
            (data, "raw read (UNVERIFIED — VACUUM failed)")
        }
    };
    log::info!("Backup: {} bytes via {} from {}", plaintext.len(), method, db_path);

    let mut encrypted = encrypt(&plaintext, &cfg.backup_password)?;

    let now_a = Utc::now().with_timezone(&FixedOffset::east_opt(ASHGABAT_OFFSET_SECS).unwrap());
    let filename = format!("watchdog-{}.db.enc", now_a.format("%Y-%m-%d"));
    let encrypted_len = encrypted.len();
    let caption = format!(
        "Daily backup\nDate: {}\nMethod: {}\nEncrypted: {} bytes (raw: {} bytes)",
        now_a.format("%Y-%m-%d %H:%M %z"),
        method,
        encrypted_len,
        plaintext.len(),
    );

    // Send to every admin. Move the bytes into the LAST send (via mem::take) and
    // clone only for the earlier ones, so the common single-admin case keeps the
    // old zero-clone footprint on the low-memory host.
    let n = cfg.admin_telegram_ids.len();
    let mut sent = 0usize;
    for (i, &id) in cfg.admin_telegram_ids.iter().enumerate() {
        let bytes = if i + 1 == n { std::mem::take(&mut encrypted) } else { encrypted.clone() };
        let file = InputFile::memory(bytes).file_name(filename.clone());
        match bot.send_document(ChatId(id), file).caption(caption.clone()).await {
            Ok(_) => sent += 1,
            Err(e) => log::error!("Backup send to admin {} failed: {}", id, e),
        }
    }
    if sent == 0 {
        return Err("backup send failed to every admin".into());
    }

    log::info!("Backup sent: {} ({} bytes) to {}/{} admin(s)", filename, encrypted_len, sent, n);
    Ok(())
}

fn encrypt(plaintext: &[u8], password: &str) -> Result<Vec<u8>, String> {
    let mut salt = [0u8; SALT_LEN];
    rand::thread_rng().fill_bytes(&mut salt);
    let mut nonce_bytes = [0u8; NONCE_LEN];
    rand::thread_rng().fill_bytes(&mut nonce_bytes);

    let key = derive_key(password, &salt)?;
    let cipher = ChaCha20Poly1305::new(&key.into());
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ciphertext = cipher
        .encrypt(nonce, plaintext)
        .map_err(|e| format!("encrypt: {}", e))?;

    let mut out = Vec::with_capacity(SALT_LEN + NONCE_LEN + ciphertext.len());
    out.extend_from_slice(&salt);
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

fn derive_key(password: &str, salt: &[u8]) -> Result<[u8; 32], String> {
    use argon2::{Algorithm, Argon2, Params, Version};
    // m=64MB, t=3, p=1 — fits comfortably on the 0.5GB Vultr host and slows
    // GPU brute-force by ~7 orders of magnitude vs raw hashing.
    let params = Params::new(64 * 1024, 3, 1, Some(32))
        .map_err(|e| format!("argon2 params: {}", e))?;
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut key = [0u8; 32];
    argon
        .hash_password_into(password.as_bytes(), salt, &mut key)
        .map_err(|e| format!("argon2 derive: {}", e))?;
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_succeeds() {
        let plaintext = b"hello world, this is a test of the backup encryption";
        let encrypted = encrypt(plaintext, "correct horse battery staple").unwrap();
        // Decrypt by hand using the same scheme.
        let salt = &encrypted[..SALT_LEN];
        let nonce = &encrypted[SALT_LEN..SALT_LEN + NONCE_LEN];
        let ct = &encrypted[SALT_LEN + NONCE_LEN..];
        let key = derive_key("correct horse battery staple", salt).unwrap();
        let cipher = ChaCha20Poly1305::new(&key.into());
        let pt = cipher.decrypt(Nonce::from_slice(nonce), ct).unwrap();
        assert_eq!(&pt, plaintext);
    }

    #[test]
    fn wrong_password_fails() {
        let encrypted = encrypt(b"secret", "right password").unwrap();
        let salt = &encrypted[..SALT_LEN];
        let nonce = &encrypted[SALT_LEN..SALT_LEN + NONCE_LEN];
        let ct = &encrypted[SALT_LEN + NONCE_LEN..];
        let key = derive_key("wrong password", salt).unwrap();
        let cipher = ChaCha20Poly1305::new(&key.into());
        assert!(cipher.decrypt(Nonce::from_slice(nonce), ct).is_err());
    }

    #[test]
    fn each_encrypt_uses_fresh_salt_and_nonce() {
        let a = encrypt(b"same plaintext", "same password").unwrap();
        let b = encrypt(b"same plaintext", "same password").unwrap();
        assert_ne!(a, b, "salt+nonce must be random per encrypt");
    }
}
