/// Escape special characters for MarkdownV2
pub fn escape_md(text: &str) -> String {
    let special = ['_', '*', '[', ']', '(', ')', '~', '`', '>', '#', '+', '-', '=', '|', '{', '}', '.', '!'];
    let mut escaped = String::with_capacity(text.len() * 2);
    for c in text.chars() {
        if special.contains(&c) {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
}

/// Build a `reqwest::Client` with sane timeouts so a hung provider API
/// can't park a Tokio task indefinitely. `Client::new()` has NO request
/// timeout by default, which is what made AccountProvisioners "silently
/// die" — a single stuck HTTP call would block the per-account loop
/// forever with no error or log output until a service restart.
///
/// connect_timeout is short (fail fast on network blackholes); the
/// overall request timeout is generous (some provider APIs take >30s
/// for create-server under load).
pub fn build_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// Map a (provider, region) pair to a `(flag_emoji, country_name)` tuple used
/// for user-facing country labels — so it's clear which country an exit server
/// routes through.
///
/// Add a new region by extending the inner match; `("🌐", "Unknown")` is the
/// catch-all and is what previously-provisioned servers (whose `region`
/// column is NULL) will display. That fallback is intentionally bland —
/// user-side it's clearly distinguishable from a real country pick.
#[allow(dead_code)] // tested country-label helper; not wired into the OpenVPN status UI yet
pub fn region_to_country(provider_type: &str, region: &str) -> (&'static str, &'static str) {
    match provider_type {
        // Selectel multi-country: prefix encodes the country (uz-/kz-/ke-/ru-/gis-).
        "selectel" => match region.split('-').next().unwrap_or("") {
            "ru" | "gis" => ("🇷🇺", "Russia"),
            "uz" => ("🇺🇿", "Uzbekistan"),
            "kz" => ("🇰🇿", "Kazakhstan"),
            "ke" => ("🇰🇪", "Kenya"),
            _ => ("🌐", "Unknown"),
        },

        _ => ("🌐", "Unknown"),
    }
}

/// Resolve an IP address to (country_name, ISO_alpha2_code) via ip-api.com.
/// Best-effort: returns (None, None) on any HTTP/JSON failure instead of
/// blocking whatever flow called it. Free tier has no auth; uses plain HTTP.
#[allow(dead_code)] // tested IP→country helper; not currently wired
pub async fn resolve_country_from_ip(ip: &str) -> (Option<String>, Option<String>) {
    let client = build_http_client();
    let url = format!("http://ip-api.com/json/{}?fields=country,countryCode", ip);
    let resp = match client.get(&url).send().await {
        Ok(r) => r,
        Err(_) => return (None, None),
    };
    let json: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(_) => return (None, None),
    };
    let name = json["country"].as_str().map(String::from);
    let code = json["countryCode"].as_str().map(String::from);
    (name, code)
}

/// ISO 3166-1 alpha-2 → flag emoji via regional indicator symbols.
/// Each ASCII letter is mapped to U+1F1E6 + (letter - 'A'); two letters paired
/// render as the country flag in modern fonts. "DE" → 🇩🇪, "NL" → 🇳🇱.
#[allow(dead_code)] // tested helper; not currently wired
pub fn flag_emoji_from_cc(cc: &str) -> String {
    cc.chars()
        .filter_map(|c| {
            let upper = c.to_ascii_uppercase();
            if upper.is_ascii_alphabetic() {
                std::char::from_u32(0x1F1E6 + (upper as u32 - 'A' as u32))
            } else {
                None
            }
        })
        .collect()
}

/// Which SSH user to log in as for a provisioned server. Selectel's Ubuntu
/// image allows direct root password login (cloud-init writes the password).
pub fn ssh_user_for(_provider_type: &str) -> &'static str {
    "root"
}

