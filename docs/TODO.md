# VPN Watchdog — Implementation Plan

## Environment
- Bot runs on Vultr free tier (0.5GB RAM, 1 CPU) — must be highly optimized, no freezing
- Bot is NOT in Turkmenistan — SSHes into TimeWeb servers and curls from there
- Server config: minimal Moscow (preset 4795, msk-1, os 109 (Debian 13), no backups, no DDoS guard)
- SSH Key ID: 340927
- sshpass installed on Vultr deploy server

---

## Phase 1: Provision a Reachable Server

1. Create server via TimeWeb API (msk-1, preset 4795, os 109, SSH key 340927, floating_ip: "create_ip")
2. **IP limit hit** (warning: "floating_ip_daily_limit_exceeded") → delete server immediately (don't poll), wait 2 hours, retry. IP limit resets after 24h, we check every 2h just in case
3. Poll `GET /api/v1/servers/{id}` every **30 seconds** until `status: "on"`, extract `root_pass` + IPv4
4. SSH into server, run curl test script: **2 soft attempts** (5s timeout, ignore result) + **1 hard attempt** (5s timeout, determines pass/fail)
5. **Success** → proceed to Phase 2
6. **Fail** → delete server, wait **2 minutes**, create new one, repeat
7. New servers that fail SSH → just delete and retry, no admin notification

---

## Phase 2: Deploy Port Forwarding Rules

### Admin provides via Telegram bot:
- Destination IP
- Destination port
- Local port (unique per server)
- Multiple destinations possible per server

### After server curl test passes:
1. SSH into server
2. For each destination rule in DB, run port forwarding (non-interactive):
   - Enable IP forwarding (`sysctl net.ipv4.ip_forward=1`) — **idempotent** (grep before append to sysctl.conf)
   - Detect network interface
   - Add NAT masquerading — **idempotent** (`iptables -C` check before `-A` append)
   - Add PREROUTING DNAT rules (TCP + UDP) — **idempotent**
   - Add FORWARD rules — **idempotent**
   - Save iptables rules
3. Write a txt file on server recording all forwarded ports/IPs
4. Mark server as "script_deployed" in DB
5. Sync Marzban hosts — if succeeds, mark `marzban_synced=true`; if fails, retry every monitoring cycle

### Per-destination deploy failure handling:
- Each destination deploy has a **120s timeout** to detect freezes
- On failure/freeze: mark `deploy_failed=true` in DB, notify admin (2 notifications), wait 20 minutes, **skip** that destination, continue with others
- Failed destinations are logged but not retried on the same server

### Rules:
- **NEVER reboot** the server (iptables lost on reboot)
- **NEVER modify/remove** existing iptables rules on a running server
- **Append-only**: if admin adds new destination → SSH in, run script for new rule only
- **All iptables rules are idempotent** — safe to re-run (uses `-C` check before `-A` append)
- IP forwarding setup (sysctl, masquerade) only needs to run once per server

### Admin destination management (Telegram bot):
- **Add**: save to DB. If server already running → SSH in, add new rule only
- **Delete**: remove from DB only. Do NOT touch iptables. Future servers won't get this rule
- **Edit**: if local port changed and doesn't conflict with existing → treat as new rule (add to DB + add to server). Old rule stays in iptables but removed from DB

### DB is source of truth for future servers. Current server may have stale rules — that's fine.

---

## Phase 3: Monitoring & Auto-Recovery

### Intervals
- **Server creating/installing**: poll every **30 seconds**
- **Server on, not deployed**: check every **30 seconds**
- **Server deployed (monitoring)**: check every **2 minutes**
- **IP limit hit**: check every **2 hours** (limit resets after 24h)
- **No server, destinations exist**: create new server, then **2 minutes** before next cycle

### Normal Monitoring (every 2 minutes)
- SSH into server and run curl test script (2 soft + 1 hard, 5s timeout each)
- 1st consecutive fail → soft fail, log, continue
- 2nd consecutive fail → soft fail, log, continue
- 3rd consecutive fail → server is dead, delete it, go to Phase 1

### Marzban Sync Retry
- If `marzban_synced=false` on a deployed server, retry sync every monitoring cycle until it succeeds
- Ensures bot restarts or transient Marzban failures don't leave hosts out of sync

### SSH Failure on Running Server (script already deployed, NOT new servers)
- 1st SSH fail → soft fail
- 2nd SSH fail → soft fail
- 3rd SSH fail → notify admin via Telegram (send 2 notifications)
- Wait 20 minutes for admin response
- No response after 20 min → delete server, go to Phase 1

### IP Limit During Re-Provisioning
- If IP limit hit while replacing dead server → delete server immediately (don't save to DB), wait 2 hours
- During 2-hour wait, check every 2 hours instead of normal 2-minute interval

### Server Replacement
- When a server is deleted and replaced, new server gets ALL current destination rules from DB applied fresh
- Deleted/edited rules (removed from DB) will NOT be on the new server

---

## Startup Reconciliation

On every bot start (before first provisioner cycle):
1. List all `vpn-node-*` servers from TimeWeb API
2. Compare with DB active servers
3. **Remote server not in DB** → orphaned, delete it + its floating IP
4. **DB server not on TimeWeb** → soft-delete from DB
5. **Floating IPs not bound to active servers** → delete (cleans up leaked IPs)

---

## DB Schema

### tw_servers
- `id`, `tw_server_id`, `ipv4`, `root_pass`, `status`
- `script_deployed` — iptables rules deployed
- `ip_setup_done` — sysctl + masquerade done
- `marzban_synced` — Marzban hosts synced after deploy
- `consecutive_curl_fails`, `consecutive_ssh_fails`, `ssh_fail_notified_at`
- `created_at`, `deleted_at`

### destinations
- `id`, `dest_ip`, `dest_port`, `local_port`, `country`
- `is_active`, `deployed_on_server`
- `deploy_failed` — marked when deploy fails/freezes on a server
- `created_at`

### Migrations
- 001-004: original (users, subscriptions, nodes, check_results)
- 005: tw_servers
- 006: destinations
- 007: add marzban_synced to tw_servers
- 008: add deploy_failed to destinations

Migration runner handles duplicate column errors gracefully (idempotent re-runs).
