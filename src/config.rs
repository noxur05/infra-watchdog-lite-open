use dotenv::dotenv;
use std::env;
use std::sync::OnceLock;

static CONFIG: OnceLock<Config> = OnceLock::new();

pub fn get_config() -> &'static Config {
    CONFIG.get_or_init(Config::from_env)
}

pub struct Config {
    pub telegram_bot_api: String,
    pub database_url: String,
    pub check_interval_secs: u64,
    pub test_urls: Vec<String>,
    // OpenVPN Access Server backend (the fixed exit the front DNATs to).
    // The bot calls its REST API to point `host.name` at the current front IP.
    pub openvpn_backend_ip: String,
    pub openvpn_api_user: String,
    pub openvpn_api_pass: String,
    pub openvpn_api_port: u16,
    // SMTP relay for BCC-ing the email list when the front IP changes.
    // All empty → mailing is disabled silently (Mailer::new → NotConfigured).
    pub smtp_host: String,
    pub smtp_port: u16,
    pub smtp_username: String,
    pub smtp_password: String,
    pub smtp_from: String,
    // Symmetric key for the daily encrypted DB backup.
    pub backup_password: String,
    // Bot code for host naming
    pub bot_code: String,
    // Admin telegram IDs (only these users can manage the bot; all get alerts
    // and the daily backup). Parsed from a comma/space-separated ADMIN_TELEGRAM_ID.
    pub admin_telegram_ids: Vec<i64>,
}

impl Config {
    pub fn from_env() -> Self {
        dotenv().ok();

        // ADMIN_TELEGRAM_ID accepts one OR several IDs, separated by commas or
        // whitespace (e.g. "111,222"). All listed users may control the bot and
        // receive its alerts/backups. Order-preserving, deduped.
        let admin_telegram_ids: Vec<i64> = {
            let mut seen = std::collections::HashSet::new();
            env::var("ADMIN_TELEGRAM_ID").unwrap_or_default()
                .split(|c: char| c == ',' || c.is_whitespace())
                .filter_map(|s| s.trim().parse::<i64>().ok())
                .filter(|&id| id != 0 && seen.insert(id))
                .collect()
        };

        Self {
            telegram_bot_api: env::var("TELOXIDE_TOKEN").expect("TELOXIDE_TOKEN must be set"),
            database_url: env::var("DATABASE_URL").unwrap_or_else(|_| "sqlite:watchdog.db".to_string()),
            check_interval_secs: env::var("CHECK_INTERVAL_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(120),
            test_urls: vec![
                "https://turkmenistanairlines.tm/".to_string(),
                "https://railway.gov.tm/".to_string(),
            ],
            // OpenVPN Access Server backend
            openvpn_backend_ip: env::var("OPENVPN_BACKEND_IP").unwrap_or_default(),
            openvpn_api_user: env::var("OPENVPN_API_USER").unwrap_or_default(),
            openvpn_api_pass: env::var("OPENVPN_API_PASS").unwrap_or_default(),
            openvpn_api_port: env::var("OPENVPN_API_PORT").ok().and_then(|s| s.parse().ok()).unwrap_or(943),
            // SMTP relay (BCC notifications)
            smtp_host: env::var("SMTP_HOST").unwrap_or_default(),
            smtp_port: env::var("SMTP_PORT").ok().and_then(|s| s.parse().ok()).unwrap_or(465),
            smtp_username: env::var("SMTP_USERNAME").unwrap_or_default(),
            smtp_password: env::var("SMTP_PASSWORD").unwrap_or_default(),
            smtp_from: env::var("SMTP_FROM").unwrap_or_default(),
            // Backup encryption key
            backup_password: env::var("BACKUP_PASSWORD").unwrap_or_default(),
            // Bot
            bot_code: env::var("BOT_CODE").unwrap_or_else(|_| "BWD".to_string()),
            admin_telegram_ids,
        }
    }

    /// Is this Telegram user one of the authorized admins?
    pub fn is_admin(&self, id: i64) -> bool {
        self.admin_telegram_ids.contains(&id)
    }
}
