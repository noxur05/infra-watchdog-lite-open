# VPN Watchdog Architecture

> **Last refreshed 2026-06-19** (HEAD `97f1aa8`) for the 12-provider roster and
> the current IP-owned host-label format. Most sections (concurrent tasks, state
> machine, WAL/shutdown, admin UI) were already accurate; the refresh corrected
> the provider list, the DNAT-vs-direct-node split, the rate-limit tracking
> sets, and the Marzban host-remark format. Inline `src/…:line` references are
> illustrative and may have drifted. For authoritative detail defer to
> `CLAUDE.md` and the code.

## 1. System Overview

### High-Level Architecture
VPN Watchdog is a multi-provider cloud server provisioner and VPN node manager. It automates the lifecycle of rented VPN proxy servers across twelve provider integrations (TimeWeb, Beget, Vultr, Selectel, Gcore, UpCloud, VDSina, Hostman, reg.ru, Scaleway, Hetzner, and Manual) and manages their integration with a Marzban VPN panel.

**Key characteristics:**
- **Single Rust binary** (~10MB when stripped, runs on Vultr free tier: 0.5GB RAM, 1 CPU)
- **Async-first architecture** using Tokio 1.x with multiple concurrent long-lived tasks
- **SQLite database** in WAL mode for robust data persistence
- **Telegram bot interface** (Teloxide 0.17) for admin control
- **SSH-based deployment** via `sshpass` to provisioned servers
- **Bot host is NOT in Turkmenistan** — it SSHes into provider servers to run blocked-site curl tests

### Runtime Environment
- **Framework stack:**
  - Teloxide 0.17 (Telegram bot framework)
  - Tokio 1.x (async runtime)
  - SQLx 0.8 (SQLite driver with compile-time query checking)
  - reqwest 0.12 (HTTP client with connection pooling and timeout support)
  - SQLite 3.x (WAL mode)

- **Build profile:** `opt-level=z`, LTO enabled, binary stripped, `panic=abort`
- **Launch requirements:**
  - Binary must be in a directory containing `migrations/` (loaded at startup)
  - `.env` file must be present (Marzban credentials + Telegram token)
  - `xray-core` must be in `PATH` (for node checking via SSH)
  - SSH access to provisioned servers (for Marzban node registration and curl tests)

### Core Invariants
1. **Single admin ID** only (`ADMIN_TELEGRAM_ID` env var); all other Telegram traffic is silently dropped
2. **Per-account credentials** are stored in database tables, not environment variables
3. **One MarzbanClient singleton** owns the JWT token cache and reqwest connection pool (shared across all calls to avoid duplicate auth round-trips)
4. **WAL mode checkpoint** on graceful shutdown leaves a clean main file (idempotent for fast restart)
5. **Graceful shutdown** via `tokio::sync::watch::channel` propagates to all background tasks within ~5 seconds

---

## 2. Three Concurrent Long-Lived Tasks

### Task 1: Telegram Dispatcher (Interactive)
**File:** `src/main.rs` (lines 180-219)

**Purpose:** Route incoming Telegram updates to handlers.

**Lifecycle:**
1. Spawned at startup as `Dispatcher::builder(bot, handler)`
2. Runs `dispatcher.dispatch()` until shutdown signal received
3. On SIGINT or SIGTERM, shutdown token is triggered → dispatcher unblocks
4. On exit, WAL is checkpointed before process termination

**Message routing:**
- `Update::filter_message()` → `handle_message()`
  - Extracts user via safe `get_user()` helper (skips bot posts)
  - Admin-only: all non-admin traffic silently dropped
  - Text messages either parse as commands (`/start`) or are routed to `admin::handle_admin_text_input()` for multi-step flows (e.g., adding a new account)
  
- `Update::filter_callback_query()` → `handle_callback()`
  - All callbacks prefixed `adm_` go to `admin::handle_admin_callback()`
  - Provider modules match their own callback patterns and handle them

**Guard mechanisms:**
- `get_user(msg)` returns `None` for channel posts and bot messages — always use this, never raw `msg.from`
- All user-supplied strings escaped with `utils::escape_md()` before MarkdownV2 parsing

---

### Task 2: Checker Scheduler (Background Monitoring)
**File:** `src/checker/scheduler.rs`

**Purpose:** Periodically test blocked-site accessibility through active Marzban nodes.

**Lifecycle:**
1. Spawned at startup via `checker::scheduler::spawn_scheduler()`
2. Runs a 60-second polling loop (wake-on-timeout or shutdown signal)
3. Every 1440 cycles (24h): cleanup old check results and orphaned dest_deployments
4. On shutdown signal: breaks loop and exits gracefully

**Cycle logic:**
```
For each active subscription:
  If due for check (interval_secs elapsed since last sync):
    Fetch + parse subscription URL (nodes)
    Upsert nodes into DB
    For each filtered node (matching NODE_FILTER):
      Parallel: curl test via xray-core to blocked-site
      Record result (up/down + latency or error reason)
      Alert admin on state change (or first check if configured)
```

**Key behaviors:**
- Tests are parallel (bounded by worker semaphore)
- Test URLs are hard-coded: `turkmenistanairlines.tm`, `railway.gov.tm`
- Alert dedup: only notifies on transition (up↔down) or on first check if `alert_on_first_check=true`
- Node filter: `config.node_filter` suffix match (empty = test all)

**Database cleanup:**
- `cleanup_old_results()` — deletes check_results older than N days
- `cleanup_orphaned_deployments()` — removes dest_deployments with deleted servers

---

### Task 3: Provisioner Manager + Account Provisioners (Server Lifecycle)
**Files:** `src/provisioner/manager.rs`, `src/provisioner/account.rs`

**Purpose:** Manage cloud server creation, deployment, and monitoring across all active accounts.

#### 3a. Manager Loop (30s cycle)
**File:** `src/provisioner/manager.rs`

```
Loop every 30 seconds:
  For each provider module in providers::all():
    Query list_active_accounts() → returns (account, Box<CloudProvider>)
  
  Stop provisioners for:
    - Deactivated accounts
    - Accounts whose region/location changed
  
  Start new provisioners for:
    - New active accounts
    - Re-provisioned accounts after region change
  
  Sleep 30s OR shutdown
```

**Key data structures:**
- `RETRY_NOTIFIES: HashMap<(provider_type, account_id), Arc<Notify>>` — admin "🔄 Retry now" button registry
  - When admin clicks button → `request_retry()` looks up the account's Notify and fires it
  - AccountProvisioner's loop wakes and skips cooldown for one cycle

- `account_handles: HashMap<ProvisionerKey, (JoinHandle, watch::Sender, location)>` — tracks all running provisioners
  - Detects location changes and respawns with new CloudProvider client

#### 3b. AccountProvisioner State Machine (Per-Account Loop)
**File:** `src/provisioner/account.rs` (~3000 lines)

**State diagram:**
```
none → creating → installing → on (not deployed) → on (deployed) → monitoring
       ↓                              ↓                    ↓
    error/retry              error/retry          error/retry
       ↓
  delete_and_cleanup
```

**Per-account in-memory state:**
```rust
pub struct AccountProvisioner {
    provider_type: String,
    account_id: i64,
    location: String,
    provider: Box<dyn CloudProvider>,
    pool: SqlitePool,
    bot: Bot,
    shutdown: watch::Receiver<bool>,
    retry_notify: Arc<Notify>,
    
    // Vultr-specific
    last_vultr_ip: Option<String>,                     // Last IP from deleted server
    vultr_tick: u32,                                   // Cycle counter (throttles monitoring to every 5th)
    vultr_skip_create_until: Option<DateTime<Utc>>,   // 2h backoff when same IP recycled
    vultr_capped_notified_at: Option<DateTime<Utc>>,  // 24h dedup for trial cap notify
    
    // UpCloud-specific
    upcloud_zone_idx: usize,                           // Zone rotation cursor
    upcloud_attempt: u32,                              // Attempts per zone (0..2)
    upcloud_tick: u32,                                 // Cycle counter (throttles monitoring)
    upcloud_capped_notified_at: Option<DateTime<Utc>>,
    
    // Selectel IP rotation
    selectel_rotation: RotationState,                  // Same-IP counter + last IP
    
    // Beget IP rotation
    beget_rotation: RotationState,                     // Same-IP counter + last IP
    
    // Error notification dedup
    last_notified_error_category: Option<String>,
    last_notified_error_at: Option<DateTime<Utc>>,
}
```

**Main loop flow:**
```
Startup:
  Reconcile orphaned servers (query provider for stranded servers)
  Run one cycle immediately
  
Main loop:
  Sleep (normal interval OR rate-limit cooldown OR infinity until signal)
    OR shutdown signal received → exit
    OR retry-notify fired → clear cooldown + run now
  
  run_cycle() → returns Result
  handle_cycle_result() → log + dedup admin notification
    - Same error category: notify only if 24h+ elapsed or category changed
    - Recovery: send ✅ message after error is fixed
```

**run_cycle() branches:**

1. **provision_new_server()** — Create a new server
   - Check rate limit (ApiSignal: query provider; CountBased: count recent DB rows)
   - On rate-limit hit: set cooldown + return (no alert yet; alert on next cycle if still capped)
   - Call `provider.create_server(location)` → get server_id + password
   - Store in `servers` table (status='creating')
   - Spawn background marzban-node registration (if enabled)

2. **poll_server_status()** — Wait for server to boot
   - Call `provider.get_server(server_id)` every 30s
   - Poll until status='active' + IPv4 assigned
   - Transition to 'installing' on success

3. **test_and_deploy()** — Initial setup + DNAT deployment
   - SSH to server as root
   - `setup_ip_forwarding()` — enable kernel forwarding + iptables DNAT rules
   - `deploy_destination_with_timeout()` — push destination port mappings (curl test each)
   - `harden_server()` — install updates, disable root login (for manual servers)
   - Transition to 'on (not deployed)' on success
   - If no destinations configured yet: stay in this state until admin adds them

4. **monitor_server()** — Verify deployed state
   - Every 5th cycle: curl test each destination through the server
   - If any destination curl fails:
     - Selectel: rotate floating IP instead of deleting
     - Beget: in-place IP rotation (delete-then-add public IP)
     - Vultr/others: mark as failed, eventually delete + retry
   - If server itself becomes unreachable (SSH fails 3x): mark for deletion
   - Transition to 'on (deployed)' on success

5. **delete_and_cleanup()** — Tear down failed server
   - Deregister marzban-node from panel (if registered)
   - Call `provider.delete_server(server_id)`
   - Cleanup DNAT entries from DB
   - Transition back to 'none'

**Error handling:**
- All errors pass through `categorize_provisioning_error()` which tags them (e.g., "rate_limit", "auth", "network", "transient")
- 24h dedup per category (same category re-notifies only after 24h or category changes)
- On recovery from error: single ✅ message sent
- `retry_keyboard` button attached to error messages for admin manual retry

**In-flight marzban-node registration dedup:**
```rust
static NODE_REG_INFLIGHT: LazyLock<Mutex<HashSet<i64>>> = ...;

// Both initial provisioning (test_and_deploy) and retry (monitor_server)
// call spawn_node_registration(), which checks NODE_REG_INFLIGHT:
//   if insert(server_id) fails (already inflight) → return early
//   otherwise → spawn async task that:
//     - registers node with panel
//     - writes node_id to DB
//     - removes server_id from the set on completion
```

This prevents double-registration races.

---

## 3. Provider Plugin Pattern

### Architecture
Each cloud provider is a plugin with two exported traits:

1. **`CloudProvider` trait** (`src/provider.rs`)
   - Implemented by provider-specific client
   - Consumed by provisioner's `AccountProvisioner`
   - Methods: `create_server()`, `get_server()`, `delete_server()`, `list_servers()`, `list_regions()`, `cleanup_resources()`, `rate_limit_config()`, `server_name_prefix()`

2. **`ProviderModule` trait** (`src/providers/mod.rs`)
   - Implemented by provider-specific module
   - Consumed by manager loop + admin UI
   - Methods: provisioner integration (`list_active_accounts()`), admin UI delegation (menu button, callbacks, text input), account listing, server status rendering

### Plugin Registration
**File:** `src/providers/mod.rs`

```rust
pub fn all() -> Vec<Box<dyn ProviderModule>> {
    vec![
        Box::new(timeweb::TimeWebProvider),
        Box::new(beget::BegetProvider),
        Box::new(vultr::VultrProvider),
        Box::new(selectel::SelectelProvider),
        Box::new(gcore::GcoreProvider),
        Box::new(vdsina::VdsinaProvider),
        Box::new(upcloud::UpcloudProvider),
        Box::new(hostman::HostmanProvider),
        Box::new(regru::RegrouProvider),
        Box::new(scaleway::ScalewayProvider),
        Box::new(hetzner::HetznerProvider),
        Box::new(manual::ManualProvider),
    ]
}
```

**Adding a new provider:**
1. Create `src/providers/newprovider/` folder
2. Implement `CloudProvider` + `ProviderModule` traits
3. Add `Box::new(newprovider::NewProviderProvider)` to `all()`
4. No other code needs to change

### Standard Provider Module Layout
Each provider folder contains:
- `mod.rs` — `ProviderModule` trait implementation
- `client.rs` — HTTP API client (CloudProvider implementation)
- `db.rs` — Account CRUD operations
- `admin.rs` — Admin UI flows (menu, callbacks, text input)
- `buttons.rs` — Callback constant definitions
- Optional: `hooks.rs` (provider-specific features like IP rotation)

### Shared Utilities
**File:** `src/providers/common/` — reusable components
- Common callback constants (add, delete, list, back)
- Shared SSH deployment logic (via `crate::ssh` module)
- HTTP client builder with timeouts (`utils::build_http_client()`)

---

## 4. Provisioner State Machine (Detailed)

### Database Context
The provisioner operates on a single `servers` table row per account:

```sql
CREATE TABLE servers (
    id INTEGER PRIMARY KEY,
    provider_type TEXT NOT NULL,
    provider_account_id INTEGER NOT NULL,
    provider_server_id TEXT NOT NULL,
    ipv4 TEXT,
    root_pass TEXT,
    status TEXT,  -- 'none', 'creating', 'installing', 'on'
    marzban_node_id INTEGER,
    marzban_synced BOOLEAN,
    deploy_failed_at TIMESTAMP,
    region TEXT,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    ...
);
```

### State Transitions

#### **none** → **creating**
- AccountProvisioner detects `status='none'` on the first cycle
- Checks rate limit (see section 7)
- Calls `provider.create_server(location)` with cloud provider
- Receives `server_id`, `root_pass`, optional `warning`
- Stores in DB with `status='creating'`
- Spawns background marzban-node registration task (idempotent via `NODE_REG_INFLIGHT`)
- Moves to **creating** state

#### **creating** → **installing**
- Polls `provider.get_server(server_id)` every 30 seconds
- Waits for status to become 'active' and `ipv4` to be assigned
- On success: update DB `status='installing'`, move to next state
- On timeout (>30min): notify admin + mark for deletion

#### **installing** → **on (not deployed)**
- SSH to `root@ipv4` with `root_pass` or SSH key
- `setup_ip_forwarding()`:
  - Enable `net.ipv4.ip_forward=1`
  - Flush iptables
  - For DNAT mode: install `socat` + configure DNAT + PREROUTING rules
- `harden_server()` (manual servers only):
  - `apt update && apt upgrade`
  - Disable root SSH password auth
  - Install SSH public key
- On success: update DB `status='on'`, `marzban_synced=false`
- Move to **on (not deployed)**

#### **on (not deployed)** → **on (deployed)**
- Call `sync_marzban_hosts_global()` to push updated host list to panel
- Wait for destinations to be deployed:
  - If 0 destinations configured: remain in this state (waiting for admin to add)
  - If destinations exist: `deploy_destination_with_timeout()` for each
    - SSH to server
    - Run curl test through DNAT local_port → dest_ip:dest_port
    - If all curls pass: mark `deploy_failed=NULL`
- On success: update DB `status='on'`, `marzban_synced=true`
- Move to **on (deployed)**

#### **on (deployed)** → monitoring loop (or error states)
- Every ~60 seconds: run curl test for each active destination
- If all pass: remain in **on (deployed)** state
- If any fail:
  - **Selectel** (special): rotate floating IP without deleting server
  - **Beget** (special): in-place public IP rotation
  - **Vultr**: mark server for deletion + respawn (IP testing mode)
  - **Gcore**: during off-hours (outside 18:00–08:00 Ashgabat): delete server + sleep
  - **Others**: mark `deploy_failed_at`, retry SSH/curl ~3 times, then delete

#### Any state → **delete_and_cleanup** (on persistent error)
- Call `provider.delete_server(server_id)`
- Call `provider.cleanup_resources()` (e.g., release floating IPs for TimeWeb)
- Query `destination_deployments` for this server_id → curl-test each destination on fallback servers
- Deregister marzban-node from panel (if registered)
- Sync Marzban host list
- Update DB `status='none'`
- Return to **none** state

### Error Recovery Flow
1. Error occurs in any state
2. `categorize_provisioning_error()` tags error (auth, rate_limit, network, transient, etc.)
3. Check 24h dedup window:
   - Same category already notified in last 24h? Skip notification, retry next cycle
   - Different category or first time? Notify admin with error + retry keyboard
4. On next cycle, if error clears:
   - Send ✅ recovery message (only once per recovery)
   - Clear `last_notified_error_category` and `last_notified_error_at`
   - Continue state machine normally

---

## 5. Database Schema and WAL Mode

### Database Initialization
**File:** `src/db.rs`

```rust
pub async fn init_pool(database_url: &str) -> Result<SqlitePool, sqlx::Error> {
    let options = SqliteConnectOptions::from_str(database_url)?
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)        // ← WAL mode
        .synchronous(SqliteSynchronous::Full);        // ← fsync every commit
    
    let pool = SqlitePoolOptions::new()
        .max_connections(5)                           // ← Small connection pool
        .connect_with(options)
        .await?;
    
    run_migrations(&pool).await?;
    Ok(pool)
}
```

**Key settings:**
- **WAL mode**: Separates the main file (checkpointed) from the WAL (new writes)
  - Reader can operate on the main file while writer fills the WAL
  - On graceful shutdown: `PRAGMA wal_checkpoint(TRUNCATE)` folds WAL into main file
- **synchronous=FULL**: Every commit fsyncs to disk before returning
  - Cost: ~1 fsync/sec for our low write volume
  - Benefit: survived write failures that previously lost data
- **journal_mode=WAL set on connection options**: Applies to every connection the pool opens (not persisted per-DB, so runtime must set it)
- **max_connections=5**: Keeps memory footprint low

### Migrations
**Directory:** `migrations/` (lexicographic order), currently `001` … `043_create_hetzner_accounts.sql`. Add new files as `NNN_*.sql`; never edit an existing one.

All migrations are **idempotent**:
- ALTER TABLE errors "duplicate column name" are caught and logged (not fatal)
- CREATE TABLE IF NOT EXISTS is used

Migration process:
```rust
async fn run_migrations(pool: &SqlitePool) {
    let mut conn = pool.acquire().await?;  // Single connection
    sqlx::query("PRAGMA foreign_keys=OFF").execute(&mut *conn).await?;  // Disable FK checks
    
    for migration_file in sorted(migrations/) {
        let sql = read(migration_file);
        for statement in sql.split(';') {
            sqlx::query(statement).execute(&mut *conn).await?;  // Idempotent
        }
    }
    
    sqlx::query("PRAGMA foreign_keys=ON").execute(&mut *conn).await?;  // Re-enable FK checks
}
```

### Core Tables

**users, subscriptions, nodes, check_results** — Original subscription-based monitoring (pre-provisioner)

**Provider accounts** (one set per provider):
- `tw_accounts` (TimeWeb)
- `beget_accounts` (Beget)
- `vultr_accounts` + `vultr_regions` (Vultr multi-region)
- `selectel_accounts` + `selectel_account_regions` + `selectel_zones` (Selectel OpenStack)
- `gcore_accounts` + `gcore_regions` (Gcore multi-region)
- `upcloud_accounts` (UpCloud)
- `vdsina_accounts` (VDSina)
- `hostman_accounts` (Hostman)
- `regru_accounts` (reg.ru)
- `scaleway_accounts` (Scaleway)
- `hetzner_accounts` (Hetzner)
- `manual_servers` (Manual IP/password entry)

**Server provisioning**:
- `servers` — Tracks state machine per account (provider_type, provider_account_id, status, ipv4, marzban_node_id, etc.)
- `destinations` — Admin-defined dest_ip:dest_port pairs (target Marzban nodes)
- `dest_deployments` — Junction table (server_id, dest_id) — which servers expose which destinations

**Global settings**:
- `bot_settings` — KV table for marzban_node_cert, bot SSH key, etc.

### Graceful Shutdown Checkpoint
**File:** `src/main.rs` (lines 220-224)

```rust
// Dispatcher has exited; all background tasks are stopping
shutdown_tx.send(true).ok();

// Fold WAL into main file before exit
db::checkpoint_wal(&pool).await;

// Process exits
```

`checkpoint_wal()` runs `PRAGMA wal_checkpoint(TRUNCATE)`:
- Copies all WAL pages into the main file
- Truncates WAL to zero length
- Next boot reads a clean, checkpointed main file

---

## 6. Marzban Dual-Publish: DNAT + Direct-Node

### Architecture Overview
Two publishing modes coexist on the **same Marzban inbound**:

1. **DNAT mode** (TimeWeb, Beget, Vultr, Selectel, VDSina, reg.ru, Scaleway, Manual)
   - Rented server runs `iptables` MASQUERADE + port-forwarding rules
   - Panel publishes `(server_ip, local_port)` per destination
   - Multiple destinations → multiple local_port entries on a single server IP
   - Each client connection: server_ip:local_port → DNAT → dest_ip:dest_port

2. **Direct-node mode** (Gcore, UpCloud, Hostman, Hetzner — the `is_direct_node()` set)
   - Rented server runs `ghcr.io/noxur05/marzban-node` as a Docker container
   - Container registers with panel API (`/api/node`)
   - Panel publishes `(server_ip, inbound_port)` per node
   - No DNAT, no destinations — node handles all proxying
   - One server per account (or per region for multi-region providers)

> **Important:** marzban-node registration is gated by the `marzban_node_cert`
> KV being set, **not** by `is_direct_node()`. When a node cert is present a
> DNAT server ALSO registers a node (dual-publish on the same inbound).
> `is_direct_node()` only controls whether the DNAT/destination path is skipped.

### Marzban Host Remarks (client-facing labels)
The remark is a clean, client-facing string with **no internal IDs and no
`BOT_CODE`** — ownership is tracked by the host's *address*, not a remark suffix
(see "Host ownership" below). Built by `host_label()` in
`src/provisioner/marzban_sync.rs`:

```
{flag} {Country} {icon×n}
```
- `{flag} {Country}` come from `utils::region_to_country(provider, region)`
  (manual servers resolve the country per-IP via ip-api.com).
- `{icon×n}` is the provider icon (`utils::provider_icon`) repeated `n` times,
  where `n` is the server's 1-based ordinal within its provider, ordered by
  `(account_id, server_id)` via `assign_server_ordinals()`. So `⚡` marks the
  1st TimeWeb account's exit, `⚡⚡` the 2nd, etc.; all hosts riding one IP share
  the sequence, and account id leads the sort so the icon survives a server
  recreation.
- `dedupe_labels()` appends ` 2`, ` 3`… to any collisions (e.g. two destinations
  in the same country behind one server).

Example: `🇩🇪 Germany ⚡⚡` — no account/dest ids, no provider name, no BOT_CODE.

**Key rule:** the label is product UI — it must **not** reveal the provider name
or any internal id. (The marzban-node *entity* name, shown only on the panel's
internal node list, is `{BOT_CODE}-{provider}-{account}-{server_id}`.)

### Global Marzban Sync
**File:** `src/provisioner/marzban_sync.rs`

**Function:** `sync_marzban_hosts_global(pool: &SqlitePool)`

**Why global sync?** Two separate calls to `update_bot_hosts()` against the same inbound would each strip bot-owned hosts, causing a race:
- Call 1 (DNAT): reads all hosts → strips bot hosts → adds new DNAT hosts → PUT
- Call 2 (Node): reads all hosts → strips bot hosts → adds new node hosts → PUT
- Result: one call's hosts are lost

**Solution:** Build one combined Vec and push via single `update_bot_hosts()` call.

**Algorithm:**
```
1. Query all running servers by status and mode
2. For DNAT servers (providers):
   - Query destinations for each server
   - Query dest_deployments to see which dests are active on which servers
   - Build one Marzban host per (server_ip, local_port, dest_id)
   - Skip non-DNAT servers (direct-node, manual without DNAT)

3. For direct-node servers (Gcore, UpCloud):
   - Query marzban_node_id from servers table
   - Build one Marzban host per node → (server_ip, inbound_port, node_id)

4. Combine DNAT hosts + node hosts → single list

5. Call MarzbanClient::shared().update_bot_hosts(inbound_tag, combined_list)
   - Reads current panel hosts
   - Strips all bot-owned hosts — matched against the bot's owned server
     addresses (current IPs plus prior IPs recorded in `server_ip_history`,
     migration 042, so a rotated-away IP's stale host is still reclaimed),
     NOT by a `_{BOT_CODE}` remark suffix
   - Adds the new combined bot hosts
   - PUTs result back to panel
```

### Host ownership (why labels carry no BOT_CODE)
Earlier the bot recognized its own hosts by a `_{BOT_CODE}` suffix on the
remark. That moved into the label-as-product-UI redesign: hosts are now owned
by **address**. `update_bot_hosts` reclaims any panel host whose address is one
the bot owns — including IPs a server has since rotated away from, tracked in
`server_ip_history` (migration 042) to close a host-leak on IP rotation.

### Marzban Panel Integration
**File:** `src/marzban.rs`

**MarzbanAuth:**
- Stateful: caches JWT token + expiry
- Refresh lock prevents thundering herd on token expiry
- Fast path (already valid) vs slow path (refresh needed)

**MarzbanClient (singleton):**
- Process-wide `LazyLock<MarzbanClient>` shared across all callers
- Provides: `get_hosts()`, `put_hosts()`, `update_bot_hosts()`, `add_node()`, `list_nodes()`, `get_node_status()`, `delete_node()`
- On 401 (auth failed): auto-refresh token + retry once
- Connection pool reused across calls

**add_node() idempotency:**
- 409 response (name exists) → check if existing node has same address → adopt it
- Different address (IP recycled) → delete stale node + re-POST
- Makes registration safe against transient failures

---

## 7. Rate Limiting Strategies

### Two Tracking Modes

#### 7.1 ApiSignal (TimeWeb, Beget, Hostman)
**Provider signals limit via API response** (checked *after* `create_server`).

```rust
pub enum RateLimitTracking {
    ApiSignal,    // Provider returns warning or error code
}
```

**Example (TimeWeb):**
```
POST /api/account/create/server
Response: { "status": "ok", "warning": "floating_ip_daily_limit_exceeded" }
```

When `create_server()` succeeds but returns a warning, the provisioner:
1. Records the server (it was created)
2. Sets a cooldown (24h default)
3. Does not attempt another create until cooldown expires
4. On next cycle, if still capped: no re-notification (24h dedup)

**Example (Beget):**
```
POST /v1/vps
Response: { "error": { "code": "CUSTOMER_IP_LIMIT_REACHED" } }
```

Returns `ProviderError::RateLimit()` → provisioner sets cooldown.

#### 7.2 CountBased (Vultr, Selectel, Gcore, UpCloud, VDSina, reg.ru, Scaleway, Hetzner)
**Count DB rows to enforce limit** (checked *before* `create_server`, via
`db::get_cooldown_remaining`).

```rust
pub struct RateLimitConfig {
    pub max_free_trials: u32,        // e.g., 2, 3, 150
    pub cooldown_hours: u32,         // e.g., 24, 48
    pub cooldown_anchor: CooldownAnchor,  // Oldest (sliding) or Latest (flat)
}

pub enum CooldownAnchor {
    Oldest,   // Resume when oldest trial exits window (sliding)
    Latest,   // Flat quiet period from most recent trial
}
```

**Algorithm:**
```
Before calling create_server():
  Query servers created in last `cooldown_hours` hours
  Count rows
  If count >= max_free_trials:
    Check anchor:
      - Oldest: count minutes until oldest row is `cooldown_hours` old
      - Latest: count minutes until `cooldown_hours` since latest row
    Set cooldown
    Return (no create attempted)
  Else:
    Proceed with create_server()
```

**Provider examples:**

| Provider | Mode | Max trials | Cooldown | Anchor | Behavior |
|----------|------|-----------|----------|--------|----------|
| Vultr | CountBased | 2 | 24h | Oldest | Sliding window — resume as soon as one slot frees |
| UpCloud | CountBased | 150 | 24h | Oldest | 150 free trials per 24h, globally |
| Selectel | CountBased | varies | varies | Oldest | Per-region trial limits (with zone rotation) |

### Rate Limit in AccountProvisioner

**Flow:**
```
run_cycle() {
    if let Some(cooldown) = cooldown_remaining {
        sleep(cooldown);                           // Rate-limited, wait
        return;
    }
    
    match provision_new_server().await {
        Ok(server_id) => {
            cooldown_remaining = None;              // Success, reset cooldown
            // Continue state machine
        }
        Err(ProviderError::RateLimit(_)) => {
            cooldown_remaining = Some(Duration);   // Set from config
            // Do NOT notify yet; check again next cycle
        }
        Err(e) => {
            // Handle other errors
        }
    }
}
```

**Notification logic:**
- First cycle hitting cap: sleep + no alert (maybe temporary)
- Next cycle still capped: notify admin once every 24h
- When cap clears: notify admin with ✅ recovery message

---

## 8. Admin UI Delegation Pattern

### Message and Callback Flow
**File:** `src/main.rs` (lines 34-88)

```
User sends /start
  → handle_message()
  → admin::handle_admin_command()
  → render main menu (provider buttons, dest buttons, etc.)

User clicks provider button
  → handle_callback() (data starts with "adm_")
  → admin::handle_admin_callback()
  → switch on provider_type
  → provider_module.handle_callback(bot, cq, pool, data, telegram_id)
  → provider's mod.rs routes to add_account, list_accounts, delete_account, etc.

User types text in multi-step flow
  → handle_message()
  → admin::handle_admin_text_input()
  → check for multi-step state (stored in-memory)
  → if provider callback from prior step, delegate to:
  → provider_module.handle_text_input(bot, msg, pool, telegram_id)
  → provider saves account to DB, clears state
```

### Shared Admin Interfaces
**File:** `src/handlers/admin.rs`

**Responsibilities:**
- Main menu rendering (provider buttons, destination buttons, server status)
- Callback routing (prefix-match on provider_type)
- Shared button constants (`admin_buttons.rs`)
- Shared state management (multi-step flow state stored in-memory)

**Architecture:**
```
admin module:
  admin.rs          ← routing, menu rendering
  admin_buttons.rs  ← shared callback constants (add, delete, list, back, etc.)

provider module:
  mod.rs            ← ProviderModule impl (delegates to sub-modules)
  admin.rs          ← provider-specific flows (add_account, etc.)
  buttons.rs        ← provider-specific callback constants
  db.rs             ← account CRUD
```

### Example: Add Beget Account

**Flow:**
1. Admin clicks "Add Beget account" button
   - Callback data: `adm_beget_add`
   - Routes to `beget::BegetProvider::handle_callback()`
   - Module prompts: "Enter account name"
   - Sets in-memory state: `pending_input=(telegram_id, 'beget_name')`

2. Admin types account name
   - Routes to `admin::handle_admin_text_input()`
   - Detects pending state
   - Calls `beget::BegetProvider::handle_text_input()`
   - Stores to DB: `INSERT INTO beget_accounts (name, ...) VALUES (?)`
   - Clears pending state
   - Responds: "Account added. Enter API token:" → next state

3. Admin types token
   - Same flow repeats
   - Final state: account fully configured
   - Button: "Account ready" or "Account already exists"

---

## 9. Graceful Shutdown

### Shutdown Signal Cascade
**Files:** `src/main.rs` (lines 151–216)

```
Kernel sends SIGINT (Ctrl-C) OR SIGTERM (systemctl stop)
  ↓
tokio signal handler → logs "SIGINT/SIGTERM received, shutting down"
  ↓
shutdown_tx.send(true)  ← broadcast via watch::channel
  ↓
dispatcher.shutdown_token()  ← signals dispatcher to stop polling for updates
  ↓
All spawned background tasks receive notification:
  - checker scheduler   → break main loop
  - provisioner manager → break manager loop, stop all AccountProvisioners
  - AccountProvisioner  → break run loop
  - manual monitor      → break loop
  - backup task         → break loop
  ↓
Each task stops cleanly within ~5 seconds:
  - No new SSH processes spawned
  - In-flight `sshpass` marked with `kill_on_drop(true)` → reaped on task drop
  - Marzban-node register tasks abort (in-flight installs may be interrupted)
  ↓
dispatcher.dispatch() returns
  ↓
main.rs reaches checkpoint_wal() → PRAGMA wal_checkpoint(TRUNCATE)
  ↓
Process exits, leaving a clean checkpointed DB
```

### watch::channel Pattern
```rust
let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

// In each spawned task
tokio::select! {
    _ = sleep(...) => { /* normal work */ }
    _ = shutdown_rx.changed() => {
        log::info!("Task received stop signal");
        break;  // Clean exit
    }
}
```

**Benefits:**
- Single broadcast signal (no channel per task)
- All tasks wake immediately
- Idempotent: `send(true)` multiple times is safe (no deadlock)

---

## 10. Per-Provider Quirks

### Selectel (OpenStack-based, IP Rotation)
**File:** `src/providers/selectel/`

**Key behaviors:**
1. **Auth:** Keystone v3, service user, project-scoped token (24h TTL)
2. **Server creation:** Ensure network → find flavor → find image → create server → assign floating IP
3. **IP rotation instead of delete:**
   - On curl fail: rotate floating IP (assign different IP to same server) instead of destroying server
   - Same IP 10 times in a row → delete server + 24h cooldown, fallback to next region
   - Re-reads floating IP from provider and re-syncs Marzban hosts
4. **Zone affinity:** Each row in `selectel_zones` is a distinct account/region pair; multi-region failover tries each in priority order
5. **TX quota:** Polls `/proc/net/dev` over SSH; enforces 3TB/month; auto-throttles or deletes on exceed
6. **Router external gateway:** Must be manually configured in Selectel panel (not auto-created by API)

### Vultr (Free trial IP tester)
**File:** `src/providers/vultr/`

**Key behaviors:**
1. **Not rate-limited:** Effectively unlimited free trials (2-region fallback)
2. **IP testing loop:** Create → test → delete → repeat until a working IP is found
3. **Last IP tracking:** `last_vultr_ip` remembers the previous deletion; if new server gets same IP → backoff 2h (Vultr cycles IPs slowly)
4. **Deployed server monitoring:** Every 5th cycle (throttled by `vultr_tick`) runs curl test
5. **Multi-region support:** `vultr_regions` table; one provider per region; respawn on region change
6. **Per-region backoff:** Each region has its own `vultr_skip_create_until` to allow other regions to create while one is backed off

### Gcore (Direct-node, Multi-region, Time-gated)
**File:** `src/providers/gcore/`

**Key behaviors:**
1. **Direct-node only:** No DNAT; runs `ghcr.io/noxur05/marzban-node` Docker container
2. **18:00–08:00 Ashgabat window:** Only creates servers during active hours; off-window tears down and sleeps until next 18:00
3. **Multi-region:** Each `gcore_regions` row is a distinct provisioner unit (different server per region)
   - `servers.provider_account_id` stores `gcore_regions.id`, NOT the parent gcore_accounts.id
   - Migration 028 did the one-shot remap
4. **Rate limit:** Per-Gcore-account 24h budget (GCORE_PER_ACCOUNT_24H_BUDGET = 10) split evenly across active regions (5 regions → 2 each)
5. **Image & flavor discovery:** Queried at runtime per region (IDs differ across regions)
6. **SSH login user:** `ubuntu` (not root) — `ssh_user_for()` returns `"ubuntu"` for Gcore; deploy scripts wrapped with `sudo_wrap()`

### Beget (Russia-only, In-place IP Rotation)
**File:** `src/providers/beget/`

**Key behaviors:**
1. **In-place public IP rotation:** Delete-then-add public IP on same server (via `/v1/vps` API)
   - Requires a private network to detach the last IP (don't leave server without any IP)
2. **Password at creation:** API returns password on create; never returns on get → stored in DB at creation time
3. **Daily IP quota:** `floating_ip_daily_limit_exceeded` warning after ~4 creates/day per region
   - Once hit: cooldown 24h, retry next day (quota resets daily at 00:00 UTC)
4. **ApiSignal rate limiting:** Provider signals limit via warning field
5. **Region-specific:** Different regions have different daily quotas; some may be exhausted while others available

### Manual Servers (Special lifecycle)
**File:** `src/providers/manual/`

**Key behaviors:**
1. **Outside generic provisioner:** `list_active_accounts()` returns empty; dedicated monitor + setup tasks in `main.rs`
2. **Setup flow (one-shot on add):**
   - SSH reboot → wait for SSH back
   - Detect existing marzban-node via `docker ps`
   - Always set up DNAT (idempotent)
   - Install + register marzban-node only if NOT pre-existing
   - Sync Marzban hosts
3. **Monitoring (background):** Parallel curl tests; on 3 fails → alert with "🗑 Remove" button
   - After 24h still failing → auto-deactivate (drops from Marzban hosts) but row persists
4. **Country label:** Resolved per-server via ip-api.com (not static map like other providers)
5. **Credentials:** IP + SSH user + password stored in-memory; no provider API calls (manual admin entry)

### Others (TimeWeb, UpCloud, VDSina, Hostman, reg.ru, Scaleway, Hetzner)
- **TimeWeb:** Russia-only, **DNAT**, ApiSignal (daily IP quota via the
  `floating_ip_daily_limit_exceeded` warning).
- **UpCloud:** **direct-node**, CountBased, zone-hunt rotation, trial credits.
- **VDSina:** **DNAT** (not direct-node), CountBased, multi-region datacenters.
- **Hostman:** **direct-node**, ApiSignal, simple API near-clone of TimeWeb.
- **reg.ru:** **DNAT**, DO-style CloudVPS API (not OpenStack), CountBased;
  Moscow / St. Petersburg / Samara regions.
- **Scaleway:** **DNAT**, single-region, CountBased, `STARDUST1-S` instances,
  country-prefixed zones (`fr-par-1`, …).
- **Hetzner:** **direct-node**, CountBased (recreate-to-rotate, ~3 creates/24h),
  key-auth via cloud-init, `cx23` instances.

---

## Summary

This architecture distributes concerns across:
1. **Dispatcher** — user interaction (Telegram)
2. **Checker** — VPN node health monitoring
3. **Provisioner manager** — account lifecycle orchestration
4. **AccountProvisioners** — per-account state machines (parallel)
5. **Provider modules** — cloud-specific clients + admin UI
6. **Database** — SQLite WAL for durability
7. **Marzban sync** — dual-publish to panel (DNAT + direct-node)

All three tasks cooperate via:
- Shared SQLite database (single writer, multiple readers via WAL)
- Graceful shutdown via `watch::channel`
- Admin notifications + state callbacks
- Per-provider rate-limit handling

The result is a resilient, single-binary VPN infrastructure manager running on minimal resources (0.5GB RAM) while handling complex multi-provider, multi-region failover and monitoring.
