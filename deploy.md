# Deployment

How the `vpn-watchdog` bot is built, shipped, and run in production. The bot is a
single Rust binary (Telegram bot + provisioner + daily backup) that runs as a
systemd service on one server.

---

## Production server

| | |
|---|---|
| Host | `198.18.39.228` |
| User | `root` |
| Auth | **SSH key only** (password auth should stay disabled — see [SSH access](#ssh-access)) |
| OS | Ubuntu 24.04 LTS (x86_64) |
| App dir | `/root/rust/vpn-watchdog/` |
| Binary | `/root/rust/vpn-watchdog/vpn-watchdog` |
| Migrations | `/root/rust/vpn-watchdog/migrations/` |
| Config | `/root/rust/vpn-watchdog/.env` (mode `600`) |
| Database | `/root/rust/vpn-watchdog/watchdog.db` (SQLite, WAL) |
| Service | `vpn-watchdog.service` |
| Runtime dep | `sshpass` (the bot SSHes into Selectel fronts with it) |

The service unit sets `WorkingDirectory=/root/rust/vpn-watchdog` so the binary
finds `.env` (via `dotenv`) and `migrations/` (loaded at startup), and
`Environment=RUST_LOG=info` so the startup marker and operational logs reach
journald. It restarts `on-failure` every 5s.

---

## CI/CD pipeline

Two GitHub Actions workflows in `.github/workflows/`:

### `ci.yml` — every branch except `main`
On push to any non-`main` branch: `cargo check` → `cargo test` → `cargo build
--release` → binary sanity → `clippy` (non-fatal). This is the pre-merge gate;
it never touches the server.

### `deploy.yml` — production deploy
Triggers:
- **Automatically** on push to `main`.
- **Manually** via `workflow_dispatch`, which requires typing `DEPLOY` in the
  `confirm` input as a guard.

Pipeline (single `production` environment job, `deploy-prod` concurrency group so
two deploys can't overlap):

1. Build + test + `cargo build --release` (same gate as CI).
2. Configure SSH from secrets (writes the deploy key + `known_hosts`).
3. **Preflight** — SSH to prod, print service state.
4. **Backup** — snapshot the current binary → `vpn-watchdog.previous` and
   `migrations/` → `migrations.previous` (so a bad migration can be rolled back
   too).
5. Stop the service.
6. Upload the new binary (to `.new`, then atomic `mv`) and each migration file.
7. **Start + health check** — start the service, wait, and assert:
   - `systemctl is-active` == `active`, and
   - the startup log has no `panic`/`FATAL`, and
   - the log shows `Bot is running` (warns if not seen).
8. **Auto-rollback on health failure** — restore `vpn-watchdog.previous` +
   `migrations.previous`, restart, and poll up to 30s for `active`.

### Run a deploy
```bash
# Automatic: just merge/push to main.
git push origin main

# Manual (from anywhere with gh + repo access):
gh workflow run "Deploy to production" -R noxur05/watchdog-open -f confirm=DEPLOY

# Watch it:
gh run watch -R noxur05/watchdog-open
```

---

## GitHub secrets

Set on repo `noxur05/watchdog-open` (Settings → Secrets and variables → Actions).
`deploy.yml` reads exactly these three:

| Secret | What it is |
|---|---|
| `PROD_HOST` | `198.18.39.228` |
| `PROD_SSH_KEY` | Private half of the dedicated CI deploy key (ed25519) |
| `PROD_HOST_KEY` | `ssh-keyscan` output for the host (pins the host key, prevents MITM) |

Update one:
```bash
printf '198.18.39.228' | gh secret set PROD_HOST -R noxur05/watchdog-open
gh secret set PROD_SSH_KEY  -R noxur05/watchdog-open < /path/to/private_key
ssh-keyscan -t ed25519,rsa,ecdsa 198.18.39.228 | gh secret set PROD_HOST_KEY -R noxur05/watchdog-open
gh secret list -R noxur05/watchdog-open
```

---

## SSH access

Two independent SSH-key roles — don't confuse them:

- **CI deploy key** — a dedicated `ed25519` keypair used only by GitHub Actions.
  Its public key sits in `/root/.ssh/authorized_keys` on prod, labeled
  `github-actions-deploy@watchdog-open`; its private key is the `PROD_SSH_KEY`
  secret. Revocable on its own without affecting human logins.
- **Bot runtime key** — the bot generates its own `ed25519` key at first run
  (stored in `bot_settings`, materialized to `.bot_ssh_key`) and offers it
  key-first / password-fallback when SSHing into the Selectel *fronts* it
  provisions. Unrelated to deploy.

### Rotate the CI deploy key
```bash
# 1. New keypair (locally, e.g. in a tmpdir)
ssh-keygen -t ed25519 -f ci_deploy_key -N "" -C "github-actions-deploy@watchdog-open"

# 2. Install the new pubkey, remove the old labeled one
NEWPUB=$(cat ci_deploy_key.pub)
ssh root@198.18.39.228 "grep -v 'github-actions-deploy@watchdog-open' /root/.ssh/authorized_keys > /root/.ssh/ak.tmp; \
  echo '$NEWPUB' >> /root/.ssh/ak.tmp; mv /root/.ssh/ak.tmp /root/.ssh/authorized_keys; chmod 600 /root/.ssh/authorized_keys"

# 3. Replace the secret, then shred the local copy
gh secret set PROD_SSH_KEY -R noxur05/watchdog-open < ci_deploy_key
shred -u ci_deploy_key ci_deploy_key.pub
```

### Disable password auth (recommended, keys are confirmed working)
```bash
ssh root@198.18.39.228 "sed -i 's/^PasswordAuthentication.*/PasswordAuthentication no/' \
  /etc/ssh/sshd_config.d/00-vpn-watchdog.conf && systemctl reload ssh"
```

---

## Configuration (`.env`)

Lives at `/root/rust/vpn-watchdog/.env`, mode `600`, loaded by `dotenv` at
startup. **Not** in git and **not** uploaded by the deploy — it persists on the
server across deploys.

**Required** (bot panics/idles without them):
```
TELOXIDE_TOKEN=<BotFather token, raw, no "bot" prefix, no quotes>
ADMIN_TELEGRAM_ID=<your numeric Telegram user id>
DATABASE_URL=sqlite:watchdog.db
BACKUP_PASSWORD=<passphrase you choose — needed to decrypt backups; store it safely>
```

**Optional:**
```
BOT_CODE=BWD                 # default BWD
CHECK_INTERVAL_SECS=120      # monitor cadence
# OpenVPN-AS backend — now managed in-bot via the 🖥 Main Server screen
# (bot_settings, .env used only as fallback). Leave blank to manage in-bot.
OPENVPN_BACKEND_IP=
OPENVPN_API_USER=
OPENVPN_API_PASS=
OPENVPN_API_PORT=943
# SMTP for front-IP-change emails — all blank → mailing silently disabled
SMTP_HOST=
SMTP_PORT=465
SMTP_USERNAME=
SMTP_PASSWORD=
SMTP_FROM=
```

Editing config takes effect on the next service restart:
```bash
ssh root@198.18.39.228 "nano /root/rust/vpn-watchdog/.env && systemctl restart vpn-watchdog.service"
```

---

## Service management

```bash
# Status / logs
ssh root@198.18.39.228 "systemctl status vpn-watchdog.service --no-pager"
ssh root@198.18.39.228 "journalctl -u vpn-watchdog.service -n 80 --no-pager"
ssh root@198.18.39.228 "journalctl -u vpn-watchdog.service -f"          # live tail

# Stop / start / restart
ssh root@198.18.39.228 "systemctl stop    vpn-watchdog.service"
ssh root@198.18.39.228 "systemctl start   vpn-watchdog.service"
ssh root@198.18.39.228 "systemctl restart vpn-watchdog.service"
```

### Manual deploy (bypass CI, e.g. hotfix)
```bash
cargo build --release --locked
ssh root@198.18.39.228 "systemctl stop vpn-watchdog.service"
scp target/release/vpn-watchdog root@198.18.39.228:/root/rust/vpn-watchdog/vpn-watchdog
scp migrations/*.sql            root@198.18.39.228:/root/rust/vpn-watchdog/migrations/
ssh root@198.18.39.228 "systemctl start vpn-watchdog.service && \
  journalctl -u vpn-watchdog.service -n 30 --no-pager"   # want 'Bot is running...'
```

---

## Database & migrations

- Single SQLite file `watchdog.db` (WAL). Migrations in `migrations/` run at
  startup, in order, and are **idempotent** — duplicate-column `ALTER TABLE`
  errors are swallowed (`src/db.rs:run_migrations`).
- Add changes as `004_*.sql` (next free number); never edit a shipped migration.
- On startup the provisioner reconciles orphans (reaps leaked Selectel servers +
  floating IPs it isn't tracking).

---

## Backups & restore

The bot sends an **encrypted** SQLite snapshot to the admin Telegram chat daily
at 00:00 Asia/Ashgabat. Format: `[16B salt][12B nonce][ChaCha20-Poly1305
ciphertext]`, key derived from `BACKUP_PASSWORD` via argon2id (`src/backup.rs`).

To restore a backup file received in Telegram:
```bash
# Build the restore helper once
cargo build --release --bin restore_backup

# Decrypt (password from $BACKUP_PASSWORD or an interactive prompt)
BACKUP_PASSWORD='<the same password used in .env>' \
  ./target/release/restore_backup watchdog-backup-YYYYMMDD.enc watchdog.db
```
Losing `BACKUP_PASSWORD` makes every stored backup unrecoverable — it's the only
key. It doesn't have to match anything else, so rotating it just means future
backups use the new one (keep the old one to read old backups).

---

## Rollback

- **Automatic** — a failed health check in `deploy.yml` restores the previous
  binary + migrations and restarts (see the pipeline above).
- **Manual** — the previous artifacts are kept on the server:
  ```bash
  ssh root@198.18.39.228 "systemctl stop vpn-watchdog.service && \
    cp /root/rust/vpn-watchdog/vpn-watchdog.previous /root/rust/vpn-watchdog/vpn-watchdog && \
    rm -rf /root/rust/vpn-watchdog/migrations && \
    cp -r /root/rust/vpn-watchdog/migrations.previous /root/rust/vpn-watchdog/migrations && \
    systemctl start vpn-watchdog.service"
  ```

---

## Troubleshooting

| Symptom | Cause / fix |
|---|---|
| `Api(InvalidToken)` panic loop at startup | `TELOXIDE_TOKEN` wrong/placeholder. Fix `.env`, restart. Token is raw from BotFather, no `bot` prefix, no quotes. |
| `TELOXIDE_TOKEN must be set` | `.env` missing or not in `WorkingDirectory`. Must be `/root/rust/vpn-watchdog/.env`. |
| Service `active` but no logs | `RUST_LOG` unset → only errors print. The unit sets `RUST_LOG=info`; if missing, re-add `Environment=RUST_LOG=info` under `[Service]`, `daemon-reload`, restart. |
| Deploy warns "did not see Bot is running" | Health marker only appears with `RUST_LOG=info` (see above). |
| Deploy fails at preflight/SSH | Check the three secrets; re-run `ssh-keyscan` for `PROD_HOST_KEY` if the host was rebuilt. |
| Bot can't reach Selectel fronts | `sshpass` missing on prod (`apt-get install -y sshpass`), or the front's password/IP changed. |
| No front / nothing published | OpenVPN-AS backend not configured — set it in the bot's 🖥 Main Server screen (or the `OPENVPN_*` `.env` fallback). |
