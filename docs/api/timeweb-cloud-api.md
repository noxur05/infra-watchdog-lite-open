# TimeWeb Cloud API Reference

## General
- **Base URL:** `https://api.timeweb.cloud`
- **Auth:** `Authorization: Bearer $TIMEWEB_CLOUD_TOKEN` (JWT)
- **Rate limit:** 20 req/sec per endpoint, returns 429 when exceeded
- **Responses:** JSON, top-level keys are entity name + `response_id` + optional `meta`
- **OpenAPI spec:** `/home/user/vpn-watchdog/openapi.json` (48K lines)

## Account
- **Login:** tg66001
- **SSH Key ID:** 340927 (ed25519, default)
- **Account status endpoint:** `GET /api/v1/account/status`
- No endpoint exists to query resource quotas/limits

## Locations & Availability Zones (live data)

| Location | Country | Zones |
|---|---|---|
| ru-1 | Russia | spb-1, spb-2, spb-3, spb-4, spb-5 |
| ru-2 | Russia | nsk-1 (Novosibirsk) |
| ru-3 | Russia | msk-1 (Moscow) |
| pl-1 | Poland | gdn-1 (Gdansk) |
| kz-1 | Kazakhstan | ala-1 (Almaty) |
| nl-1 | Netherlands | ams-1 (Amsterdam) |
| us-2 | USA | nyc-1 (New York) |
| de-1 | Germany | fra-1 (Frankfurt) |

- IPv6 only available for ru-1
- `GET /api/v2/locations` to fetch live list

## Server Lifecycle

### Create Server
`POST /api/v1/servers`

Required: `name` + one of (`preset_id` | `configuration`) + one of (`os_id` | `image_id`)

Optional: `ssh_keys_ids`, `project_id`, `availability_zone`, `cloud_init`, `bandwidth`, `comment`, `is_ddos_guard`, `network` (with `floating_ip: "create_ip"`)

Example request body:
```json
{
  "name": "my-server",
  "os_id": 109,
  "preset_id": 4795,
  "availability_zone": "msk-1",
  "ssh_keys_ids": [340927],
  "project_id": 1457497,
  "is_ddos_guard": false,
  "is_root_password_required": true,
  "network": { "floating_ip": "create_ip" }
}
```

Response (201): `{ "server": { vds object }, "warning": "...", "response_id": "..." }`

### Server Statuses
| Status | Meaning |
|---|---|
| `installing` | Being provisioned |
| `software_install` | Software installing |
| `configuring` | Being configured |
| `turning_on` | Booting |
| `on` | **Ready and running** |
| `off` | Powered off |
| `turning_off` | Shutting down |
| `hard_turning_off` | Force shutting down |
| `rebooting` | Rebooting |
| `hard_rebooting` | Force rebooting |
| `removing` | Being deleted |
| `removed` | Deleted |
| `cloning` | Being cloned |
| `transfer` | In transfer |
| `blocked` | Blocked |
| `permanent_blocked` | Permanently blocked |
| `no_paid` | Payment issue |

Typical flow: `installing` → `on` (~1 minute observed)

### Get Server
`GET /api/v1/servers/{server_id}`

Response includes: `id`, `status`, `os`, `cpu`, `ram`, `disks`, `networks` (with IPs), `root_pass`, `vnc_pass`, `availability_zone`, `location`, `created_at`

### List Servers
`GET /api/v1/servers` — returns `{ "servers": [], "meta": { "total": 0 } }`

### Delete Server
`DELETE /api/v1/servers/{server_id}`

- Returns 204 on success (direct delete if token has `is_able_to_delete: true`)
- If Telegram confirmation enabled: returns 423 with `hash`, then resend with `?hash=...&code=...`
- **All regular IPs are deleted with the server**

### Server Actions (all return 204, no body needed)
| Action | Endpoint |
|---|---|
| Start | `POST /api/v1/servers/{id}/start` |
| Shutdown | `POST /api/v1/servers/{id}/shutdown` |
| Force shutdown | `POST /api/v1/servers/{id}/hard-shutdown` |
| Reboot | `POST /api/v1/servers/{id}/reboot` |
| Force reboot | `POST /api/v1/servers/{id}/hard-reboot` |
| Reset password | `POST /api/v1/servers/{id}/reset-password` |
| Clone | `POST /api/v1/servers/{id}/clone` (returns 201 with new server) |
| Generic | `POST /api/v1/servers/{id}/action` body: `{"action": "start\|shutdown\|..."}` |

## VDS (Server) Object — Key Fields
```
id, name, comment, status, os.id, os.name, os.version,
preset_id, location, availability_zone, boot_mode,
cpu, cpu_frequency, ram, gpu, is_dedicated_cpu,
disks[].id, disks[].size, disks[].type, disks[].status,
networks[].type (public|local), networks[].bandwidth,
networks[].ips[].ip, networks[].ips[].type (ipv4|ipv6), networks[].ips[].is_main,
root_pass, vnc_pass, cloud_init, created_at, start_at,
is_ddos_guard, project_id, blocked_ports[]
```

## IP Addresses

### Regular Server IPs (tied to server, deleted with server)
- `GET /api/v1/servers/{id}/ips` — list
- `POST /api/v1/servers/{id}/ips` — add (`{"type": "ipv4|ipv6"}`)
- `DELETE /api/v1/servers/{id}/ips` — remove (`{"ip": "..."}`, cannot delete main IP)
- `PATCH /api/v1/servers/{id}/ips` — update PTR

### Floating IPs (independent, survive server deletion)
- `GET /api/v1/floating-ips` — list all
- `POST /api/v1/floating-ips` — create (`{"is_ddos_guard": bool, "availability_zone": "..."}`)
- `GET /api/v1/floating-ips/{id}` — get details
- `DELETE /api/v1/floating-ips/{id}` — delete
- `POST /api/v1/floating-ips/{id}/bind` — attach (`{"resource_type": "server", "resource_id": 123}`)
- `POST /api/v1/floating-ips/{id}/unbind` — detach

## IP Daily Limit Behavior
- **No endpoint to check IP quota**
- When floating IP daily limit is exceeded, server creation still succeeds (201)
- Response includes `"warning": "floating_ip_daily_limit_exceeded"`
- Server gets only IPv6, no IPv4 assigned
- Must wait for limit reset or use existing floating IP

## Other Useful Endpoints
- `GET /api/v1/presets/servers` — list server plans/pricing
- `GET /api/v1/os/servers` — list available OS images
- `GET /api/v1/ssh-keys` — list SSH keys
- `GET /api/v1/projects` — list projects
- `GET /api/v1/servers/{id}/statistics` — server metrics
- `GET /api/v1/servers/{id}/logs` — server logs
- `GET /api/v1/firewall/groups` — firewall rules

## Tested Values (from real API calls)
- OS ID 109 = Debian 13
- Preset ID 4795 = 1 CPU, 1GB RAM, 15GB NVMe
- Project ID 1457497 = existing project
- SSH Key ID 340927 = default ed25519 key
- Server provisioning time: ~1 minute (installing → on)
- Delete: immediate (204), no confirmation needed with current token
