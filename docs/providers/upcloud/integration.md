# UpCloud — Integration Spec (API 1.3)

Synthesized from research of the official UpCloud 1.3 reference and product docs.
Base URL: `https://api.upcloud.com/1.3/` (version scheme `X.Y.Z`; only `X.Y` appears
in the path). Success on create is **async** (`202 Accepted`), so polling is required.

> **MONEY WARNING (read §8 before writing any code).** UpCloud has **no free tier**,
> billing is **hourly and prepaid**, and **a powered-off server is still billed** —
> only `DELETE` stops charges. Default `DELETE` keeps (and bills) the storage. Apply
> the same real-money discipline used for VDSina: hard create caps, delete-on-failure.

---

## 1. Authentication

Two auth methods, both via the `Authorization` header:

1. **Bearer API token (recommended for new integrations).**
   `Authorization: Bearer <token>`. Tokens carry the `ucat_` prefix, are created in
   the Control Panel or via CLI, and support expiry, IP restriction, and individual
   revocation. **API access is implicitly allowed when using a token** (independent of
   the account `allow_api` flag).
2. **HTTP Basic (discouraged, deprecation-path).**
   `Authorization: Basic <base64(username:password)>`. The architecture page states
   verbatim: *"use of this authentication method is discouraged, and it will be
   deprecated and disabled at a later date."* HTTP Basic additionally requires the
   account's `allow_api` flag set to `"yes"` in the Control Panel.

> **Decision needed (see §9):** notes recommend Bearer tokens as forward-compatible.
> Tokens have a **max validity of 8760 hours (365 days)** and expire — a long-lived
> bot must rotate them. HTTP Basic credentials don't expire but are on the deprecation
> path. The existing provider model stores per-account credentials in DB tables; either
> a token or username/password can be stored. Pick one and record the trade-off.

### Required headers (every request)

| Header | Value |
|---|---|
| `Authorization` | `Bearer <token>` **or** `Basic <base64(user:pass)>` |
| `Content-Type` | `application/json` |
| `Accept` | `application/json` |

### Sub-account setup (Control Panel)

Control Panel → **People → Accounts → "Create subaccount"**. Provide contact details,
set the **API password**, and enable **API access** in permissions.

- **Default scoping:** *"by default, the API account will have full permissions to any
  resources it creates but no access to existing servers or storage."* Recommendation
  is separate API credentials per app. ⚠️ This default means a leaked/over-broad
  sub-account can spin up billable servers — scope it down (see fields below).
- Sub-account restriction fields (Accounts API schema, exact names):
  - IP allow-list: `ip_filters` → array of `ip_filter` (CIDR / dash-range / single IP).
  - Server scope: `server_access` → `server[]` of `{ uuid, storage:"yes"/"no" }`; `"*"` = all.
  - Storage scope: `storage_access` → `storage[]` of UUIDs or `"*"`.
  - Tag scope: `tag_access` → `tag[]` of `{ name, storage:"yes"/"no" }`.
  - Network scope: `network_access` → `network[]` of UUIDs or `"*"`.
  - Access flags: `allow_api`, `allow_gui` (both `"yes"`/`"no"`).
- Sub-account endpoints: `POST /1.3/account/sub`, `PUT /1.3/account/sub/{username}`,
  `DELETE /1.3/account/sub/{username}`, `GET /1.3/account/list` (main account only).
- ⚠️ Uncertain: the exact Control Panel UI labels for disabling GUI access / setting the
  IP allow-list were **not** captured from the guide pages; the underlying API fields
  (`allow_gui`, `ip_filters`) are confirmed, the UI wording is not.

### Error response envelope (exact field names)

```json
{
  "error": {
    "error_code": "SERVER_NOT_FOUND",
    "error_message": "The server 00af0f73-... does not exist."
  }
}
```

Top-level `error` wraps string `error_code` + human-readable `error_message`. Map
`error_code` to `ProviderError` variants (e.g. auth failures → `ProviderError::Auth`).

**Sources:** https://developers.upcloud.com/1.3/2-architecture/ ·
https://developers.upcloud.com/1.3/3-accounts/ ·
https://developers.upcloud.com/1.3/24-api-tokens/ ·
https://upcloud.com/docs/guides/getting-started-upcloud-api-basic-auth/ ·
https://upcloud.com/docs/guides/getting-started-upcloud-api/

---

## 2. Key endpoints

All relative to `https://api.upcloud.com/1.3/`.

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/account` | Account balance (`credits`) + `resource_limits` — low-balance guard |
| `POST` | `/account/sub` | Create API sub-account |
| `PUT` | `/account/sub/{username}` | Update sub-account (scope/IP filters) |
| `DELETE` | `/account/sub/{username}` | Delete sub-account |
| `GET` | `/account/list` | List sub-accounts (main account only) |
| `POST` | `/account/tokens` | Create API token (returns `token` once) |
| `GET` | `/account/tokens/{id}` | Token details |
| `GET` | `/account/tokens?limit=&offset=` | List tokens |
| `DELETE` | `/account/tokens/{id}` | Delete token |
| `GET` | `/zone` | List zones (`list_regions`) |
| `GET` | `/plan` | List plans (find cheapest plan `name`) |
| `GET` | `/storage/template` | List OS templates (find Ubuntu 24.04 UUID) |
| `GET` | `/storage/public` | List public storages (templates are a subset) |
| `POST` | `/server` | **Create server** → `202 Accepted` |
| `GET` | `/server` | **List servers** (reconcile by `title`/`hostname`) |
| `GET` | `/server/{uuid}` | **Get server** (state, IPs) |
| `POST` | `/server/{uuid}/stop` | Stop server (required before delete) |
| `DELETE` | `/server/{uuid}?storages=1&backups=delete` | **Delete server + storages** |
| `DELETE` | `/storage/{uuid}?backups=` | Delete an orphaned/detached storage |
| `POST` | `/server/{uuid}/storage/detach` | Detach storage (before standalone delete) |

**Sources:** https://developers.upcloud.com/1.3/3-accounts/ ·
https://developers.upcloud.com/1.3/5-zones/ ·
https://developers.upcloud.com/1.3/7-plans/ ·
https://developers.upcloud.com/1.3/8-servers/ ·
https://developers.upcloud.com/1.3/9-storages/ ·
https://developers.upcloud.com/1.3/24-api-tokens/

---

## 3. Create-server request body (cheapest 1xCPU-1GB Ubuntu 24.04, public IPv4, generated root password)

`POST /1.3/server` → `202 Accepted`.

```json
{
  "server": {
    "zone": "fi-hel1",
    "title": "vpn-node-<account_id>",
    "hostname": "vpn-node-<id>",
    "plan": "1xCPU-1GB",
    "metadata": "yes",
    "storage_devices": {
      "storage_device": [
        {
          "action": "clone",
          "storage": "<UBUNTU_24_04_TEMPLATE_UUID>",
          "title": "vpn-node-<id>-disk",
          "tier": "maxiops"
        }
      ]
    },
    "networking": {
      "interfaces": {
        "interface": [
          {
            "type": "public",
            "ip_addresses": { "ip_address": [ { "family": "IPv4" } ] }
          }
        ]
      }
    },
    "login_user": {
      "username": "root",
      "create_password": "yes"
    },
    "password_delivery": "none"
  }
}
```

Field-by-field justification:

- `plan: "1xCPU-1GB"` — ⚠️ the *exact literal API plan `name`* for the smallest plan is
  **not confirmed from the 1.3 docs**. Product/config docs call it `STARTER-1xCPU-1GB`;
  the API may return `1xCPU-1GB`. **Verify against a live `GET /1.3/plan`** and use the
  string the API actually returns. (See §5.)
- `metadata: "yes"` — **required.** *"Metadata cannot be disabled when creating a server
  from a cloud-init template,"* and Ubuntu 22.04+ templates require metadata enabled.
  Passing `"no"` on a 24.04 clone **fails creation**.
- `storage_devices.storage_device[].action: "clone"`, `storage: <template uuid>` — clone
  the Ubuntu 24.04 template. **Omit `size`** to inherit the template's minimal size (10 GB
  Starter). For clone, `size` must be **greater than** the template size (range 10–1024),
  so only set it if you genuinely want a larger disk — it bills more.
- `tier: "maxiops"` — `maxiops` is the fast SSD tier; `hdd` is cheaper. ⚠️ Choose
  deliberately; `maxiops` costs more. The Starter plan's included storage is "Standard,"
  so leaving `tier` unset (inherit plan default) may be the cheapest correct choice —
  verify, do not blindly send `maxiops`.
- `networking…interface[].type: "public"` + `ip_addresses.ip_address[] { family: "IPv4" }`
  — provisions the single included public IPv4.
- `login_user.create_password: "yes"` with **no `ssh_keys`** — generates a one-time root
  password. `create_password` and `ssh_keys` are **mutually exclusive**: supplying
  `ssh_keys` flips `create_password` to `"no"`.
- `password_delivery: "none"` — default is `"email"`, which would email the root password
  in plaintext on every create. Set to `"none"`; read the password from the create
  response instead.

> ⚠️ **Cloud-init edge (uncertain, must verify on a real account):** docs say
> `create_password` is *"disabled on cloud-init templates"* and `ssh_keys` is *"required
> for cloud-init templates."* If the Ubuntu 24.04 template is cloud-init based, the API
> **may ignore `create_password` and return no password** — in which case we'd be forced
> to inject an SSH key and the "generated root password" model breaks. The `ServerInfo`/
> `CreateServerResult` types support an empty password (`Option<String>`), but the SSH
> path (`sshpass` with `ssh_user_for`) assumes a password. Confirm before shipping.

**Source:** https://developers.upcloud.com/1.3/8-servers/

---

## 4. Server lifecycle

### States (`state` field on `GET /1.3/server/{uuid}`)

| State | Meaning |
|---|---|
| `maintenance` | Transient — being configured (this is the state in the **create response**) |
| `started` | Running / ready |
| `stopped` | Powered off (**still billed**) |
| `error` | Inaccessible due to a cloud-service error |

Create is asynchronous: *"While the create operation is running, the server is in
`maintenance` state… As soon as the server is ready, the state will change to `started`."*

### Where IP + password become available

**Both are returned immediately in the create (`202`) response** while `state` is still
`"maintenance"` — you do **not** need to wait for `started` to read them.

- **Password:** `server.password` (root level of the `server` object). It is **NOT**
  `server.login_user.password` — there is no `login_user` object in the response.
  Distinct from `remote_access_password` (VNC/console).
  **`server.password` is returned ONLY at creation** — `GET /1.3/server/{uuid}` does
  **not** return it later. Persist it at create time or you lose the credential.
- **Public IPv4:** `server.ip_addresses.ip_address[]`, the element where
  `access == "public"` AND `family == "IPv4"` → `address`. Same array on
  `GET /1.3/server/{uuid}`.

Practical flow: capture IP + password from the create response, then **poll
`GET /1.3/server/{uuid}` until `state == "started"`** before SSHing in (the box is still
configuring during `maintenance`).

> ⚠️ `firewall: "on"` is the default in the create response example. It may block the
> curl/SSH blocked-site tests unless accounted for (disable, or open required ports).

### EXACT teardown sequence (leak-free)

A server **must be `stopped` before it can be deleted** (verbatim: *"The server state
must be `stopped`"*). Sequence:

1. `POST /1.3/server/{uuid}/stop` with body
   `{ "stop_server": { "stop_type": "soft", "timeout": "60" } }`
   (`stop_type` ∈ `soft`/`hard`; `timeout` 1–600 s — soft sends ACPI, escalates to hard on
   timeout). ⚠️ **The stop call returns while `state` is still `"started"`**: *"The state
   will change to `stopped` once the server has shut down."* No SLA is documented.
2. **Poll `GET /1.3/server/{uuid}` until `state == "stopped"`.** Do not delete on the stop
   call returning 200. If it never reaches `stopped`, fall back to `stop_type: "hard"`.
3. `DELETE /1.3/server/{uuid}?storages=1&backups=delete`
   - `storages` accepts `0,1,true,false`, **default `0`**. **`0` does NOT delete the disk
     — it detaches and KEEPS it, and kept storage keeps billing.** Pass `1` (or `true`).
   - `backups` (only meaningful when storages are deleted) accepts
     `keep,keep_latest,delete`, **default `keep`**. `keep` leaves billed backups. Pass
     `delete` for a clean teardown.
   - On delete, *"IP addresses used by the server are released"* — the included public
     IPv4 is freed automatically. (Floating IPs are separate — see §8.)

> **STORAGE-BILLING LEAK (explicit):** deleting with the **default `storages=0`** leaves an
> orphaned, fully-billed storage volume behind. **This is the #1 money leak for UpCloud.**
> The provisioner's `delete_server` MUST always send `?storages=1&backups=delete`. If a
> delete ever happened without it, recover via:
> detach (`POST /1.3/server/{uuid}/storage/detach`, if still attached) → ensure storage
> state `online` and unattached → `DELETE /1.3/storage/{uuid}?backups=delete`.

**Sources:** https://developers.upcloud.com/1.3/8-servers/ ·
https://developers.upcloud.com/1.3/9-storages/ ·
https://upcloud.com/docs/products/block-storage/billing/

---

## 5. Zones, cheapest plan, pricing (NO free tier)

### Zones — `GET /1.3/zone`

Response: `zones.zone[]`, each with `id`, `description`, `public` (`"yes"`/`"no"`),
`parent_zone` (private clouds only). Zone ID format `{country}-{city}{n}` (e.g.
`fi-hel1`). The API doc's inline example list is partly stale (includes a fake
`fi-fake-priv-cloud`). **Treat the live `GET /1.3/zone` as authoritative** and filter
`public == "yes"`. Current public zones (15) for reference:

| Zone ID | City | Zone ID | City |
|---|---|---|---|
| `au-syd1` | Sydney | `pl-waw1` | Warsaw |
| `de-fra1` | Frankfurt | `se-sto1` | Stockholm |
| `dk-cph1` | Copenhagen | `sg-sin1` | Singapore |
| `es-mad1` | Madrid | `uk-lon1` | London |
| `fi-hel1` | Helsinki #1 | `us-chi1` | Chicago |
| `fi-hel2` | Helsinki #2 | `us-nyc1` | New York |
| `nl-ams1` | Amsterdam | `us-sjo1` | San Jose |
| `no-svg1` | Stavanger | | |

(No India/Mumbai zone exists as of this writing.)

### Plans — `GET /1.3/plan`

Per-plan fields: `name`, `core_number`, `memory_amount` (MiB), `storage_size` (GB),
`storage_tier` (e.g. `"maxiops"` or `null`), `public_traffic_out` (GB/mo egress);
GPU plans add `gpu_amount`, `gpu_model`.

- **Cheapest plan:** `STARTER-1xCPU-1GB` per product/config docs — 1 vCPU, 1 GiB RAM,
  **10 GB Standard storage**, **capped at 5 instances/account**.
  ⚠️ The *exact literal API `name`* is not confirmed from the API doc (it only shows
  example plans like `1xCPU-2GB`). Verify via a live `GET /1.3/plan` and use the returned
  string in the create body.
- ⚠️ Storage-size conflict between sources: UpCloud docs say **10 GB** for the Starter
  variant; a third party (Spare Cores) lists 25 GB (likely the discontinued General
  Purpose 1xCPU-1GB). Use 10 GB; confirm via live API.

### Pricing (NO free tier — only a 7-day, ~€10 trial)

- Billing is **hourly, prepaid, capped at 672 h/mo (24×28)**, and **continues while the
  server is stopped** — only delete stops it.
- 1xCPU-1GB ≈ **$7.00/mo, ~$0.0121/hr** (approximate; "from $3/mo" advertising refers to a
  smaller/discounted tier). Storage/network beyond the plan allotment is billed hourly.
- After the trial expires you cannot deploy until a one-time deposit (min €10). Verify
  exact current EUR/USD pricing at https://calc.upcloud.com/.

**Sources:** https://developers.upcloud.com/1.3/5-zones/ ·
https://developers.upcloud.com/1.3/7-plans/ ·
https://upcloud.com/docs/products/cloud-servers/availability/ ·
https://upcloud.com/docs/products/cloud-servers/configurations/ ·
https://upcloud.com/docs/getting-started/free-trial/ · https://upcloud.com/pricing/

---

## 6. Ubuntu 24.04 template

**Discover at runtime — do not hardcode.** The only Ubuntu UUID published in official docs
is **22.04**: `01000000-0000-4000-8000-000030220200` (title *"Ubuntu Server 22.04 LTS
(Jammy Jellyfish)"*). UpCloud template UUIDs follow the pattern
`01000000-0000-4000-8000-0000302NNN00`, so a 24.04 UUID like
`01000000-0000-4000-8000-000030240200` is **plausible by pattern but could NOT be verified
against any source** — do not hardcode it.

**Runtime discovery:** `GET /1.3/storage/template`, response `storages.storage[]`, match
on `title` containing `"Ubuntu Server 24.04"` (or `"Noble"`), then read its `uuid`. Use
that UUID as `storage_devices.storage_device[].storage` in the create body. This mirrors
the existing Gcore/Selectel runtime image-discovery pattern.

Template list element fields: `uuid`, `title`, `type`, `access`, `size`, `tier`, `state`,
`zone`, `encrypted`, `license`, `labels[]`, `template_type`. The two load-bearing fields
for matching are `uuid` and `title`.

> ⚠️ Hardcoding an unverified/retired UUID is itself a leak risk: a wrong UUID causes
> repeated failed creates (each potentially billable) or silently clones the wrong OS.

**Sources:** https://developers.upcloud.com/1.3/9-storages/ ·
https://developers.upcloud.com/1.3/8-servers/ ·
https://upcloud.com/docs/products/block-storage/templates/ ·
https://upcloud.com/docs/changelog/2024-04-26-ubuntu2204-release/

---

## 7. Mapping to the `CloudProvider` trait

Trait in `src/provider.rs`. UpCloud is a **DNAT-mode** provider (rents a server, runs
iptables MASQUERADE + PREROUTING; not a direct-node), so it behaves like
TimeWeb/Beget/Vultr/Selectel.

| Trait method | UpCloud call(s) | Notes / gotchas |
|---|---|---|
| `create_server(location)` | `POST /1.3/server` (body in §3); `location` → `zone` | Returns `CreateServerResult { server_id = server.uuid, root_pass = Some(server.password), warning }`. **Capture `server.password` here — it's never returned again.** Resolve the Ubuntu 24.04 template UUID at runtime (§6) and the plan `name` (§5) before building the body. `202` ≠ ready: state is `maintenance`. If create returns 2xx but you fail to record the row, that's a money leak (§8). |
| `get_server(server_id)` | `GET /1.3/server/{uuid}` | Map `state` → normalized status (`maintenance`/`started`/`stopped`/`error`). `ipv4` = `ip_addresses.ip_address[]` where `access=="public" && family=="IPv4"`. **`root_pass` is `None` here** — `GET` does not return the login password (only `remote_access_password`, which is VNC). Provisioner must rely on the password captured at create. |
| `delete_server(server_id)` | `POST /1.3/server/{uuid}/stop` → poll `GET` until `state=="stopped"` → `DELETE /1.3/server/{uuid}?storages=1&backups=delete` | **Multi-step, not a single call.** A `started` server cannot be deleted. **MUST pass `storages=1&backups=delete`** or storage/backups keep billing (§4, §8). Stop is async — poll, with a hard-stop fallback. Make idempotent: if `GET` returns `SERVER_NOT_FOUND`, treat delete as success. |
| `list_servers(name_prefix)` | `GET /1.3/server` | Response `servers.server[]` includes `uuid`, `hostname`, `title`, `state`, `zone`, `plan` — **no per-server GET needed** for reconciliation. Map to `RemoteServer { id=uuid, name=title (or hostname), region=Some(zone) }`. ⚠️ Neither `title` nor `hostname` is enforced unique — prefix matching can mis-identify a human-named server. Consider also gating on `zone`/`plan`, or use the structured `labels`/`tags` arrays as a robust bot-owned marker (affects whether the reconciler fails to delete = leak, or wrongly deletes). |
| `list_regions()` | `GET /1.3/zone` | Filter `public=="yes"`; map `zone.id` → `Region.id`, `zone.description` → `Region.description`. Add a `region_to_country` arm in `utils.rs` for each zone ID, else it renders `🌐 Unknown` (CLAUDE.md). |
| `cleanup_resources(active_ids)` | (optional) `GET /1.3/storage` / `GET /1.3/server` to find orphans → `DELETE /1.3/storage/{uuid}?backups=delete` | TimeWeb uses this for orphan floating IPs. For UpCloud the analog is **orphaned storages** (left if any delete ever ran without `storages=1`) and possibly **floating IPs** if used. Safe default: scan storages not attached to a server and not in `active_ids`, delete them. If we never allocate floating IPs and always delete with `storages=1`, this can be a near no-op — but a storage-sweep is cheap insurance against the #1 leak. |
| `rate_limit_config()` | n/a — **no published UpCloud rate limit** | UpCloud's 1.3 docs document **no 429 / rate-limit / Retry-After**. There is no provider-side backstop. Use a **self-imposed `CountBased` cap** like VDSina (e.g. small `max_free_trials` with `cooldown_anchor: Latest`), counting `servers` rows in the cooldown window. There is no free tier, so this cap is purely a runaway-spend guard. Tune the numbers to budget. |
| `server_name_prefix()` | n/a (returns a `&'static str`) | Return e.g. `"vpn-node-"` (or `"upcloud-node-"`). Used as both the `title`/`hostname` prefix on create and the `list_servers` reconciliation filter. Keep the create body and this constant in sync. Note CLAUDE.md: the client-facing Marzban host label never contains a provider name — that's separate from this reconciliation prefix. |

Additional integration touch-points (from CLAUDE.md):
- SSH: `utils::ssh_user_for("upcloud")` should return `root` (UpCloud `login_user.username`
  defaults to `root`); standard DNAT iptables script, no `sudo_wrap` needed.
- Register in `providers::all()` (`src/providers/mod.rs`) + create the
  `src/providers/upcloud/` folder (`client.rs`, `db.rs`, `admin.rs`, `buttons.rs`,
  `mod.rs`). New DB table `upcloud_accounts` via a new idempotent migration.
- Use `utils::build_http_client()` (timeout) for the HTTP client; never bare `Client::new()`.

---

## 8. MONEY / LEAK RISKS

Every way a created resource could keep billing if an error path is mishandled, with the
mitigation:

1. **Orphan storage on delete (the #1 leak).** `DELETE /1.3/server/{uuid}` defaults to
   `storages=0`, which **detaches and KEEPS the disk — kept storage keeps billing.**
   *Mitigation:* `delete_server` MUST always send `?storages=1&backups=delete`. Add a
   `cleanup_resources` storage-sweep that deletes unattached storages not in `active_ids`.
2. **Billed backups.** Deleting storage with `backups=keep` (default) leaves backups billed
   as additional storage. *Mitigation:* always `backups=delete` on teardown.
3. **Stopped ≠ free.** A powered-off server is **still billed hourly** (cap 672 h/mo). A
   teardown that only stops (or fails between stop and delete) leaks money silently.
   *Mitigation:* never treat stop as teardown; always proceed to `DELETE`; on failure,
   retry delete; surface via `categorize_provisioning_error` + 24h-dedup notify.
4. **Server created but never recorded.** `POST /1.3/server` returns `202` and bills
   immediately; if the process crashes / the DB write fails before persisting `uuid`, the
   server runs untracked and unbilled-against-cap. *Mitigation:* persist the `servers` row
   (with `uuid` + `password`) immediately on a 2xx create; on startup/periodically,
   reconcile via `list_servers` (prefix/label) and delete any UpCloud server with the bot
   prefix that has no matching active DB row.
5. **Stuck `stopping` / never reaches `stopped`.** Stop is async with **no documented SLA**;
   if it hangs, the delete precondition (`state == stopped`) is never met and the server
   keeps billing. *Mitigation:* poll with a timeout; escalate to `stop_type: "hard"`;
   alert if still not `stopped` after the bound.
6. **Lost login password.** `server.password` is returned **only** in the create response;
   `GET` never returns it. Losing it doesn't directly bill but blocks SSH teardown logic /
   forces a password reset, indirectly prolonging a billed server. *Mitigation:* persist
   `root_pass` atomically with the create row.
7. **Separate / floating-IP billing.** The plan's 1 included public IPv4 is released on
   delete, but **additional public IPv4s are billed hourly** and **floating IPs are "billed
   per attached IP" and are NOT documented as released by server delete.** *Mitigation:*
   don't allocate extra/floating IPs (the create body in §3 requests only the included
   public IPv4); if any are ever allocated, detach/release them explicitly in
   `cleanup_resources`. (IPv6 is free.)
8. **Wrong/retired template UUID → repeated failed creates.** Hardcoding an unverified
   24.04 UUID can cause repeated billable failed attempts or clone the wrong OS.
   *Mitigation:* discover the UUID at runtime (§6).
9. **`password_delivery` default emails the password.** Default `"email"` ships the root
   password in plaintext each create (a security leak, not a billing one).
   *Mitigation:* `password_delivery: "none"`.
10. **No provider-side rate limit.** UpCloud publishes no 429/backstop, so a
    retry/oscillation loop in the provisioner = duplicate paid servers, each billed from
    `202`. *Mitigation:* self-imposed `CountBased` create cap (§7) + delete-on-failure +
    the global `MARZBAN_SYNC_LOCK`/per-account state machine that prevents double-create.
11. **Over-broad sub-account / leaked credentials.** A sub-account defaults to *"full
    permissions to any resources it creates"*; a leaked Bearer token with
    `can_create_tokens=true` or unrestricted `allowed_ip_ranges` can mint tokens / spin up
    servers. *Mitigation:* scope the sub-account (`server_access`/`storage_access`/
    `tag_access`), set `ip_filters`/`allowed_ip_ranges`, leave `can_create_tokens=false`.
    Use `GET /1.3/account` `credits` as a low-balance guard.

**Sources:** https://developers.upcloud.com/1.3/8-servers/ ·
https://developers.upcloud.com/1.3/9-storages/ ·
https://upcloud.com/docs/products/block-storage/billing/ ·
https://upcloud.com/docs/products/networking/billing/ ·
https://developers.upcloud.com/1.3/24-api-tokens/ ·
https://developers.upcloud.com/1.3/3-accounts/

---

## 9. Open questions / not confirmed by the docs

1. **Exact API plan `name` for the cheapest plan.** Product docs say `STARTER-1xCPU-1GB`;
   the API doc only shows examples like `1xCPU-2GB`. Confirm with a live `GET /1.3/plan`.
2. **Ubuntu 24.04 template UUID.** Not published anywhere official; the
   `…000030240200` guess is unverified. Discover at runtime (§6).
3. **Starter storage size:** 10 GB (UpCloud docs) vs 25 GB (third party). Confirm via API.
4. **Cloud-init password behavior.** Docs say `create_password` is *"disabled on cloud-init
   templates"* and `ssh_keys` is *"required for cloud-init templates."* Unknown whether the
   24.04 template is cloud-init based and therefore returns **no** generated password
   (which would break the password-based SSH model). Verify on a real account.
5. **Rate limit / 429 behavior.** Entirely undocumented in 1.3. Absence noted, **not**
   interpreted as "no limit" — client should still handle non-2xx (incl. possible 429)
   with backoff.
6. **Floating-IP release on server delete.** Docs confirm the plan's public IPv4 is
   released but do **not** state floating IPs are. Assume separate release.
7. **Soft-stop completion time / SLA.** Not documented — must poll for `state=="stopped"`.
8. **Auth choice (Bearer token vs HTTP Basic).** Tokens are recommended/forward-compatible
   but expire (max 365 days, need rotation); HTTP Basic doesn't expire but is on the
   deprecation path and needs `allow_api=yes`. Decide which to store per-account.
9. **`title`/`hostname` round-trip + uniqueness.** Not documented as globally unique; values
   are *expected* (not guaranteed) to persist verbatim. Reconciliation by prefix alone may
   mis-identify servers — prefer structured `labels`/`tags` if feasible.
10. **Control Panel UI labels** for disabling GUI access / setting the IP allow-list on a
    sub-account were not captured (the API fields `allow_gui`/`ip_filters` are confirmed).
11. **Exact per-GB storage / per-hour IP prices** are not in the API docs (redirect to the
    pricing page). Treat figures in §5/§8 as approximate; confirm at
    https://calc.upcloud.com/ and https://upcloud.com/pricing/.
