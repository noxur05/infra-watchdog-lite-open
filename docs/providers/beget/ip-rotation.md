# Beget in-place IP rotation (implemented)

When a Beget server's public IP is blocked in TM, the bot now **rotates the IP
in place** instead of deleting and recreating the whole VPS — mirroring Selectel
floating-IP rotation (`src/providers/selectel/hooks.rs`). Implemented in
`src/providers/beget/hooks.rs` (`rotate_ip`) + `src/providers/beget/client.rs`
(`add_ip` / `delete_ip` / `ensure_private_network*`) + `src/provisioner/account.rs`
(`beget_rotate_ip`, wired into `monitor_server`'s curl-fail branch).

## The three rules (validated live against a real account, 2026-06)

1. **One public IP per customer at a time.** Adding a 2nd → `error.code =
   CUSTOMER_IP_LIMIT_REACHED`. ⇒ rotation must be **delete-then-add**;
   add-then-release is impossible.
2. **Can't detach the last public IP without a private network.** Deleting a
   VPS's only public IP → `CANNOT_DETACH_LAST_IP_ADDRESS`, *unless* a private
   network backs the VPS — then DELETE succeeds and the box stays up on its
   internal address (public ip becomes `""`).
3. **Daily public-IP allocation quota** (TimeWeb-style). Once spent, the add
   returns `CUSTOMER_IP_LIMIT_REACHED` even with the VPS at 0 public IPs — this
   is the rotation give-up signal.

Errors come back as **HTTP 200 with an `error` object in the body** (not via HTTP
status), the same envelope `create_server`/`delete_server` already check.

## Auth

Same `https://api.beget.com/v1/vps` API the existing `BegetClient` uses
(login+password → JWT via `api_request`). No separate panel login needed.

## Endpoints (`API_BASE = https://api.beget.com/v1/vps`)

| Action | Call | Notes |
|---|---|---|
| add public IP | `POST {API_BASE}/network` `{"id": vps_uuid}` | → `{"ip_address","additional_ip":{"ip"\|"address"}}`; `CUSTOMER_IP_LIMIT_REACHED` = quota |
| release public IP | `DELETE {API_BASE}/network/<ip>?ip_address=<ip>` | `CANNOT_DETACH_LAST_IP_ADDRESS` if no private net |
| list networks | `GET {API_BASE}/network` | `private_network[]` = account-wide subnets (no per-VPS attachment) |
| create private net | `POST {API_BASE}/private-network` `{"region": r}` | → `{"network":{"id"}}` |
| attach private net | `POST {API_BASE}/<vps_id>/private-network/<network_id>` `{id, network_id}` | |

The create *request* (`Manage_CreateVpsRequest`) takes `private_networks`
(**plural**); the VPS-info *response* (`Manage_VpsInfo`) exposes attached nets
under `private_network` (**singular**). The server-detail endpoint does not
reliably populate it, so the attach self-heal does not depend on reading it.

## Flow

- **create_server** ensures a per-region private network exists
  (`ensure_private_network`, reused across servers) and passes
  `private_networks:[{id}]` so the public IP is detachable from birth.
  Best-effort: a private-network failure doesn't block server creation; the
  rotation path attaches one lazily (`ensure_private_network_attached`).
- **on 3 curl fails** (`monitor_server`): `beget_rotate_ip` → `hooks::rotate_ip`:
  1. `delete_ip(old)` (attach a private net + retry once on
     `CANNOT_DETACH_LAST_IP`).
  2. `add_ip` →
     - **Ok** → persist new IP, `reset_curl_fails`, `unmark_marzban_synced`;
       caller does one `sync_marzban_hosts_global` → `Rotated`.
     - **`CUSTOMER_IP_LIMIT_REACHED`** → `Exhausted` → delete server + 12h wait.
     - **any other error** (box now public-IP-less) → `Failed` → delete server
       + 1h wait (no stale-IP orphan).

DNAT iptables are interface-scoped (`-i $IFACE` / `--to-destination dest`), never
the box's public IP, so an in-place IP change needs only the Marzban resync — no
iptables re-deploy.

## Rate-limit model

`rate_limit_config` is **ApiSignal** (`max_free_trials: 0`, `cooldown_hours: 12`)
— no DB trial-count cap. Spend is bounded by Beget's daily IP quota; on
exhaustion the server is deleted and the account waits 12h (an in-memory
`interruptible_sleep`, lost on restart — after a restart it re-rotates rather
than tight-looping, so it self-limits). A create-time `CUSTOMER_IP_LIMIT_REACHED`
also triggers a silent 12h back-off; `INSUFFICIENT_FUNDS` is kept separate (an
`Api` error → "top up + Retry now" admin alert).

## Not yet validated live

The full create-with-private-network + first rotation against a live account is
pending (Beget accounts are deactivated in prod). The next-cycle curl-test is the
safety net if a rotated IP doesn't bind or work.
