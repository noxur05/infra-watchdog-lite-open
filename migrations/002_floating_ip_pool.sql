-- Warm-pool floating IPs for the Selectel single-server OpenVPN front.
-- The one active server keeps a small pool of floating IPs (1 active + N spares),
-- each bound to its own fixed IP on the server's single Neutron port. Each IP is
-- curl-tested individually; the active one is what OVPN-AS host.name points at.
CREATE TABLE IF NOT EXISTS server_floating_ips (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    server_id INTEGER NOT NULL,
    fixed_ip TEXT NOT NULL,            -- the server-side fixed IP this FIP maps to (curl --interface source)
    floating_ip TEXT NOT NULL,         -- the public floating IP clients connect to
    fip_id TEXT NOT NULL,              -- Neutron floatingip id (for release)
    role TEXT NOT NULL DEFAULT 'spare',     -- 'active' | 'spare'
    status TEXT NOT NULL DEFAULT 'untested',-- 'good' | 'blocked' | 'untested'
    consecutive_fails INTEGER NOT NULL DEFAULT 0,
    tested_at TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS idx_sfip_server ON server_floating_ips(server_id);
