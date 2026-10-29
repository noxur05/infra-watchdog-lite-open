use async_trait::async_trait;

// ── Shared Types ──

pub struct CreateServerResult {
    pub server_id: String,
    pub root_pass: Option<String>,
    pub warning: Option<String>,
}

pub struct ServerInfo {
    pub status: String,
    pub ipv4: Option<String>,
    pub root_pass: Option<String>,
}

pub struct RemoteServer {
    pub id: String,
    pub name: String,
    pub region: Option<String>,
}

#[allow(dead_code)] // populated by CloudProvider::list_regions; fields not displayed in this build
pub struct Region {
    pub id: String,
    pub description: String,
}

#[derive(Debug)]
pub enum ProviderError {
    Auth(String),
    Api(String),
    Network(String),
    #[allow(dead_code)] // abstraction variant; the Selectel-only build never produces it
    RateLimit(String),
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Auth(msg) => write!(f, "auth error: {}", msg),
            Self::Api(msg) => write!(f, "API error: {}", msg),
            Self::Network(msg) => write!(f, "network error: {}", msg),
            Self::RateLimit(msg) => write!(f, "rate limit: {}", msg),
        }
    }
}

impl ProviderError {
    pub fn is_auth(&self) -> bool {
        matches!(self, Self::Auth(_))
    }

    /// True for a provider-signalled rate-limit / quota-exhaustion error.
    /// Lets the provisioner back off (set a cooldown) instead of treating it
    /// as a generic cycle error that re-notifies on the normal cadence.
    pub fn is_rate_limit(&self) -> bool {
        matches!(self, Self::RateLimit(_))
    }
}

#[derive(Debug, Clone)]
pub enum RateLimitTracking {
    /// Provider signals limit via API response (TimeWeb: warning field;
    /// Beget: CUSTOMER_IP_LIMIT_REACHED daily IP quota)
    #[allow(dead_code)] // abstraction variant; Selectel is CountBased
    ApiSignal,
    /// Count DB rows to enforce limit (e.g. Vultr/UpCloud/Selectel trial caps)
    CountBased,
}

#[derive(Debug, Clone)]
pub struct RateLimitConfig {
    pub max_free_trials: u32,
    pub cooldown_hours: u32,
    pub tracking: RateLimitTracking,
    /// Which timestamp the cooldown is measured from when the cap is hit
    /// (CountBased only). Most providers want `Oldest` (sliding window — resume
    /// as soon as one slot reopens). Cost-sensitive trial providers want
    /// `Latest` (flat `cooldown_hours` quiet period from the most recent trial).
    pub cooldown_anchor: crate::db::CooldownAnchor,
}

// ── CloudProvider Trait ──

#[async_trait]
pub trait CloudProvider: Send + Sync {
    /// Create a server. Returns provider's native server ID + optional password + optional warning.
    async fn create_server(&self, location: &str) -> Result<CreateServerResult, ProviderError>;

    /// Poll server status. Returns normalized status + IP + password (if available).
    async fn get_server(&self, server_id: &str) -> Result<ServerInfo, ProviderError>;

    /// Delete a server on the provider.
    async fn delete_server(&self, server_id: &str) -> Result<(), ProviderError>;

    /// List servers on provider matching a name prefix (for reconciliation).
    async fn list_servers(&self, name_prefix: Option<&str>) -> Result<Vec<RemoteServer>, ProviderError>;

    /// List available regions for account setup UI.
    async fn list_regions(&self) -> Result<Vec<Region>, ProviderError>;

    /// Cleanup orphaned resources (floating IPs for TimeWeb, no-op for others).
    async fn cleanup_resources(&self, active_ids: &[String]) -> Result<u32, ProviderError>;

    /// Provider's rate limit configuration.
    fn rate_limit_config(&self) -> RateLimitConfig;

    /// Server name prefix for reconciliation ("vpn-node-", "beget-node-").
    fn server_name_prefix(&self) -> &'static str;
}
