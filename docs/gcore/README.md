# Gcore Cloud API — Reference for vpn-watchdog

Collected from https://gcore.com/docs/api-reference (Apr 2026) and **verified end-to-end against a live account** (create → poll → get → delete → orphan check; firewall create + attach/detach on running instance re-verified May 2026). Used to implement the Gcore provider as a **direct-node** provider (Marzban node installed on the server, no proxy chain).

## Auth

- Base URL for Cloud: `https://api.gcore.com/cloud`
- Header: `Authorization: APIKey <YOUR_TOKEN>`
- Both `APIKey` and `apikey` prefixes work (case-insensitive on the server); prefer the docs spelling `APIKey`.
- Two key formats seen in the wild: legacy `<id>$<secret>` (the `$` warning above) and **Permanent API Tokens** of the form `<id>_<secret>` (e.g. `30169_b350db82…`). Both work with the same `Authorization: APIKey …` header. The underscore form has no shell-escape gotcha.
- Token has no refresh cycle (unlike Selectel Keystone) — one static key per account.
- Sanity ping: `GET https://api.gcore.com/iam/clients/me` should return 200 with client info (also returns `capabilities` and `serviceStatuses` — confirm `CLOUD.status == "active"` and `CLOUD.enabled == true` before trusting the key for cloud calls).

## Getting project_id

The API key alone doesn't reveal the project. List projects to find it:

```
GET /cloud/v1/projects
→ { "count": 1, "results": [{ "id": <PROJECT_ID>, "name": "Default", "is_default": true, ... }] }
```

A fresh account has exactly one "Default" project. Store its `id` per-account in DB alongside the key.

## Path scoping

Almost every resource is scoped as `.../{project_id}/{region_id}`.
- `project_id` — integer, from `/cloud/v1/projects`
- `region_id` — integer, from `GET /cloud/v1/regions`

## Async task model

Write operations (create, delete, resize, attach) return a list of task UUIDs instead of the resource itself:
```json
{ "tasks": ["d478ae29-dedc-4869-82f0-96104425f565"] }
```
Poll `GET /cloud/v1/tasks/{task_id}` until `state` is `FINISHED` or `ERROR`. `task_type` identifies the operation (e.g. `create_vm`). `created_resources.instances[]` gives you the new instance ID once finished.

Live timings (Almaty, standard tier):
- Create → RUNNING → FINISHED: ~5–10s (much faster than Selectel; image must already be local to the region).
- Delete → FINISHED: ~1–2s.

So a 5s polling interval is plenty, and a 2-minute hard timeout is comfortable.

## Endpoints we need

### Regions
`GET /cloud/v1/regions` — returns `{ count, results: [{ id, display_name, country, has_kvm, has_basic_vm, state, ... }] }`. Filter by `has_kvm: true, state: "ACTIVE"`.

**Closest to Turkmenistan** (verified live):
- `id=136` **Tashkent, UZ** — first choice for latency (shared fiber path to TM).
- `id=46` **Almaty, KZ** — solid second.
- Both have `has_basic_vm: true`.

### Flavors
`GET /cloud/v1/flavors/{project_id}/{region_id}?include_prices=true`

Each flavor record returns both `price_per_hour` and `price_per_month` (plus `currency_code`). No need to compute monthly yourself.

Tiers seen (Tashkent pricing re-verified May 2026, EUR):
| Flavor | vCPU / RAM | €/hour | €/month |
|---|---|---|---|
| `g2s-shared-1-1-25` | 1 / 1 GB / 25 GB bundled | 0.000972 | (Basic VM, often capped) |
| `g2s-shared-1-2-25` | 1 / 2 GB / 25 GB bundled | 0.00325 | (Basic VM, often capped) |
| `g1-standard-1-2` | 1 / 2 GB, external volume | **0.021216** | **15.275** |
| `g1-standard-2-4` | 2 / 4 GB, external volume | 0.042534 | ~30.62 |

`g0-*` tier observed in earlier doc revisions is **retired** — list the API to discover what's available rather than hardcoding flavor strings. The `g1-cpu-*` (1:1 CPU:RAM) and `g1-memory-*` (1:8 CPU:RAM) variants exist for compute-/memory-bound workloads but are unnecessary for a Marzban node. `g1w-*` is the Windows variant (~2× the cost — Windows licensing).

**Gotcha (verified live): shared `g2s-shared-*` tiers have a per-region cap that a new account often already hits.** Expect `403 ForbiddenError "The region X has reached the limit of flavor Y"`. Fall back to the `g1-standard-*` "Cloud Server" tier (we verified `g1-standard-1-2` worked first try in Tashkent on a fresh account). At €0.021216/hr that's ~€15.28/month for a 24/7 server, or **~€8.91/month for a 14 h/day window**.

`-25` / `-50` / `-80` suffix on `g2s-shared-*` flavors means disk is **bundled** in the flavor. Non-shared flavors don't include disk — you size the volume yourself.

**"Virtual Instance" vs "Virtual Machine"** in Gcore terminology — verified there is no difference. The dashboard uses both labels interchangeably (their docs page titled "Types of Virtual Machines" lives at `/docs/cloud/virtual-instances/types-of-virtual-machines`). API-side: only `/cloud/v?/instances/` exists; probing `/cloud/v?/{vms,virtual_machines,virtualmachines}` returns 404. The task type returned for create is `create_vm` regardless of flavor tier. So "VI", "VM", and "Cloud Server" all refer to the same resource — the distinction live consumers care about is **flavor family**: shared `g2s-shared-*` (= "Basic VM" in marketing) vs dedicated `g1-*` (= "Virtual Server" / "Cloud Server" in marketing).

### Images
`GET /cloud/v1/images/{project_id}/{region_id}` — filter `os_distro == "ubuntu"`, pick an LTS x86_64. Image IDs differ per region (verified). Examples captured live:
- Almaty: `0de74e2c-9e4e-4f1a-9625-0a05038fac67` — ubuntu-24.04-x64
- Tashkent: `3414c5ad-cf5f-458b-95e4-9fa3c02c6fa1` — ubuntu-24.04-x64

At runtime: list, filter by `name.starts_with("ubuntu-24.04-x64")`, pick first.

### Create instance
`POST /cloud/v2/instances/{project_id}/{region_id}` — note **v2**, not v1.

Minimal body (verified working):
```json
{
  "flavor": "g1-standard-1-2",
  "name": "gcore-watchdog-BWD",
  "interfaces": [{ "type": "external" }],
  "volumes": [{
    "source": "image",
    "image_id": "<image-uuid>",
    "size": 10,
    "boot_index": 0,
    "type_name": "standard"
  }],
  "password": "<root-pw>"
}
```

Key behaviors (verified live):
- `interfaces: [{ type: "external" }]` attaches a **direct public IP on `pub_net`** — NOT a separate floating IP. The address shape is `addresses.pub_net[0]` with `type: "fixed"`, not `type: "floating"`. This is important for the delete cleanup — see below.
- `password` sets the root password on Linux. Cannot coexist with `user_data` (docs: "user_data is ignored when the password is specified"). Pick one.
- `user_data` is base64-encoded cloud-init.
- For shared-bundled flavors (`g2s-shared-1-1-25`), volume `size` must match the bundle (25). For non-shared flavors, any reasonable size works (10 GB tested).
- Returns `{ tasks: [task_id] }`. Poll the task → `created_resources.instances[0]` → new instance UUID.

### Get instance
`GET /cloud/v1/instances/{project_id}/{region_id}/{instance_id}`

Useful fields:
- `status` — `"ACTIVE"`/`"BUILD"`/`"ERROR"`. Poll until ACTIVE to know SSH is reachable.
- `addresses` — shape `{ "<network_name>": [{ "addr": "x.x.x.x", "type": "fixed"|"floating", "subnet_name": "...", "subnet_id": "..." }] }`. For `interfaces:[{type:"external"}]` this is `addresses.pub_net[0].addr` with `type: "fixed"`.
- `volumes` — `[{ "id": "...", "volume_type": "..." }, ...]`. Also returned in the create task's `created_resources.volumes` — so you don't usually need this GET just for volume IDs.
- `security_groups` — `[{"name": "..."}]` — what's actually attached. Port-level SG assignment is also visible via `GET .../instances/{id}/ports`.
- `flavor.flavor_id` — the flavor string.

### Delete instance (with cleanup)
`DELETE /cloud/v1/instances/{project_id}/{region_id}/{instance_id}?delete_floatings=true&volumes=<comma-separated>`

Query params (always set both):
- `delete_floatings=true` — delete attached floating IPs. For the `{type:"external"}` path the IP is not a floating IP, so this is a no-op there, but it's safe and also covers the case where we later allocate an explicit floating IP.
- `volumes=<id1,id2,...>` — **delete attached volumes**. This is the critical one. Without it, volumes persist after instance deletion and keep billing.
- Alternative: `delete_floatings` and `floatings` are mutually exclusive per docs — pass only `delete_floatings=true`.

**Verified live**: after a DELETE with both params, `list-instances`, `list-volumes`, and `list-floatingips` in that region all return `count: 0`. No orphans.

### Tasks
- `GET /cloud/v1/tasks/{task_id}` — single task polling
- States observed live: `RUNNING`, `FINISHED`. Docs mention `NEW` and `ERROR` too.
- `created_resources` populated on finish: `.instances[]`, `.volumes[]`, `.floatingips[]`, `.networks[]`, etc.

### SSH Keys (optional, not used)
Password auth via `sshpass` is the pattern already used by TimeWeb/Beget/Selectel — reuse it via the `password` field on create. If we ever want key auth:
- `GET /cloud/v1/ssh_keys/{project_id}` — list
- `POST /cloud/v1/ssh_keys/{project_id}` — create/import
- `DELETE /cloud/v1/ssh_keys/{project_id}/{ssh_key_id}` — delete

### Security Groups — firewall

Every instance gets a security group attached at its port. Without specifying one, Gcore assigns the region's `default` SG, which is very permissive (TCP 80–65535 ingress from any). For Marzban-node production use we deploy a tight per-account SG.

**Gcore platform policy**: **egress TCP and UDP port 25 is blocked** — any rule that would open it (e.g. a catch-all egress rule) is rejected with `400 "Opening egress port 25 for TCP or UDP traffic is prohibited. Please, contact support."`. Works around by splitting egress into `1–24` and `26–65535` ranges, mirroring what Gcore's own default SG does.

**Create SG** (verified live — 11 rules, returned SG ID `160349d2-d5db-4be7-a747-612531daf88f` in the test):

`POST /cloud/v1/securitygroups/{project_id}/{region_id}`
```json
{
  "security_group": {
    "name": "gcore-watchdog-BWD",
    "description": "gcore-watchdog firewall (SMTP port 25 intentionally blocked)",
    "security_group_rules": [
      {"direction": "ingress", "ethertype": "IPv4", "protocol": "tcp",  "port_range_min": 22,    "port_range_max": 22,    "remote_ip_prefix": "0.0.0.0/0"},
      {"direction": "ingress", "ethertype": "IPv4", "protocol": "tcp",  "port_range_min": 443,   "port_range_max": 443,   "remote_ip_prefix": "0.0.0.0/0"},
      {"direction": "ingress", "ethertype": "IPv4", "protocol": "udp",  "port_range_min": 443,   "port_range_max": 443,   "remote_ip_prefix": "0.0.0.0/0"},
      {"direction": "ingress", "ethertype": "IPv4", "protocol": "tcp",  "port_range_min": 62050, "port_range_max": 62051, "remote_ip_prefix": "0.0.0.0/0"},
      {"direction": "ingress", "ethertype": "IPv4", "protocol": "icmp"},
      {"direction": "egress",  "ethertype": "IPv4", "protocol": "tcp",  "port_range_min": 1,     "port_range_max": 24},
      {"direction": "egress",  "ethertype": "IPv4", "protocol": "tcp",  "port_range_min": 26,    "port_range_max": 65535},
      {"direction": "egress",  "ethertype": "IPv4", "protocol": "udp",  "port_range_min": 1,     "port_range_max": 24},
      {"direction": "egress",  "ethertype": "IPv4", "protocol": "udp",  "port_range_min": 26,    "port_range_max": 65535},
      {"direction": "egress",  "ethertype": "IPv4", "protocol": "icmp"}
    ]
  }
}
```
Note the `security_group` wrapper object — POST body is `{ "security_group": { ... } }`, not a bare object. Response includes `id` (UUID) and the expanded rule list with per-rule IDs.

**Attach SG at instance-create time** — put the SG's UUID in the `interface.security_groups[]` array:
```json
{
  "flavor": "g1-standard-1-2",
  "name": "gcore-watchdog-BWD",
  "interfaces": [{
    "type": "external",
    "security_groups": [{"id": "<sg-uuid>"}]
  }],
  "volumes": [...],
  "password": "..."
}
```
Verified: attaches only our SG — the default SG does NOT get added as fallback.

**Verify attachment** after create:
- `GET /cloud/v1/instances/{proj}/{reg}/{id}` → `security_groups: [{"name": "gcore-watchdog-BWD"}]`
- `GET /cloud/v1/instances/{proj}/{reg}/{id}/ports` → port's `security_groups` array shows the UUID.

**Attach / detach SG on a running instance** (verified live May 2026 — both return `204 No Content`, take effect immediately on the live data plane):

```
POST /cloud/v1/instances/{proj}/{reg}/{instance_id}/addsecuritygroup
POST /cloud/v1/instances/{proj}/{reg}/{instance_id}/delsecuritygroup
```
Body for both: `{"name": "<sg-name>"}` (NOT the UUID — the **name**). No body wrapper, just a flat object.

Endpoint shapes that **don't** work and look plausible (probed, all returned 404 or 405): `/action`, `/add_security_group`, `/security_groups`, `/v2/instances/.../addsecuritygroup`, `DELETE /instances/.../security_groups/<id>`. Stick to the `addsecuritygroup` / `delsecuritygroup` pair.

Useful pattern: at instance-create time we attach our SG (so the default never gets a window where it's the only one), then immediately call `delsecuritygroup` with `{"name":"default"}` so the running instance has only our SG. After both calls, `GET .../ports` should show the port's `security_groups` is just `[{name: "gcore-watchdog-<BOT_CODE>"}]`.

**Add a rule to an existing SG** (for when we later need a new inbound port, e.g. a second Xray protocol): `POST /cloud/v1/securitygroups/{proj}/{reg}/{sg_id}/rules` — same rule schema as during create.

**Delete SG**: `DELETE /cloud/v1/securitygroups/{proj}/{reg}/{sg_id}` → 204 on success. Fails if the SG is still attached to any port.

**Default SG is permanent** — `DELETE` on the `default` SG is refused; it's created automatically per region. Fine for us — we create our own alongside it.

**Strategy for the provider**: at account-setup time, create one `gcore-watchdog-<BOT_CODE>` SG per region the account uses, persist `(account_id, region_id, sg_id)` in a small `gcore_security_groups` table, and reuse it for every instance in that (account, region) pair. On account deactivation, delete instances first, then delete the SGs.

### Live firewall verification

End-to-end confirmation that the SG actually enforces rules was run against a VM with a **wide-open-ingress** variant (`vpn-watchdog-open`, all TCP/UDP 1–65535 + ICMP ingress from 0.0.0.0/0; egress identical to the tight version). Build this SG with:

```json
"security_group_rules": [
  {"direction": "ingress", "ethertype": "IPv4", "protocol": "tcp",  "port_range_min": 1, "port_range_max": 65535, "remote_ip_prefix": "0.0.0.0/0"},
  {"direction": "ingress", "ethertype": "IPv4", "protocol": "udp",  "port_range_min": 1, "port_range_max": 65535, "remote_ip_prefix": "0.0.0.0/0"},
  {"direction": "ingress", "ethertype": "IPv4", "protocol": "icmp", "remote_ip_prefix": "0.0.0.0/0"},
  {"direction": "ingress", "ethertype": "IPv6", "protocol": "tcp",  "port_range_min": 1, "port_range_max": 65535, "remote_ip_prefix": "::/0"},
  {"direction": "ingress", "ethertype": "IPv6", "protocol": "udp",  "port_range_min": 1, "port_range_max": 65535, "remote_ip_prefix": "::/0"},
  {"direction": "ingress", "ethertype": "IPv6", "protocol": "ipv6-icmp", "remote_ip_prefix": "::/0"},
  /* egress block — same split-around-25 pattern as tight variant */
]
```

**Ubuntu-image default SSH user** — verified to be `ubuntu`, not `root`. `root` login is refused even with the right password. `ubuntu` has passwordless sudo. The `password` field at create-time sets the password for the `ubuntu` user. For our bot's `sshpass` flow, set `SSH_USER=ubuntu` (or probe once after create to confirm the image hasn't changed). Our existing TimeWeb/Beget/Selectel code hardcodes `root@` — we'll need a per-provider override for Gcore.

**Inside-VM observations** (Ubuntu 24.04 from `0de74e2c-9e4e-4f1a-9625-0a05038fac67`):
- `ufw status` → `Status: inactive` (no OS-level firewall)
- `iptables -S INPUT` → default policy `ACCEPT` with no rules

So on Gcore, **the security group is the entire firewall** — no Ubuntu-level filter to fight with.

**Live test results** (VM `198.18.37.218`, wide-open SG attached, probed from an external box):

| Test | Result | Meaning |
|---|---|---|
| `curl :8080` (listener running inside VM) | HTTP 200 in 0.4 s | SG allows arbitrary TCP ingress |
| TCP connect :8080 via `/dev/tcp` | open | low-level confirmation |
| TCP connect :12345 (nothing listening) | **Connection refused** (not timeout) | SG passes the SYN; kernel rejects — proves SG isn't dropping |
| `ping` | 2/2, ~210 ms RTT | ICMP ingress works |
| Outbound TCP 443 from inside | ok | our egress rules allow it |
| Outbound TCP 25 from inside | **fail** (timeout) | Gcore platform block, independent of our SG |
| Outbound DNS UDP 53 | ok | egress UDP works |

The **`refused` vs `timeout`** distinction is the key diagnostic: a dropping firewall gives timeouts, a passing firewall gives `Connection refused` (because the kernel emits RST on a closed port). We saw refused, so the SG is transparent for open rules. For the tight SG we'll expect *timeouts* on any port not listed — run the same test after a real node is deployed to confirm.

**May 2026 re-verification on a fresh account, single SG attached at create-time + default detached via `delsecuritygroup`** (instance `198.18.38.165`, Tashkent, `g1-standard-1-2`, image `3414c5ad-…`, SG named `vpn-watchdog-open` exactly as in the create body above):

| Probe | Direction | Result | Diagnostic |
|---|---|---|---|
| TCP 8765 (live `nc -lk` listener inside) | ingress | open | listener visible from outside |
| TCP 12345 / 1024 / 65000 (no listener) | ingress | refused | SG passed SYN, kernel RST → SG transparent |
| TCP 22 (sshd) | ingress | open | normal |
| ICMP echo | ingress | 2/2 received, ~239 ms RTT | ICMP rule works |
| TCP 80 → example.com | egress | HTTP 200 | egress TCP works |
| TCP 443 → gcore.com | egress | HTTP 200 | egress TCP/TLS works |
| **TCP 25 → gmail-smtp-in.l.google.com** | egress | **timeout** | platform-blocked + our SG also excludes |
| UDP 53 → 8.8.8.8 | egress | resolves | egress UDP works |
| ICMP → 8.8.8.8 | egress | 2/2 received | egress ICMP works |

So the policy "all ports open ingress, all ports open egress except port 25" is enforceable end-to-end with the SG above, and survives instance recreation as long as the SG is attached at create time (or attached + default detached on a running instance — both routes verified).

## Design notes for `src/providers/gcore/`

- **`client.rs`** — `GcoreClient` holds `api_key`, `project_id`, `region_id`. No auth refresh, no Arc<Mutex> token cache.
- **`create_server`** — POST, poll create task (5s interval, 2 min max), read instance UUID from `created_resources.instances[0]`. Then poll `GET /instances/{id}` until `status == "ACTIVE"`. Return `(uuid, root_pass)` + capture `volumes[].id` → persist to DB (new `gcore_volume_id` column or a JSON field on `servers`, so we don't need to refetch on delete).
- **`delete_server`** — if we have the volume IDs from create-time, use them directly; otherwise `GET /instances/{id}` first to read them. Then `DELETE` with `delete_floatings=true&volumes=<id,…>`, poll delete task.
- **`list_regions`** — maps `{id, display_name, country}` into shared `Region{id: String(id), description: display_name+country}`.
- **Rate limit** — Gcore is pay-as-you-go. `RateLimitConfig { max_free_trials: u32::MAX, cooldown_hours: 0, tracking: ApiSignal }` — effectively disabled.
- **Direct-node flow** — after `create_server` → ACTIVE, the provisioner skips the dest-deploy + socat chain, SSH-installs Marzban node, and POSTs `/api/node` to the Marzban panel. Gate via a `CloudProvider::server_kind()` returning e.g. `ServerKind::DirectNode`.

## Pricing sanity check (re-verified May 2026)

For an 18:00–08:00 local window (14 h/day × 30 days = 420 h/month):
- Shared bundled `g2s-shared-1-1-25`: 420 × €0.000972 = **€0.41/month** (only if capacity available — Basic VM is usually capped on a fresh account).
- Standard `g1-standard-1-2`: 420 × €0.021216 = **€8.91/month** + volume.
- Volume at ~€0.05/GB-month × 10 GB × (14/24) = **€0.29/month** (if billed only while attached; the volume API doesn't expose its price — confirm on real bill).
- Public IP on `pub_net`: no separate listing on the test instance, bundled with the instance.

Plan on **€9–10/month/server** for the standard tier and the 14h window. For 24/7 it's ~€15.78/month (€15.28 flavor + ~€0.50 volume).

Note: there is no cost-prediction or pricing-preview API — `/cloud/v?/{cost-estimate, cost-prediction, preview-price, pricing}` all 404 in our probes. The `price_per_hour` / `price_per_month` fields on the flavor record are the only authoritative numbers the API will give you. Volume pricing has to be read from the dashboard pricing page or computed from a real bill.

## Gotchas (verified live)

1. **Shared-tier flavor caps** — `g2s-shared-*` tiers return 403 immediately in both Tashkent and Almaty. Fall back to `g1-standard-*` (or `g1-cpu-*` / `g1-memory-*` if you need a different CPU:RAM ratio) at runtime. Don't hardcode a flavor — list the API per region.
2. **`$` in API keys** breaks shell double-quote interpolation — use single quotes or env var assignment.
3. **v1 vs v2 paths mixed** — `POST /cloud/v2/instances/...` but `GET /cloud/v1/instances/...`. Don't assume uniformity.
4. **Public IP is `type: "fixed"` on `pub_net`**, not a floating IP. `delete_floatings=true` is harmless but not doing anything for this path.
5. **Volumes ARE returned in the create task** under `created_resources.volumes` — earlier note said otherwise. Capture at create-time, no second GET needed.
6. **Create is fast (~20–30s with SG)** — clean create went ACTIVE in ~10s; with SG attached it took ~23s (task created→finished). Still much faster than Selectel.
7. **Image UUIDs are per-region** — can't hardcode, must list+filter per region.
8. **Egress port 25 is permanently blocked** — any rule that spans it gets `400 ValidationError`. Split egress into `1–24` and `26–65535` ranges.
9. **Security group POST body is wrapped**: `{ "security_group": { name, description, security_group_rules: [...] } }` — easy to miss.
10. **Default SG can't be deleted**; list-SGs always shows at least `default` as `count: 1`.
11. **Ubuntu image default SSH user is `ubuntu`**, not `root`. Root login refused even with correct password. `password` field sets password for `ubuntu` user. Our existing providers hardcode `root@`; Gcore needs a per-provider SSH-user override.
12. **No OS firewall on Gcore Ubuntu images** — `ufw` inactive, `iptables` default ACCEPT. The SG is the entire firewall layer.
13. **Diagnostic tip** — dropping-firewall vs open-firewall can be told apart by `connection refused` (SG open, host rejects) vs `timeout` (SG drops). Use this when validating a production tight-SG deploy.
14. **SG attach/detach on a running instance uses non-obvious URLs** — `/instances/{id}/addsecuritygroup` and `/instances/{id}/delsecuritygroup`, body `{"name": "<sg-name>"}` (NOT `id`). Many obvious shapes (`/action`, `/security_groups`, `DELETE` on a sub-resource, etc.) all 404 / 405. Verified live; both return 204.
15. **`g0-*` flavor tier is gone** — the cheapest standard cloud-server flavor is now `g1-standard-1-2` (€0.021216/hr, €15.275/month). List the flavors API at runtime instead of hardcoding strings — Gcore retires tiers without warning.
16. **No cost-prediction API** — `/cloud/v?/{cost-estimate, cost-prediction, preview-price, pricing, billing/estimate}` all 404. Trust the flavor record's `price_per_hour` / `price_per_month`; volume pricing isn't returned by any endpoint.
17. **"Virtual Instance" / "Virtual Machine" / "Cloud Server" are all the same resource** — same endpoint (`/cloud/v2/instances/`), same task type (`create_vm`). Marketing names differ, API doesn't. The genuine product split is **Basic VM** (`g2s-shared-*` flavor, shared CPU, bundled disk) vs **Cloud Server** (`g1-*` flavor, dedicated CPU, external volume) — both via the same endpoint.
18. **Permanent API tokens use `_` separator, not `$`** — modern keys look like `30169_b350db82...` (no shell-escape gotcha). The legacy `$`-separated key advice in the Auth section above still applies for older keys, but new accounts get the underscore form.
