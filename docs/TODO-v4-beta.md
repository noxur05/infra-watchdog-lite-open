# TODO v4-beta — Feature Ideas

## Approved for Implementation

### 1. Same Account, Different Region (Clone Account)
One-tap "clone account to another region" in admin UI. Saves re-entering credentials.

**Conflict concern**: If the same provider account is added twice with different regions, each AccountProvisioner runs reconciliation with the same API credentials. Reconciliation lists servers by name prefix — both provisioners see ALL servers for that account, not just their region's. One provisioner could delete the other's server as "orphan".

**Solution**: Reconciliation must filter by region too. When listing remote servers, match both name prefix AND check the server's region/datacenter matches the account's configured region. This makes two provisioners for the same account safe — each only manages servers in its own region.

### 2. Smarter Health Checks (Latency Tracking)
Currently: pass/fail binary. Improvement: measure and store curl latency per server per check.

- Add `latency_ms` column to health check data
- Track rolling average per server
- If a server's average latency exceeds threshold (e.g. 3000ms), notify admin:
  "Region X has high latency for TM websites (avg 2847ms)"
- Admin can see latency trends per region to decide which regions work best
- Helps identify degrading connections before they fully break

### 3. Force Replace Server
Admin button on server detail: "Force Replace" — immediately deletes the server and triggers reprovisioning without waiting for 3 curl failures.

Use case: Admin notices issues via Marzban/users complaining, wants to replace a server without waiting 6+ minutes for auto-detection.

### 4. Force Redeploy Server
Admin button: "Force Redeploy" — reboots the server via provider API, then re-runs the full deployment pipeline (SSH script, iptables, destination deploy, Marzban sync).

Use case: Server is "on" but destinations stopped working. Faster than full replace (no new server creation, no 20min cooldown).

Flow:
1. Reboot server via provider API
2. Wait for SSH to come back (poll every 15s, timeout 5min)
3. Reset deploy flags in DB (script_deployed=0, ip_setup_done=0, marzban_synced=0)
4. Clear dest_deployments for this server
5. Provisioner picks it up and re-deploys from scratch

### 5. Daily Summary Notification
Automated daily message to admin (e.g. 09:00 UTC) with:
- Servers: X active, Y replaced in last 24h
- Per-provider breakdown (TimeWeb: 2 servers, Beget: 1 server)
- Uptime %: based on curl pass/fail ratio
- Average latency per region (if latency tracking implemented)
- Accounts: any at rate limit, any with auth errors
- Marzban: total hosts synced, any sync failures

### 6. Stale Server Detection
If a server has been in "creating" status for >15 minutes, auto-delete it and log a warning. Prevents ghost servers that consume provider quota but never become usable.

### 7. Server Lifecycle History
Track all state transitions in a `server_events` table:
```sql
CREATE TABLE server_events (
    id INTEGER PRIMARY KEY,
    server_id INTEGER,
    event TEXT,  -- 'created', 'ip_assigned', 'deployed', 'curl_fail', 'replaced', 'deleted'
    detail TEXT, -- extra info (IP, error message, etc)
    created_at TEXT DEFAULT (datetime('now'))
);
```
Enables: "show me history of server X", "how many servers did account Y burn through today"

### 8. Pause/Resume Provisioner Per Account
Admin button on account detail: "Pause" / "Resume". Sets `is_active=0` in DB, provisioner manager stops that account's loop on next 30s tick. Useful for debugging or when a provider is having issues.

Already partially implemented — `is_active` field exists. Just needs UI buttons.

---

## Planned — Vultr Provider

Vultr is different from TimeWeb/Beget. **No destinations, no iptables, no Marzban sync.**
It's an IP tester — create servers, check if the IP works from Turkmenistan, notify admin.

### Server specs
- **Plan**: Shared compute, $5/mo, no backup, no extras
- **IP**: Assigned at creation, released on deletion (no reserved IPs)
- **Password**: Expires in 10 min — must SSH immediately after creation

### Flow
1. Create shared compute server → get IP + password
2. SSH → disable UFW only (no iptables, no script)
3. Curl test TM sites through the server
4. If accessible → **notify admin only** (admin decides what to do)
5. If not accessible → delete server, retry

### Rate limits & cost control
- **Each try costs ~$0.01** — must be conservative
- **Max 10 tries per day**, with ~2 min between tries
- After 10 tries → **wait 24 hours** before next batch
- **Same IP detection**: If Vultr gives the same IP on consecutive retries, that IP is likely blocked → **wait 2 hours** before trying again
- Track seen IPs per account to detect duplicates

### Safety constraints (CRITICAL)
- **NEVER touch existing servers. NEVER. Only manage servers with bot's assigned prefix (e.g. `vultr-node-*`)**
- **Bot itself runs on Vultr** — one wrong API call can kill the bot's own server
- Every API call that lists, deletes, or modifies servers MUST filter by bot prefix FIRST
- Never call delete without verifying the server name starts with the prefix
- Reconciliation must strictly filter by prefix — ignore everything else
- Waiting for user to verify TM site accessibility from Vultr before implementing

---

## Rejected / Skipped

### Kamatera Provider
API lacks IP add/remove endpoints needed for our use case. Skipped.

### Manual Curl Test Button
Bot already tests every 2 minutes. Manual test adds complexity for minimal value.

---

## Priority Order
1. Force Replace (quick win, high value)
2. Force Redeploy (pairs with Force Replace)
3. Same Account Clone + reconciliation fix
4. Latency Tracking
5. Daily Summary
6. Stale Server Detection
7. Server Lifecycle History
8. Pause/Resume (partially done)
9. Vultr Provider (pending TM accessibility verification)
