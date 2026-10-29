use std::sync::Arc;
use std::time::{Instant, Duration};
use tokio::sync::Mutex;
use reqwest::Client;
use serde_json::Value;
use rand::Rng;
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};

use crate::config::get_config;
use crate::provider::*;
use crate::utils::build_http_client;

const KEYSTONE_URL: &str = "https://cloud.api.selcloud.ru/identity/v3/auth/tokens";
/// Token valid for 24h, but refresh 1h early to be safe.
const TOKEN_REFRESH_SECS: u64 = 23 * 3600;
/// Boot-volume size (GB) when creating a volume-backed server (zones with no
/// local-disk quota, e.g. ru-2a). Must be ≥ the Ubuntu image `min_disk` (5 GB);
/// 10 GB leaves headroom for apt/logs on a disposable front.
const BOOT_VOLUME_GB: u64 = 10;

/// Selectel OpenStack client. Handles Keystone auth, Nova, Neutron, Glance.
pub struct SelectelClient {
    client: Client,
    username: String,
    password: String,
    account_id: String,
    project_id: String,
    region: String,
    token: Arc<Mutex<Option<(String, Instant)>>>,
}

impl SelectelClient {
    pub fn new(
        username: String,
        password: String,
        account_id: String,
        project_id: String,
        region: String,
    ) -> Self {
        Self {
            client: build_http_client(),
            username,
            password,
            account_id,
            project_id,
            region,
            token: Arc::new(Mutex::new(None)),
        }
    }

    /// Strip the trailing zone letter from an availability zone to get the
    /// parent region. e.g. "ru-1a" -> "ru-1", "uz-2a" -> "uz-2".
    /// If the value already looks like a region (ends in a digit), returns it as-is.
    fn region_part(&self) -> &str {
        let r = self.region.as_str();
        match r.chars().last() {
            Some(c) if c.is_ascii_alphabetic() => &r[..r.len() - 1],
            _ => r,
        }
    }

    /// Base URL for region-specific APIs (Nova, Neutron, Glance).
    /// Built from the parent region (everything except the trailing zone letter).
    fn base_url(&self) -> String {
        let region = self.region_part();
        if region.starts_with("uz-")
            || region.starts_with("kz-")
            || region.starts_with("ke-")
        {
            format!("https://{}.cloud.api.servercore.com", region)
        } else {
            format!("https://{}.cloud.api.selcloud.ru", region)
        }
    }

    fn compute_url(&self, path: &str) -> String {
        format!("{}/compute/v2.1{}", self.base_url(), path)
    }

    fn network_url(&self, path: &str) -> String {
        format!("{}/network/v2.0{}", self.base_url(), path)
    }

    fn image_url(&self, path: &str) -> String {
        format!("{}/image/v2{}", self.base_url(), path)
    }

    /// Cinder (block storage) v3 base for this region — used to look up the
    /// zone-pinned volume type when creating a volume-backed server.
    fn volume_url(&self, path: &str) -> String {
        format!("{}/volume/v3/{}{}", self.base_url(), self.project_id, path)
    }

    /// Authenticate with Keystone v3 and get a project-scoped token.
    async fn authenticate(&self) -> Result<String, ProviderError> {
        let body = serde_json::json!({
            "auth": {
                "identity": {
                    "methods": ["password"],
                    "password": {
                        "user": {
                            "name": self.username,
                            "domain": {"name": self.account_id},
                            "password": self.password
                        }
                    }
                },
                "scope": {
                    "project": {
                        "id": self.project_id
                    }
                }
            }
        });

        let resp = self.client
            .post(KEYSTONE_URL)
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::Network(format!("keystone auth: {}", e)))?;

        let status = resp.status();
        if status.as_u16() == 401 || status.as_u16() == 403 {
            let text = resp.text().await.unwrap_or_default();
            return Err(ProviderError::Auth(format!("keystone {}: {}", status, text)));
        }
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(ProviderError::Api(format!("keystone {}: {}", status, text)));
        }

        // Token is in X-Subject-Token header
        let token = resp.headers()
            .get("x-subject-token")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
            .ok_or_else(|| ProviderError::Auth("no X-Subject-Token in response".to_string()))?;

        Ok(token)
    }

    /// Get a valid token, re-authenticating if expired.
    async fn get_token(&self) -> Result<String, ProviderError> {
        let mut guard = self.token.lock().await;
        if let Some((tok, created)) = guard.as_ref() {
            if created.elapsed() < Duration::from_secs(TOKEN_REFRESH_SECS) {
                return Ok(tok.clone());
            }
        }
        let tok = self.authenticate().await?;
        *guard = Some((tok.clone(), Instant::now()));
        Ok(tok)
    }

    /// Invalidate cached token (on 401).
    async fn invalidate_token(&self) {
        let mut guard = self.token.lock().await;
        *guard = None;
    }

    /// Make an authenticated API request. Retries once on 401.
    async fn api_request(
        &self,
        method: reqwest::Method,
        url: &str,
        body: Option<&Value>,
    ) -> Result<(reqwest::StatusCode, String), ProviderError> {
        self.api_request_h(method, url, body, &[]).await
    }

    /// Like `api_request` but with extra request headers — used to opt into a
    /// Nova microversion (e.g. `OpenStack-API-Version: compute 2.67` so a
    /// `volume_type` may be set in `block_device_mapping_v2`).
    async fn api_request_h(
        &self,
        method: reqwest::Method,
        url: &str,
        body: Option<&Value>,
        headers: &[(&str, &str)],
    ) -> Result<(reqwest::StatusCode, String), ProviderError> {
        let token = self.get_token().await?;

        let mut req = self.client.request(method.clone(), url)
            .header("X-Auth-Token", &token);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        if let Some(json) = body {
            req = req.json(json);
        }

        let resp = req.send().await
            .map_err(|e| ProviderError::Network(format!("request to {}: {}", url, e)))?;

        let status = resp.status();

        if status.as_u16() == 401 {
            self.invalidate_token().await;
            let new_token = self.get_token().await?;

            let mut retry = self.client.request(method, url)
                .header("X-Auth-Token", &new_token);
            for (k, v) in headers {
                retry = retry.header(*k, *v);
            }
            if let Some(json) = body {
                retry = retry.json(json);
            }

            let resp = retry.send().await
                .map_err(|e| ProviderError::Network(format!("retry request: {}", e)))?;
            let retry_status = resp.status();
            let text = resp.text().await.unwrap_or_default();

            if retry_status.as_u16() == 401 || retry_status.as_u16() == 403 {
                return Err(ProviderError::Auth(format!("{}: {}", retry_status, text)));
            }

            return Ok((retry_status, text));
        }

        let text = resp.text().await.unwrap_or_default();
        Ok((status, text))
    }

    /// Make a request and parse JSON response.
    async fn api_json(
        &self,
        method: reqwest::Method,
        url: &str,
        body: Option<&Value>,
    ) -> Result<Value, ProviderError> {
        let (status, text) = self.api_request(method, url, body).await?;
        if !status.is_success() {
            return Err(ProviderError::Api(format!("{}: {}", status, text)));
        }
        serde_json::from_str(&text)
            .map_err(|e| ProviderError::Api(format!("parse response: {}", e)))
    }

    /// Find the external network in this region.
    async fn find_external_network(&self) -> Result<String, ProviderError> {
        let json = self.api_json(
            reqwest::Method::GET,
            &self.network_url("/networks?router:external=true"),
            None,
        ).await?;

        json["networks"].as_array()
            .and_then(|nets| nets.first())
            .and_then(|n| n["id"].as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| ProviderError::Api("no external network found".to_string()))
    }

    /// Find or create a private network + subnet + router for this region.
    /// Returns the network UUID.
    ///
    /// Selection rules (in order):
    ///   1. Network named exactly "vpn-net" with at least one subnet — that's
    ///      one the bot itself created previously. Reuse it.
    ///   2. Otherwise create a fresh "vpn-net" + subnet + router.
    ///
    /// We deliberately do NOT reuse arbitrary user-created private networks
    /// even if they have subnets — they may be missing an external-gateway
    /// router or pointing at a broken router (real failure mode seen in
    /// production: two "net" networks with one router that has no external
    /// gateway, causing every floating-IP association to fail with
    /// `ExternalGatewayForFloatingIPNotFound`). Owning our own "vpn-net"
    /// keeps the bot's setup independent from manual cleanup state.
    async fn ensure_network(&self) -> Result<String, ProviderError> {
        let json = self.api_json(
            reqwest::Method::GET,
            &self.network_url("/networks"),
            None,
        ).await?;

        let networks = json["networks"].as_array()
            .ok_or_else(|| ProviderError::Api("no networks array".to_string()))?;

        // Pass 1: reuse "vpn-net" if the bot created one before in this region.
        for net in networks {
            let name = net["name"].as_str().unwrap_or("");
            if name != "vpn-net" {
                continue;
            }
            let external = net["router:external"].as_bool().unwrap_or(false);
            if external {
                continue;
            }
            let has_subnet = net["subnets"].as_array().map(|s| !s.is_empty()).unwrap_or(false);
            if !has_subnet {
                continue;
            }
            let net_id = net["id"].as_str().unwrap_or("").to_string();
            log::info!("Selectel: reusing vpn-net ({}) in {}", net_id, self.region);
            // Heal the router if needed (set external gateway / admin_state_up)
            self.ensure_router(&net_id).await?;
            return Ok(net_id);
        }

        // No existing vpn-net: create one fresh.
        log::info!("Selectel: creating vpn-net in {}", self.region);
        let ext_net = self.find_external_network().await?;

        // Create network
        let net_json = self.api_json(
            reqwest::Method::POST,
            &self.network_url("/networks"),
            Some(&serde_json::json!({
                "network": { "name": "vpn-net" }
            })),
        ).await?;
        let net_id = net_json["network"]["id"].as_str()
            .ok_or_else(|| ProviderError::Api("no network id in create response".to_string()))?
            .to_string();

        // Create subnet
        self.api_json(
            reqwest::Method::POST,
            &self.network_url("/subnets"),
            Some(&serde_json::json!({
                "subnet": {
                    "network_id": net_id,
                    "name": "vpn-subnet",
                    "cidr": "192.168.100.0/24",
                    "ip_version": 4,
                    "dns_nameservers": ["8.8.8.8", "8.8.4.4"],
                    "enable_dhcp": true
                }
            })),
        ).await?;

        // Create router with external gateway
        let router_json = self.api_json(
            reqwest::Method::POST,
            &self.network_url("/routers"),
            Some(&serde_json::json!({
                "router": {
                    "name": "vpn-router",
                    "admin_state_up": true,
                    "external_gateway_info": {
                        "network_id": ext_net
                    }
                }
            })),
        ).await?;
        let router_id = router_json["router"]["id"].as_str().unwrap_or("");

        // Attach subnet to router
        // Query subnet ID since it might not be in the network response yet
        let subnets_json = self.api_json(
            reqwest::Method::GET,
            &self.network_url(&format!("/subnets?network_id={}", net_id)),
            None,
        ).await?;
        let subnet_id = subnets_json["subnets"].as_array()
            .and_then(|s| s.first())
            .and_then(|s| s["id"].as_str())
            .ok_or_else(|| ProviderError::Api("no subnet found after creation".to_string()))?;

        let _ = self.api_request(
            reqwest::Method::PUT,
            &self.network_url(&format!("/routers/{}/add_router_interface", router_id)),
            Some(&serde_json::json!({ "subnet_id": subnet_id })),
        ).await;

        log::info!("Selectel: created network {} + subnet + router in {}", net_id, self.region);
        Ok(net_id)
    }

    /// Ensure at least one router with external gateway exists for a network.
    async fn ensure_router(&self, net_id: &str) -> Result<(), ProviderError> {
        let json = self.api_json(
            reqwest::Method::GET,
            &self.network_url("/routers"),
            None,
        ).await?;

        let routers = json["routers"].as_array()
            .ok_or_else(|| ProviderError::Api("no routers array".to_string()))?;

        for router in routers {
            let admin_up = router["admin_state_up"].as_bool().unwrap_or(false);
            let has_ext = router["external_gateway_info"].is_object()
                && !router["external_gateway_info"].is_null();

            if admin_up && has_ext {
                return Ok(()); // Router already good
            }

            // Fix the router: enable + add external gateway
            let router_id = router["id"].as_str().unwrap_or("");
            if router_id.is_empty() { continue; }

            let ext_net = self.find_external_network().await?;
            let _ = self.api_request(
                reqwest::Method::PUT,
                &self.network_url(&format!("/routers/{}", router_id)),
                Some(&serde_json::json!({
                    "router": {
                        "admin_state_up": true,
                        "external_gateway_info": { "network_id": ext_net }
                    }
                })),
            ).await;

            log::info!("Selectel: fixed router {} (admin_up + ext gateway)", router_id);
            return Ok(());
        }

        // No router at all — create one
        let ext_net = self.find_external_network().await?;
        let router_json = self.api_json(
            reqwest::Method::POST,
            &self.network_url("/routers"),
            Some(&serde_json::json!({
                "router": {
                    "name": "vpn-router",
                    "admin_state_up": true,
                    "external_gateway_info": { "network_id": ext_net }
                }
            })),
        ).await?;
        let router_id = router_json["router"]["id"].as_str().unwrap_or("");

        // Attach the network's subnet
        let subnets_json = self.api_json(
            reqwest::Method::GET,
            &self.network_url(&format!("/subnets?network_id={}", net_id)),
            None,
        ).await?;
        if let Some(subnet_id) = subnets_json["subnets"].as_array()
            .and_then(|s| s.first())
            .and_then(|s| s["id"].as_str())
        {
            let _ = self.api_request(
                reqwest::Method::PUT,
                &self.network_url(&format!("/routers/{}/add_router_interface", router_id)),
                Some(&serde_json::json!({ "subnet_id": subnet_id })),
            ).await;
        }

        Ok(())
    }

    /// Find candidate 1 vCPU / 1024 MB flavors for both boot modes.
    ///
    /// Returns `(local_disk, volume_boot)` flavor ids (either may be `None`).
    /// `create_server` tries the **local-disk** flavor first and only falls back
    /// to the **diskless** flavor + boot volume when local disk hits the
    /// `local_gb` quota. Neither preference works universally: some zones (ru-2a)
    /// have 0 local-disk quota and *need* volume-boot, while others (ru-3a) can't
    /// schedule the diskless line at all ("No valid host") and *need* local disk.
    /// So we surface both and decide by trial.
    async fn find_flavor(&self) -> Result<(Option<String>, Option<String>), ProviderError> {
        let json = self.api_json(
            reqwest::Method::GET,
            &self.compute_url("/flavors/detail?limit=200"),
            None,
        ).await?;

        let flavors = json["flavors"].as_array()
            .ok_or_else(|| ProviderError::Api("no flavors array".to_string()))?;

        let mut volume_bl: Option<String> = None;   // Basic Line diskless (preferred diskless)
        let mut volume_any: Option<String> = None;   // any diskless flavor
        let mut local_best: Option<(String, i64)> = None; // cheapest local-disk (smallest disk)
        for f in flavors {
            let name = f["name"].as_str().unwrap_or("");
            let vcpus = f["vcpus"].as_i64().unwrap_or(0);
            let ram = f["ram"].as_i64().unwrap_or(0);
            let disk = f["disk"].as_i64().unwrap_or(0);
            if vcpus != 1 || ram != 1024 { continue; }
            let id = f["id"].as_str().unwrap_or("").to_string();
            if id.is_empty() { continue; }

            if disk == 0 {
                if name.starts_with("BL") && volume_bl.is_none() { volume_bl = Some(id.clone()); }
                if volume_any.is_none() { volume_any = Some(id); }
            } else if name.starts_with("SL") && (local_best.is_none() || disk < local_best.as_ref().unwrap().1) {
                // Local-disk: restrict to the Standard Line (SL). Other lines' local
                // -disk flavors (e.g. BL*-8) are rejected as "availability zone not
                // suitable for the requested instance type" in some zones (ru-3a).
                local_best = Some((id, disk));
            }
        }

        let local = local_best.map(|(id, _)| id);
        let volume = volume_bl.or(volume_any);
        if local.is_none() && volume.is_none() {
            return Err(ProviderError::Api(format!("no 1vcpu/1024mb flavor in {}", self.region)));
        }
        Ok((local, volume))
    }

    /// Find a boot-volume type pinned to the current availability zone.
    ///
    /// Selectel volume types are named `<class>.<zone>` (e.g. `universal.ru-2a`).
    /// A volume must live in the same cluster as its server, so we match the AZ
    /// suffix and prefer SSD (`universal`) → cheap (`basic`) → fast (`fast`).
    /// Returns `None` if nothing matches (caller then lets Cinder pick a default).
    async fn find_boot_volume_type(&self) -> Result<Option<String>, ProviderError> {
        let json = self.api_json(
            reqwest::Method::GET,
            &self.volume_url("/types"),
            None,
        ).await?;

        let names: Vec<String> = json["volume_types"].as_array()
            .map(|ts| ts.iter().filter_map(|t| t["name"].as_str().map(String::from)).collect())
            .unwrap_or_default();

        let suffix = format!(".{}", self.region);
        for pref in ["universal", "basic", "fast"] {
            let want = format!("{}{}", pref, suffix);
            if names.iter().any(|n| n == &want) {
                return Ok(Some(want));
            }
        }
        // Any non-ISO type pinned to this zone.
        Ok(names.into_iter().find(|n| n.ends_with(&suffix) && !n.starts_with("iso")))
    }

    /// POST a single server-create. `boot_volume` selects the boot mode:
    /// `None` = boot off the flavor's local disk (`imageRef` = image); `Some(t)`
    /// = boot off a fresh network volume created from the image via a BDM (`t` is
    /// the zone-pinned volume type, or `None` to let Cinder default). Returns the
    /// new server id.
    async fn create_server_call(
        &self,
        flavor_id: &str,
        image_id: &str,
        zone: &str,
        net_id: &str,
        userdata_b64: &str,
        server_name: &str,
        boot_volume: Option<Option<String>>,
    ) -> Result<String, ProviderError> {
        let mut server = serde_json::json!({
            "name": server_name,
            "flavorRef": flavor_id,
            "availability_zone": zone,
            "networks": [{"uuid": net_id}],
            "security_groups": [{"name": "default"}],
            "user_data": userdata_b64
        });

        // Nova microversion ≥2.67 is required to accept `volume_type` in a BDM.
        let mut nova_headers: Vec<(&str, &str)> = Vec::new();
        match &boot_volume {
            Some(vol_type) => {
                // `imageRef` MUST be empty when the boot source is a block device.
                let mut bdm = serde_json::json!({
                    "boot_index": 0,
                    "uuid": image_id,
                    "source_type": "image",
                    "destination_type": "volume",
                    "volume_size": BOOT_VOLUME_GB,
                    "delete_on_termination": true
                });
                if let Some(t) = vol_type {
                    bdm["volume_type"] = serde_json::json!(t);
                    nova_headers.push(("OpenStack-API-Version", "compute 2.67"));
                    nova_headers.push(("X-OpenStack-Nova-API-Version", "2.67"));
                }
                server["imageRef"] = serde_json::json!("");
                server["block_device_mapping_v2"] = serde_json::json!([bdm]);
                log::info!(
                    "Selectel: creating server in {} (flavor={}, boot-from-volume {}GB type={}, image={})",
                    zone, flavor_id, BOOT_VOLUME_GB, vol_type.as_deref().unwrap_or("<default>"), image_id
                );
            }
            None => {
                server["imageRef"] = serde_json::json!(image_id);
                log::info!("Selectel: creating server in {} (flavor={}, local-disk image={})", zone, flavor_id, image_id);
            }
        }

        let body = serde_json::json!({ "server": server });
        let (status, text) = self.api_request_h(
            reqwest::Method::POST,
            &self.compute_url("/servers"),
            Some(&body),
            &nova_headers,
        ).await?;
        if !status.is_success() {
            return Err(ProviderError::Api(format!("create server {}: {}", status, text)));
        }
        let json: Value = serde_json::from_str(&text)
            .map_err(|e| ProviderError::Api(format!("parse create response: {}", e)))?;
        let server_id = json["server"]["id"].as_str()
            .ok_or_else(|| ProviderError::Api("no server id in create response".to_string()))?
            .to_string();

        log::info!("Selectel: server {} created, waiting for ACTIVE...", server_id);
        Ok(server_id)
    }

    /// Find the Ubuntu 24.04 image in this region.
    async fn find_image(&self) -> Result<String, ProviderError> {
        let json = self.api_json(
            reqwest::Method::GET,
            &self.image_url("/images?limit=100&visibility=public"),
            None,
        ).await?;

        let images = json["images"].as_array()
            .ok_or_else(|| ProviderError::Api("no images array".to_string()))?;

        for img in images {
            let name = img["name"].as_str().unwrap_or("");
            if name == "Ubuntu 24.04 LTS 64-bit" {
                if let Some(id) = img["id"].as_str() {
                    return Ok(id.to_string());
                }
            }
        }

        Err(ProviderError::Api(format!("Ubuntu 24.04 LTS image not found in {}", self.region)))
    }

    /// Release (dissociate + delete) a floating IP.
    pub async fn release_floating_ip(&self, fip_id: &str) -> Result<(), ProviderError> {
        // Dissociate first
        let _ = self.api_request(
            reqwest::Method::PUT,
            &self.network_url(&format!("/floatingips/{}", fip_id)),
            Some(&serde_json::json!({
                "floatingip": { "port_id": null }
            })),
        ).await;

        // Delete
        let (status, text) = self.api_request(
            reqwest::Method::DELETE,
            &self.network_url(&format!("/floatingips/{}", fip_id)),
            None,
        ).await?;

        if !status.is_success() && status.as_u16() != 404 {
            return Err(ProviderError::Api(format!("delete floating ip: {}: {}", status, text)));
        }

        log::info!("Selectel: released floating IP {}", fip_id);
        Ok(())
    }

    /// Generate a random password for cloud-init.
    fn generate_password() -> String {
        let mut rng = rand::thread_rng();
        let upper = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ";
        let lower = b"abcdefghijklmnopqrstuvwxyz";
        let digits = b"0123456789";

        let mut password = Vec::with_capacity(16);
        password.push(upper[rng.gen_range(0..upper.len())]);
        password.push(lower[rng.gen_range(0..lower.len())]);
        password.push(digits[rng.gen_range(0..digits.len())]);

        let alnum = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
        for _ in 3..16 {
            password.push(alnum[rng.gen_range(0..alnum.len())]);
        }

        for i in (1..password.len()).rev() {
            let j = rng.gen_range(0..=i);
            password.swap(i, j);
        }

        String::from_utf8(password).unwrap_or_else(|_| "Passw0rdXyZ12345".to_string())
    }

    // ── Warm-pool floating IPs ──
    //
    // The single OpenVPN front keeps POOL_SIZE floating IPs on ONE Neutron port:
    // the port carries N fixed IPs (one per pool member) and each fixed IP gets
    // its own floating IP via the floatingip `fixed_ip_address` binding. All
    // fixed IPs live on the one NIC, so the blanket DNAT covers every pool IP and
    // failover is just a `host.name` repoint — no iptables change.
    //
    // This whole mechanism is UNVALIDATED against live Selectel/OpenStack
    // (floating-IP quota ≥ N, multi-fixed-IP ports, per-fixed-IP FIP binding).
    // It is deliberately isolated in these methods so the live spike can poke it
    // in one place. Assumptions, if any prove wrong:
    //   * a port accepts ≥ N fixed IPs via PUT /ports/{id} {fixed_ips:[...]}
    //   * the account's floating-IP quota allows ≥ N simultaneous FIPs
    //   * a floatingip can be pinned to a chosen fixed IP via `fixed_ip_address`

    /// Resolve the server's single Neutron port id.
    pub async fn get_port_id(&self, server_id: &str) -> Result<String, ProviderError> {
        let ports_json = self.api_json(
            reqwest::Method::GET,
            &self.network_url(&format!("/ports?device_id={}", server_id)),
            None,
        ).await?;
        ports_json["ports"].as_array()
            .and_then(|p| p.first())
            .and_then(|p| p["id"].as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| ProviderError::Api("no port found for server".to_string()))
    }

    /// Ensure the server's port carries at least `n` fixed IPs on its subnet.
    /// Returns (port_id, fixed_ips) where `fixed_ips.len() >= n` (stable order,
    /// existing IPs first). Adds subnet-only entries so Neutron auto-allocates
    /// any missing addresses.
    pub async fn ensure_port_fixed_ips(
        &self, server_id: &str, n: usize,
    ) -> Result<(String, Vec<String>), ProviderError> {
        let port_id = self.get_port_id(server_id).await?;

        let port_json = self.api_json(
            reqwest::Method::GET,
            &self.network_url(&format!("/ports/{}", port_id)),
            None,
        ).await?;

        let read_fixed = |v: &Value| -> Vec<(String, String)> {
            v["port"]["fixed_ips"].as_array().map(|arr| {
                arr.iter().filter_map(|f| {
                    let ip = f["ip_address"].as_str()?.to_string();
                    let subnet = f["subnet_id"].as_str().unwrap_or("").to_string();
                    Some((ip, subnet))
                }).collect()
            }).unwrap_or_default()
        };

        let current = read_fixed(&port_json);
        if current.len() >= n {
            return Ok((port_id, current.into_iter().map(|(ip, _)| ip).collect()));
        }

        let subnet_id = current.first().map(|(_, s)| s.clone())
            .ok_or_else(|| ProviderError::Api("port has no fixed_ips/subnet".to_string()))?;

        // Desired list: keep every existing IP, then `n - len` subnet-only
        // entries for Neutron to auto-allocate.
        let mut desired: Vec<Value> = current.iter()
            .map(|(ip, _)| serde_json::json!({"subnet_id": subnet_id, "ip_address": ip}))
            .collect();
        for _ in current.len()..n {
            desired.push(serde_json::json!({"subnet_id": subnet_id}));
        }

        let updated = self.api_json(
            reqwest::Method::PUT,
            &self.network_url(&format!("/ports/{}", port_id)),
            Some(&serde_json::json!({"port": {"fixed_ips": desired}})),
        ).await?;

        let fixed: Vec<String> = read_fixed(&updated).into_iter().map(|(ip, _)| ip).collect();
        if fixed.len() < n {
            return Err(ProviderError::Api(format!(
                "port {} only has {} fixed IPs after PUT (wanted {})", port_id, fixed.len(), n
            )));
        }
        log::info!("Selectel: port {} now carries {} fixed IPs", port_id, fixed.len());
        Ok((port_id, fixed))
    }

    /// Allocate a fresh floating IP bound to a SPECIFIC fixed IP on `port_id`.
    /// Returns (floating_ip_address, fip_id).
    pub async fn allocate_fip_for_fixed(
        &self, port_id: &str, fixed_ip: &str,
    ) -> Result<(String, String), ProviderError> {
        let ext_net = self.find_external_network().await?;
        let fip_json = self.api_json(
            reqwest::Method::POST,
            &self.network_url("/floatingips"),
            Some(&serde_json::json!({
                "floatingip": {
                    "floating_network_id": ext_net,
                    "port_id": port_id,
                    "fixed_ip_address": fixed_ip
                }
            })),
        ).await?;
        let fip_id = fip_json["floatingip"]["id"].as_str()
            .ok_or_else(|| ProviderError::Api("no floating ip id".to_string()))?
            .to_string();
        let fip_addr = fip_json["floatingip"]["floating_ip_address"].as_str()
            .ok_or_else(|| ProviderError::Api("no floating ip address".to_string()))?
            .to_string();
        log::info!("Selectel: floating IP {} bound to fixed {} (port {})", fip_addr, fixed_ip, port_id);
        Ok((fip_addr, fip_id))
    }

    /// List floating IPs currently associated to `port_id` as
    /// (fixed_ip, floating_ip_address, fip_id).
    pub async fn list_port_floating_ips(
        &self, port_id: &str,
    ) -> Result<Vec<(String, String, String)>, ProviderError> {
        let json = self.api_json(
            reqwest::Method::GET,
            &self.network_url(&format!("/floatingips?port_id={}", port_id)),
            None,
        ).await?;
        let mut out = Vec::new();
        if let Some(arr) = json["floatingips"].as_array() {
            for f in arr {
                let fixed = f["fixed_ip_address"].as_str().unwrap_or("").to_string();
                let addr = f["floating_ip_address"].as_str().unwrap_or("").to_string();
                let id = f["id"].as_str().unwrap_or("").to_string();
                if !addr.is_empty() && !id.is_empty() {
                    out.push((fixed, addr, id));
                }
            }
        }
        Ok(out)
    }

    /// Build (or complete) the warm pool: ensure `n` fixed IPs on the port, and
    /// a floating IP bound to each. Idempotent — reuses FIPs already on the port
    /// (e.g. one bound earlier), only allocating the missing ones. Returns
    /// Vec<(fixed_ip, floating_ip, fip_id)> of length `n`.
    pub async fn build_pool(
        &self, server_id: &str, n: usize,
    ) -> Result<Vec<(String, String, String)>, ProviderError> {
        let (port_id, fixed_ips) = self.ensure_port_fixed_ips(server_id, n).await?;
        let existing = self.list_port_floating_ips(&port_id).await?;
        let mut out = Vec::with_capacity(n);
        for fixed in fixed_ips.into_iter().take(n) {
            if let Some((_, addr, id)) = existing.iter().find(|(fx, _, _)| fx == &fixed) {
                out.push((fixed, addr.clone(), id.clone()));
            } else {
                let (addr, id) = self.allocate_fip_for_fixed(&port_id, &fixed).await?;
                out.push((fixed, addr, id));
            }
        }
        Ok(out)
    }
}

#[async_trait::async_trait]
impl CloudProvider for SelectelClient {
    async fn create_server(&self, _location: &str) -> Result<CreateServerResult, ProviderError> {
        // 1. Ensure network infrastructure
        let net_id = self.ensure_network().await?;

        // 2. Find flavor candidates and image.
        let (local_flavor, volume_flavor) = self.find_flavor().await?;
        let image_id = self.find_image().await?;
        // self.region now stores the availability zone directly (e.g. "ru-1a").
        let zone = self.region.clone();
        let password = Self::generate_password();
        let config = get_config();

        // 3. Build cloud-init userdata
        let userdata = format!(
            concat!(
                "#cloud-config\n",
                "ssh_pwauth: true\n",
                "disable_root: false\n",
                "chpasswd:\n",
                "  expire: false\n",
                "  users:\n",
                "    - name: root\n",
                "      password: {}\n",
                "      type: text\n",
                "runcmd:\n",
                "  - sed -i 's/^#\\?PermitRootLogin.*/PermitRootLogin yes/' /etc/ssh/sshd_config\n",
                "  - sed -i 's/^#\\?PasswordAuthentication.*/PasswordAuthentication yes/' /etc/ssh/sshd_config\n",
                "  - systemctl restart ssh || systemctl restart sshd\n",
            ),
            password
        );
        let userdata_b64 = B64.encode(&userdata);
        let server_name = format!("sel-node-{}", config.bot_code);

        // 4. Create the server. Prefer the local-disk flavor; fall back to
        // boot-from-volume only if local disk hits the `local_gb` quota. Trying
        // local first is what makes both zone shapes work: ru-3a can't schedule
        // the diskless line ("No valid host"), while ru-2a has 0 local-disk quota
        // and needs the volume path. The 403 quota error is synchronous (no
        // server is created), so the fallback leaks nothing.
        if let Some(flavor) = &local_flavor {
            match self.create_server_call(flavor, &image_id, &zone, &net_id, &userdata_b64, &server_name, None).await {
                Ok(server_id) => {
                    return Ok(CreateServerResult { server_id, root_pass: Some(password), warning: None });
                }
                Err(e) => {
                    let local_gb_quota = e.to_string().contains("local_gb");
                    if !(local_gb_quota && volume_flavor.is_some()) {
                        return Err(e);
                    }
                    log::warn!("Selectel: local-disk flavor hit local_gb quota in {}; falling back to boot-from-volume", zone);
                }
            }
        }

        // Volume-boot path (no local-disk flavor, or local disk quota-blocked).
        let flavor = volume_flavor
            .ok_or_else(|| ProviderError::Api(format!("no schedulable flavor in {}", zone)))?;
        let vol_type = self.find_boot_volume_type().await?;
        let server_id = self
            .create_server_call(&flavor, &image_id, &zone, &net_id, &userdata_b64, &server_name, Some(vol_type))
            .await?;
        Ok(CreateServerResult { server_id, root_pass: Some(password), warning: None })
    }

    async fn get_server(&self, server_id: &str) -> Result<ServerInfo, ProviderError> {
        let json = self.api_json(
            reqwest::Method::GET,
            &self.compute_url(&format!("/servers/{}", server_id)),
            None,
        ).await?;

        let srv = &json["server"];
        let os_status = srv["status"].as_str().unwrap_or("UNKNOWN");
        let task_state = srv["OS-EXT-STS:task_state"].as_str();

        let status = normalize_status(os_status, task_state);

        // Get floating IP from addresses
        let mut ipv4 = None;
        if let Some(addrs) = srv["addresses"].as_object() {
            for (_net, addr_list) in addrs {
                if let Some(arr) = addr_list.as_array() {
                    for addr in arr {
                        let ip_type = addr["OS-EXT-IPS:type"].as_str().unwrap_or("");
                        if ip_type == "floating" {
                            ipv4 = addr["addr"].as_str().map(|s| s.to_string());
                        }
                    }
                    // If no floating IP, use fixed IP
                    if ipv4.is_none() {
                        for addr in arr {
                            if addr["version"].as_i64() == Some(4) {
                                ipv4 = addr["addr"].as_str().map(|s| s.to_string());
                            }
                        }
                    }
                }
            }
        }

        Ok(ServerInfo {
            status,
            ipv4,
            root_pass: None, // Set via cloud-init, stored in DB
        })
    }

    async fn delete_server(&self, server_id: &str) -> Result<(), ProviderError> {
        let (status, text) = self.api_request(
            reqwest::Method::DELETE,
            &self.compute_url(&format!("/servers/{}", server_id)),
            None,
        ).await?;

        if !status.is_success() && status.as_u16() != 404 {
            return Err(ProviderError::Api(format!("delete server: {}: {}", status, text)));
        }

        Ok(())
    }

    async fn list_servers(&self, name_prefix: Option<&str>) -> Result<Vec<RemoteServer>, ProviderError> {
        let json = self.api_json(
            reqwest::Method::GET,
            &self.compute_url("/servers/detail?limit=100"),
            None,
        ).await?;

        let mut result = Vec::new();
        if let Some(servers) = json["servers"].as_array() {
            for s in servers {
                let name = s["name"].as_str().unwrap_or("").to_string();
                if let Some(prefix) = name_prefix {
                    if !name.starts_with(prefix) { continue; }
                }
                let id = s["id"].as_str().unwrap_or("").to_string();
                let status = s["status"].as_str().unwrap_or("");
                if id.is_empty() || status == "DELETED" { continue; }

                result.push(RemoteServer { id, name, region: Some(self.region.clone()) });
            }
        }

        Ok(result)
    }

    async fn list_regions(&self) -> Result<Vec<Region>, ProviderError> {
        // Selectel regions are static, but we query availability zones to verify access
        let json = self.api_json(
            reqwest::Method::GET,
            &self.compute_url("/os-availability-zone"),
            None,
        ).await?;

        let mut regions = Vec::new();
        if let Some(zones) = json["availabilityZoneInfo"].as_array() {
            for z in zones {
                let name = z["zoneName"].as_str().unwrap_or("").to_string();
                let available = z["zoneState"]["available"].as_bool().unwrap_or(false);
                if !available || name.is_empty() { continue; }
                regions.push(Region {
                    id: name.clone(),
                    description: name,
                });
            }
        }

        Ok(regions)
    }

    async fn cleanup_resources(&self, _active_ids: &[String]) -> Result<u32, ProviderError> {
        // Clean up unassociated floating IPs
        let json = self.api_json(
            reqwest::Method::GET,
            &self.network_url("/floatingips"),
            None,
        ).await?;

        let mut cleaned = 0u32;
        if let Some(fips) = json["floatingips"].as_array() {
            for fip in fips {
                let port_id = fip["port_id"].as_str().unwrap_or("");
                if port_id.is_empty() || fip["port_id"].is_null() {
                    // Unassociated floating IP — delete it
                    if let Some(fip_id) = fip["id"].as_str() {
                        let _ = self.api_request(
                            reqwest::Method::DELETE,
                            &self.network_url(&format!("/floatingips/{}", fip_id)),
                            None,
                        ).await;
                        cleaned += 1;
                    }
                }
            }
        }

        Ok(cleaned)
    }

    fn rate_limit_config(&self) -> RateLimitConfig {
        RateLimitConfig {
            max_free_trials: 100, // No trial limit for Selectel
            cooldown_hours: 24,
            tracking: RateLimitTracking::CountBased,
            cooldown_anchor: crate::db::CooldownAnchor::Oldest,
        }
    }

    fn server_name_prefix(&self) -> &'static str {
        "sel-node-"
    }
}

/// Map an OpenStack `status` (+ `OS-EXT-STS:task_state`) to the provisioner's
/// status set. An ACTIVE server still spawning/networking is reported as
/// "creating", not "on", so the provisioner waits for it to settle before
/// running the curl test.
fn normalize_status(os_status: &str, task_state: Option<&str>) -> String {
    match os_status {
        "BUILD" => "creating",
        "ACTIVE" => {
            if task_state.is_some_and(|t| t == "spawning" || t == "networking") {
                "creating"
            } else {
                "on"
            }
        }
        "SHUTOFF" => "off",
        "DELETED" | "SOFT_DELETED" => "deleting",
        "ERROR" => "error",
        _ => "error",
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::normalize_status;

    #[test]
    fn active_while_settling_is_creating_not_on() {
        assert_eq!(normalize_status("ACTIVE", Some("spawning")), "creating");
        assert_eq!(normalize_status("ACTIVE", Some("networking")), "creating");
    }

    #[test]
    fn active_when_idle_is_on() {
        assert_eq!(normalize_status("ACTIVE", None), "on");
        assert_eq!(normalize_status("ACTIVE", Some("")), "on");
        assert_eq!(normalize_status("ACTIVE", Some("scheduling")), "on");
    }

    #[test]
    fn lifecycle_states() {
        assert_eq!(normalize_status("BUILD", None), "creating");
        assert_eq!(normalize_status("SHUTOFF", None), "off");
        assert_eq!(normalize_status("DELETED", None), "deleting");
        assert_eq!(normalize_status("SOFT_DELETED", None), "deleting");
        assert_eq!(normalize_status("ERROR", None), "error");
    }

    #[test]
    fn unknown_status_defaults_to_error() {
        // Unlike Gcore, Selectel's catch-all is "error", not "creating".
        assert_eq!(normalize_status("WEIRD", None), "error");
        assert_eq!(normalize_status("", None), "error");
    }
}

