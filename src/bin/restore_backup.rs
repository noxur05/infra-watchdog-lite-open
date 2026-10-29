//! Standalone restore utility for the encrypted daily backups produced by
//! `src/backup.rs`. Decrypts a `watchdog-YYYY-MM-DD.db.enc` file and writes
//! the resulting SQLite database to disk.
//!
//! Intentionally self-contained — does NOT depend on the rest of the crate
//! (no teloxide, no sqlx, no tokio), so the resulting binary is small (~2MB)
//! and can be cross-compiled or built on a fresh recovery host without the
//! bot's full toolchain.
//!
//! Usage:
//!   `cargo run --release --bin restore-backup -- <input.db.enc> <output.db>`
//!
//! Password source (in priority order):
//!   1. `$BACKUP_PASSWORD` env var
//!   2. interactive prompt on stderr
//!
//! Blob format mirrors `crate::backup::encrypt` exactly:
//!   `[16-byte salt][12-byte nonce][ciphertext + 16-byte poly1305 tag]`

use std::env;
use std::io::{self, BufRead, Write};
use std::process::ExitCode;

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};

const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 12;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    if args.len() != 3 {
        eprintln!("Usage: {} <encrypted-backup> <output-db>", args[0]);
        eprintln!("Password: read from $BACKUP_PASSWORD or prompted on stdin.");
        return ExitCode::from(2);
    }
    let in_path = &args[1];
    let out_path = &args[2];

    let password = match env::var("BACKUP_PASSWORD") {
        Ok(p) if !p.is_empty() => p,
        _ => prompt_password(),
    };

    let blob = match std::fs::read(in_path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("read {}: {}", in_path, e);
            return ExitCode::from(1);
        }
    };

    let plaintext = match decrypt(&blob, &password) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("decrypt: {} (wrong password? corrupted file?)", e);
            return ExitCode::from(1);
        }
    };

    if let Err(e) = std::fs::write(out_path, &plaintext) {
        eprintln!("write {}: {}", out_path, e);
        return ExitCode::from(1);
    }

    eprintln!(
        "OK: decrypted {} → {} ({} bytes)",
        in_path,
        out_path,
        plaintext.len()
    );
    ExitCode::SUCCESS
}

fn prompt_password() -> String {
    eprint!("BACKUP_PASSWORD: ");
    io::stderr().flush().ok();
    let mut s = String::new();
    io::stdin().lock().read_line(&mut s).expect("read stdin");
    s.trim_end_matches(['\n', '\r']).to_string()
}

fn decrypt(blob: &[u8], password: &str) -> Result<Vec<u8>, String> {
    if blob.len() < SALT_LEN + NONCE_LEN {
        return Err(format!(
            "blob too short ({} bytes, need ≥ {})",
            blob.len(),
            SALT_LEN + NONCE_LEN
        ));
    }
    let salt = &blob[..SALT_LEN];
    let nonce_bytes = &blob[SALT_LEN..SALT_LEN + NONCE_LEN];
    let ciphertext = &blob[SALT_LEN + NONCE_LEN..];

    let key = derive_key(password, salt)?;
    let cipher = ChaCha20Poly1305::new(&key.into());
    let nonce = Nonce::from_slice(nonce_bytes);
    cipher
        .decrypt(nonce, ciphertext)
        .map_err(|e| format!("aead: {}", e))
}

fn derive_key(password: &str, salt: &[u8]) -> Result<[u8; 32], String> {
    use argon2::{Algorithm, Argon2, Params, Version};
    let params = Params::new(64 * 1024, 3, 1, Some(32))
        .map_err(|e| format!("argon2 params: {}", e))?;
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut key = [0u8; 32];
    argon
        .hash_password_into(password.as_bytes(), salt, &mut key)
        .map_err(|e| format!("argon2 derive: {}", e))?;
    Ok(key)
}
