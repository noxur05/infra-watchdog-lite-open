# Marzban Panel API Reference

## General
- **Panel URL:** `https://example-a.com`
- **API Base:** `https://example-a.com/api`
- **Version:** 0.8.4
- **Auth:** OAuth2 Password Bearer → JWT token

## Authentication

### Get Token
`POST /api/admin/token` (form-urlencoded)
```
grant_type=password&username=ADMIN&password=PASS
```
Response: `{ "access_token": "...", "token_type": "bearer" }`

Use in all requests: `Authorization: Bearer {access_token}`

---

## Host Management (KEY FOR BOT INTEGRATION)

### Get All Hosts
`GET /api/hosts`

Response: object keyed by inbound tag, each containing array of ProxyHost:
```json
{
  "VLESS TCP REALITY": [
    { "remark": "Germany_BWD", "address": "1.2.3.4", "port": 443, ... },
    { "remark": "Manual Host", "address": "5.6.7.8", "port": 443, ... }
  ]
}
```

### Update Hosts
`PUT /api/hosts`

**IMPORTANT:** This is a full replacement per inbound tag. Must send ALL hosts for each tag, not just the ones you're changing.

Body: same structure as GET response.

### ProxyHost Schema
```
{
  remark: string (required) — host name, e.g. "Germany_BWD"
  address: string (required) — server IP or domain
  port: integer | null — listening port
  sni: string | null — TLS Server Name Indication
  host: string | null — HTTP Host header
  path: string | null — WebSocket path
  security: "inbound_default" | "none" | "tls" (default: "inbound_default")
  alpn: "" | "h3" | "h2" | "http/1.1" | "h3,h2,http/1.1" | "h3,h2" | "h2,http/1.1"
  fingerprint: "" | "chrome" | "firefox" | "safari" | "ios" | "android" | "edge" | "360" | "qq" | "random" | "randomized"
  allowinsecure: boolean | null
  is_disabled: boolean | null
  mux_enable: boolean | null
  fragment_setting: string | null
  noise_setting: string | null
  random_user_agent: boolean | null
  use_sni_as_host: boolean | null
}
```

---

## Inbounds

### Get Inbounds
`GET /api/inbounds`

Response: object keyed by protocol:
```json
{
  "vless": [
    { "tag": "VLESS TCP REALITY", "protocol": "vless", "network": "tcp", "tls": "reality", "port": 443 }
  ],
  "vmess": [...],
  "trojan": [...],
  "shadowsocks": [...]
}
```

---

## Nodes

### List Nodes
`GET /api/nodes`

### Add Node
`POST /api/node`
```json
{
  "name": "DE node",
  "address": "1.2.3.4",
  "port": 62050,
  "api_port": 62051,
  "usage_coefficient": 1.0,
  "add_as_new_host": true
}
```

### Modify Node
`PUT /api/node/{node_id}`
```json
{
  "name": "...", "address": "...", "port": ..., "api_port": ...,
  "usage_coefficient": ..., "status": "connected|connecting|error|disabled"
}
```

### Delete Node
`DELETE /api/node/{node_id}`

### Reconnect Node
`POST /api/node/{node_id}/reconnect`

### Node Usage
`GET /api/nodes/usage?start=...&end=...`

---

## Users

### Create User
`POST /api/user`
```json
{
  "username": "user1",
  "status": "active",
  "proxies": {},
  "expire": null,
  "data_limit": 0,
  "data_limit_reset_strategy": "no_reset",
  "inbounds": {},
  "note": null
}
```

### Get User
`GET /api/user/{username}`

### Modify User
`PUT /api/user/{username}`

### Delete User
`DELETE /api/user/{username}`

### List Users
`GET /api/users?offset=0&limit=50&search=...&status=active`

### Reset User Traffic
`POST /api/user/{username}/reset`

### Revoke Subscription
`POST /api/user/{username}/revoke_sub`

### User Status Values
- `active` — can connect
- `disabled` — admin disabled
- `limited` — hit data limit
- `expired` — subscription expired
- `on_hold` — temporarily suspended

---

## Admin

### Get Current Admin
`GET /api/admin`

### Create Admin
`POST /api/admin` — body: `{ username, password, is_sudo, telegram_id?, discord_webhook? }`

### Modify Admin
`PUT /api/admin/{username}`

### Delete Admin
`DELETE /api/admin/{username}`

### List Admins
`GET /api/admins?offset=0&limit=50`

---

## System

### System Stats
`GET /api/system`
```json
{
  "version": "...",
  "mem_total": 0, "mem_used": 0,
  "cpu_cores": 0, "cpu_usage": 0.0,
  "total_user": 0, "online_users": 0,
  "users_active": 0, "users_disabled": 0,
  "incoming_bandwidth": 0, "outgoing_bandwidth": 0
}
```

### Core Stats
`GET /api/core`
```json
{ "version": "...", "started": true, "logs_websocket": "..." }
```

### Restart Core
`POST /api/core/restart`

### Core Config
`GET /api/core/config` / `PUT /api/core/config`

---

## Subscriptions (Public, No Auth)

- `GET /sub/{token}/` — subscription link (auto-detect client)
- `GET /sub/{token}/info` — subscription details
- `GET /sub/{token}/usage?start=...&end=...` — usage stats
- `GET /sub/{token}/{client_type}` — specific client format (sing-box|clash-meta|clash|outline|v2ray|v2ray-json)

---

## Bot Integration Notes

### Host naming convention
- Bot-managed hosts: `{country_name}_{BOT_CODE}` (e.g., `Germany_BWD`)
- Bot only touches hosts matching `_{BOT_CODE}` suffix
- Manual hosts are never modified

### Host update workflow
1. `GET /api/hosts` — fetch all current hosts
2. Find the target inbound tag
3. Modify only bot-owned hosts (matching code suffix)
4. Preserve all other hosts untouched
5. `PUT /api/hosts` — send full updated structure back

### On server replacement
1. Get all hosts, find bot-owned ones
2. Update `address` to new server IP for each
3. PUT the full host list back

### On destination add
1. Resolve destination IP → country name
2. Get all hosts, add new ProxyHost with `remark: "{country}_{code}"`, `address: server_ip`, `port: local_port`
3. PUT full host list

### On destination delete
- Do nothing until new server (then simply don't re-add it)
