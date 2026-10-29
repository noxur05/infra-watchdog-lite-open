// Admin menu
pub const CB_ADMIN_SERVER: &str = "adm_server";
pub const CB_ADMIN_BACK: &str = "adm_back";


// Server
pub const CB_SERVER_REFRESH: &str = "adm_srv_ref";
pub const CB_SERVER_DETAIL_PREFIX: &str = "adm_srv:";
pub const CB_SERVER_DELETE_PREFIX: &str = "adm_srvdel:";
pub const CB_SERVER_DELETE_DB_PREFIX: &str = "adm_srvdeldb:";
pub const CB_SERVER_DELETE_PROVIDER_PREFIX: &str = "adm_srvdelpv:";
/// Force-replace = delete this server from the provider AND immediately wake
/// the per-account provisioner so it provisions a fresh one. Use when the
/// admin can see the server is bad (users can't connect) but the bot's
/// curl_test happens to pass so its auto-recycle logic won't kick in.
pub const CB_SERVER_FORCE_REPLACE_PREFIX: &str = "adm_srvrepl:";

// Test URLs
pub const CB_ADMIN_TEST_URLS: &str = "adm_turls";
pub const CB_ADMIN_ADD_TEST_URL: &str = "adm_add_turl";
pub const CB_TEST_URL_DELETE_PREFIX: &str = "adm_turldel:";

// Email list (BCC'd when the front IP changes)
pub const CB_ADMIN_EMAILS: &str = "adm_emails";
pub const CB_ADMIN_ADD_EMAILS: &str = "adm_add_emails";
pub const CB_ADMIN_DEL_EMAIL: &str = "adm_del_email";
pub const CB_EMAIL_DELETE_PREFIX: &str = "adm_emaildel:";
// Send a one-off test email through the configured SMTP sender to verify it.
pub const CB_EMAIL_TEST: &str = "adm_email_test";
// Editable mail template (subject/body with {key} placeholders, Turkmen default).
pub const CB_MAIL_TPL: &str = "adm_mailtpl";
pub const CB_MAIL_TPL_EDIT_SUBJECT: &str = "adm_mailtpl_subj";
pub const CB_MAIL_TPL_EDIT_BODY: &str = "adm_mailtpl_body";
pub const CB_MAIL_TPL_RESET: &str = "adm_mailtpl_reset";

// Main server (OVPN-AS backend): credentials + DNAT reset
pub const CB_ADMIN_MAINSRV: &str = "adm_mainsrv";
pub const CB_MAINSRV_SET_IP: &str = "adm_ms_ip";
pub const CB_MAINSRV_SET_PORT: &str = "adm_ms_port";
pub const CB_MAINSRV_SET_USER: &str = "adm_ms_user";
pub const CB_MAINSRV_SET_PASS: &str = "adm_ms_pass";
pub const CB_MAINSRV_TEST: &str = "adm_ms_test";
pub const CB_MAINSRV_RESET_DNAT: &str = "adm_ms_dnat";
pub const CB_MAINSRV_RESET_DNAT_CONFIRM: &str = "adm_ms_dnat_yes";
