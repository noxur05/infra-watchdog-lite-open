# Multi-Account Parallel Provisioning — v2

## Context
Bot currently supports one TimeWeb account with one server. Need multiple accounts, each managing one region, all running in parallel. Every destination deploys on ALL accounts' servers.

## Key Decisions
- One server per account
- Destinations are global — every server gets every destination
- No `account_id` on destinations (no change to destinations table)
- `account_id` added to `tw_servers` only
- Same preset/OS/SSH key for all accounts
- Account tokens stored plaintext in SQLite
- When adding account: fetch regions from TimeWeb API, show as buttons
- Account deletion = deactivate in DB, stop provisioner loop, don't touch TimeWeb
- Marzban sync is global with Mutex — gathers all deployed dests across all servers

---

## DB Changes

### `migrations/009_create_tw_accounts.sql`
```sql
CREATE TABLE IF NOT EXISTS tw_accounts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    token TEXT NOT NULL,
    location TEXT NOT NULL DEFAULT 'msk-1',
    is_active INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
```

### `migrations/010_add_account_id_to_servers.sql`
```sql
ALTER TABLE tw_servers ADD COLUMN account_id INTEGER REFERENCES tw_accounts(id);
```

### Bootstrap (main.rs after init_pool)
If `tw_accounts` empty AND `TIMEWEB_CLOUD_TOKEN` env var set → auto-create default account, backfill existing `tw_servers` rows with `account_id = 1`.

---

## Changes by File

### 1. `src/db.rs`

**New struct:**
```rust
pub struct TwAccount {
    pub id: i64,
    pub name: String,
    pub token: String,
    pub location: String,
    pub is_active: bool,
    pub created_at: String,
}
```

**New functions:**
- `list_active_accounts(pool)` → `Vec<TwAccount>`
- `get_account(pool, id)` → `Option<TwAccount>`
- `add_account(pool, name, token, location)` → `i64`
- `deactivate_account(pool, id)`
- `bootstrap_default_account(pool)` — env→DB migration + backfill tw_servers
- `get_active_tw_server_for_account(pool, account_id)` → `Option<TwServer>`
- `clear_destinations_deployment_for_server(pool, server_id)` — reset deployed_on_server for dests deployed on a specific server

**Modified:**
- `TwServer` struct: add `account_id: Option<i64>`
- `create_tw_server(pool, tw_server_id, account_id)` — add account_id param
- Keep existing global functions working (list_active_destinations, etc.)

**NOT changed:** destinations table — no account_id, stays global.

### 2. `src/timeweb.rs`

- Add `new_with_token(token: String)` constructor
- Change `create_server(location)` to accept location param instead of reading from config
- Add `list_regions()` → fetch available locations from TimeWeb API for account creation UI
- Keep `new()` for backward compat / bootstrap

### 3. `src/provisioner.rs` — Core refactor

**New struct:**
```rust
struct AccountProvisioner {
    account: db::TwAccount,
    tw_client: TimeWebClient,
    pool: SqlitePool,
    bot: Bot,
    marzban_sync: Arc<tokio::sync::Mutex<()>>,
    shutdown: watch::Receiver<bool>,
}
```

**New architecture:**
```
spawn_provisioner() → provisioner_manager()
    ├─ Every 30s: read active accounts from DB
    ├─ Spawn AccountProvisioner for new accounts
    ├─ Stop provisioners for deactivated accounts
    └─ HashMap<account_id, (JoinHandle, stop_tx)>
```

**AccountProvisioner::run()** — same state machine as today but:
- `get_active_tw_server` → `get_active_tw_server_for_account(self.account.id)`
- `create_tw_server(tw_id)` → `create_tw_server(tw_id, self.account.id)`
- `TimeWebClient::new()` → `self.tw_client` (account-specific token)
- `create_server()` passes `self.account.location`
- Destinations stay global: `list_active_destinations()` unchanged
- `clear_destinations_deployment()` → `clear_destinations_deployment_for_server(srv.id)` — only clear dests deployed on THIS server, not all

**Marzban sync:**
- Static `LazyLock<Arc<Mutex<()>>>` — shared lock accessible from admin handler too
- `sync_marzban_hosts_global(pool, lock)` — gathers ALL deployed destinations across ALL accounts, each with their server's IP, pushes to Marzban
- New DB query: `list_deployed_destinations_with_server_ip(pool)` — JOIN destinations + tw_servers
- Host remark: `{country}_{BOT_CODE}` (same as now)
- On server delete: global sync removes that server's hosts, keeps others

**Reconciliation:** runs per-account with account's token.

### 4. `src/handlers/admin.rs`

**New flows:**
- "Accounts" button on main menu → list all accounts (name, location, server status)
- "Add Account" → ask name → ask token → fetch regions from TimeWeb API → show as buttons → create
- "Delete Account" → confirm → deactivate (provisioner stops, TimeWeb untouched)

**Modified flows:**
- Server status: show per-account (each account's server info)
- Destination add: unchanged (destinations are global, no account selection)

### 5. `src/handlers/admin_buttons.rs`
New callback constants for account management.

### 6. `src/main.rs`
Add `db::bootstrap_default_account(&pool).await` after init_pool.

### 7. `src/config.rs`
No changes. Env vars kept for bootstrap only.

---

## Edge Cases

| Case | Handling |
|------|----------|
| Account deleted while provisioner running | Manager sends stop signal, loop breaks. Server stays on TimeWeb. |
| Multiple accounts hit IP limit | Each has own `ip_limit_wait`, independent. |
| Marzban sync race | `Arc<Mutex<()>>` serializes all syncs. |
| Local port conflicts | `check_local_port_conflict` stays global — ports unique across all servers. |
| New destination added while servers running | Each account's monitor_server detects undeployed dests, deploys on its server. |
| Server deleted on one account | Global Marzban sync removes only that server's hosts. Other accounts' hosts stay. |
| Bot restart | Each account's provisioner re-reads its server from DB. |

---

## Implementation Order

1. Migrations (009, 010)
2. `db.rs` — TwAccount struct, account CRUD, per-account server queries, bootstrap
3. `main.rs` — bootstrap call
4. `timeweb.rs` — `new_with_token()`, parameterized `create_server()`, `list_regions()`
5. `provisioner.rs` — AccountProvisioner, manager, global Marzban sync, static lock
6. `admin_buttons.rs` — new constants
7. `admin.rs` — account management UI, per-account server status
8. `.env.example` — document bootstrap

## Verification

1. Start with existing DB → default account auto-created, existing tw_servers backfilled
2. Single account still works end-to-end
3. Add second account via bot → provisioner spawns within 30s
4. Both accounts provision servers, deploy ALL destinations on each
5. Marzban hosts show destinations from both servers
6. Deactivate account → provisioner stops, Marzban re-synced
7. Add new destination → appears on all running servers within next monitoring cycle
