use std::time::Duration;

use lettre::message::{Mailbox, MultiPart, SinglePart};
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

#[derive(Debug)]
pub enum MailerError {
    NotConfigured,
    Build(String),
    Send(String),
}

impl std::fmt::Display for MailerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MailerError::NotConfigured => write!(f, "SMTP not configured"),
            MailerError::Build(s) => write!(f, "build: {}", s),
            MailerError::Send(s) => write!(f, "send: {}", s),
        }
    }
}

#[derive(Clone)]
pub struct Mailer {
    transport: AsyncSmtpTransport<Tokio1Executor>,
    from: Mailbox,
}

impl Mailer {
    pub fn new(host: &str, port: u16, username: &str, password: &str, from: &str) -> Result<Self, MailerError> {
        if host.is_empty() || username.is_empty() || password.is_empty() || from.is_empty() {
            return Err(MailerError::NotConfigured);
        }
        let from_mb: Mailbox = from.parse().map_err(|e: lettre::address::AddressError| MailerError::Build(e.to_string()))?;
        let transport = AsyncSmtpTransport::<Tokio1Executor>::relay(host)
            .map_err(|e| MailerError::Build(e.to_string()))?
            .port(port)
            .timeout(Some(Duration::from_secs(10)))
            .credentials(Credentials::new(username.to_string(), password.to_string()))
            .build();
        Ok(Self { transport, from: from_mb })
    }

    /// Send an HTML notification to many recipients via BCC. `html_body` is the
    /// HTML message; a plain-text alternative is derived automatically
    /// (`html_to_text`) so the mail is a proper `multipart/alternative` — HTML
    /// clients render the card, text-only clients still get readable content
    /// (also better for spam scoring). The `To:` field is the sender so no
    /// recipient sees the others' addresses.
    pub async fn send_bcc(
        &self,
        recipients: &[String],
        subject: &str,
        html_body: &str,
    ) -> Result<(), MailerError> {
        if recipients.is_empty() {
            return Ok(());
        }
        let mut builder = Message::builder()
            .from(self.from.clone())
            .to(self.from.clone())
            .subject(subject);
        let mut valid = 0usize;
        for r in recipients {
            let parsed: Mailbox = match r.parse() {
                Ok(m) => m,
                Err(e) => {
                    log::warn!("Skipping invalid recipient {}: {}", r, e);
                    continue;
                }
            };
            builder = builder.bcc(parsed);
            valid += 1;
        }
        if valid == 0 {
            log::warn!("send_bcc: no valid recipients among {} entries — not sending", recipients.len());
            return Ok(());
        }
        let msg = builder
            .multipart(
                MultiPart::alternative()
                    .singlepart(SinglePart::plain(html_to_text(html_body)))
                    .singlepart(SinglePart::html(html_body.to_string())),
            )
            .map_err(|e| MailerError::Build(e.to_string()))?;
        self.transport.send(msg).await
            .map_err(|e| MailerError::Send(e.to_string()))?;
        Ok(())
    }
}

/// Parse a free-form blob of emails separated by whitespace, commas, or
/// semicolons. Lowercases and dedupes (preserving order). Skips empties and
/// obviously invalid tokens.
pub fn parse_email_blob(text: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for token in text.split(|c: char| c.is_whitespace() || c == ',' || c == ';') {
        let s = token.trim().trim_matches(|c: char| c == '<' || c == '>' || c == '"' || c == '\'');
        if s.is_empty() { continue; }
        let lower = s.to_lowercase();
        if !is_plausible_email(&lower) { continue; }
        if seen.insert(lower.clone()) {
            out.push(lower);
        }
    }
    out
}

fn is_plausible_email(s: &str) -> bool {
    let at = s.find('@');
    let Some(at) = at else { return false };
    let (local, domain) = s.split_at(at);
    let domain = &domain[1..];
    !local.is_empty() && domain.contains('.') && !domain.starts_with('.') && !domain.ends_with('.')
}

// ── Mail templates ──
//
// The IP-change notification subject/body are editable templates stored in
// `bot_settings` (see `db::get_mail_template`); these are the built-in Turkmen
// defaults used when the admin hasn't set a custom one. Both support the same
// `{key}` placeholders, substituted by `render_template`.

/// Placeholder keys a template may use, with a short Turkmen description.
/// Surfaced verbatim on the "📝 Mail template" admin screen so the admin knows
/// which keys are available. `{{IP}}` is required in the body; `{{Date}}` is
/// optional.
pub const MAIL_TEMPLATE_KEYS: &[(&str, &str)] = &[
    ("{{IP}}", "serweriň salgysy (hökman)"),
    ("{{Date}}", "üýtgän senesi (islege görä)"),
];

/// The one placeholder a body template MUST contain — otherwise recipients would
/// never learn the new address. Enforced when the admin saves a custom body.
pub const REQUIRED_TEMPLATE_KEY: &str = "{{IP}}";

/// True if `tpl` contains the required `{{IP}}` placeholder.
pub fn template_has_required_key(tpl: &str) -> bool {
    tpl.contains(REQUIRED_TEMPLATE_KEY)
}

/// Default subject (Turkmen): "New Address".
pub const DEFAULT_MAIL_SUBJECT: &str = "Täze Salgy";

/// Default body — a professional, responsive Turkmen HTML card. Uses `{ip}` and
/// `{date}`. Table-based with inline styles for broad email-client support.
pub const DEFAULT_MAIL_BODY: &str = r#"<!DOCTYPE html>
<html lang="tk">
<body style="margin:0;padding:0;background:#eef1f4;">
  <table role="presentation" width="100%" cellpadding="0" cellspacing="0" style="background:#eef1f4;padding:24px 12px;font-family:Segoe UI,Roboto,Helvetica,Arial,sans-serif;">
    <tr><td align="center">
      <table role="presentation" width="480" cellpadding="0" cellspacing="0" style="max-width:480px;width:100%;background:#ffffff;border-radius:14px;overflow:hidden;box-shadow:0 2px 8px rgba(16,24,40,.08);">
        <tr><td style="background:#12694f;padding:22px 28px;">
          <div style="color:#ffffff;font-size:20px;font-weight:700;letter-spacing:.2px;">🔒 Täze Salgy</div>
        </td></tr>
        <tr><td style="padding:26px 28px 8px;">
          <p style="margin:0 0 14px;color:#111827;font-size:15px;line-height:1.6;">Salam!</p>
          <p style="margin:0 0 20px;color:#4b5563;font-size:15px;line-height:1.6;">VPN serweriňiziň salgysy täzelendi. Täze baglanyşyk maglumatlary:</p>
          <table role="presentation" width="100%" cellpadding="0" cellspacing="0" style="background:#f3f6f9;border-radius:10px;">
            <tr><td style="padding:16px 18px;">
              <div style="color:#6b7280;font-size:11px;text-transform:uppercase;letter-spacing:.6px;margin-bottom:6px;">Täze salgy</div>
              <div style="color:#0f172a;font-size:22px;font-weight:700;font-family:Consolas,Menlo,monospace;">{{IP}}</div>
            </td></tr>
          </table>
          <p style="margin:14px 0 0;color:#6b7280;font-size:13px;">Sene: {{Date}}</p>
          <p style="margin:20px 0 0;color:#4b5563;font-size:15px;line-height:1.6;">Baglanyşygy dowam etdirmek üçin programmaňyzyň sazlamalaryndaky serwer salgysyny ýokardaky täze salgy bilen çalşyň.</p>
          <p style="margin:22px 0 4px;color:#111827;font-size:15px;">Sag boluň!</p>
        </td></tr>
        <tr><td style="padding:8px 28px 24px;">
          <div style="border-top:1px solid #eceff2;padding-top:14px;color:#9aa2ad;font-size:12px;">Bu awtomatiki iberilen habardyr.</div>
        </td></tr>
      </table>
    </td></tr>
  </table>
</body>
</html>"#;

/// Crude HTML→plain-text reducer for the `multipart/alternative` fallback part.
/// Not a full parser: turns common block/break tags into newlines, drops the
/// rest of the tags, decodes a few entities, and collapses blank runs. Good
/// enough for a text fallback of our own templates.
pub fn html_to_text(html: &str) -> String {
    let mut s = html.to_string();
    for (pat, rep) in [
        ("<br>", "\n"), ("<br/>", "\n"), ("<br />", "\n"),
        ("</p>", "\n"), ("</div>", "\n"), ("</tr>", "\n"),
        ("</h1>", "\n"), ("</h2>", "\n"), ("</table>", "\n"),
    ] {
        s = s.replace(pat, rep);
    }
    // Strip all remaining tags.
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out = out
        .replace("&nbsp;", " ").replace("&amp;", "&")
        .replace("&lt;", "<").replace("&gt;", ">").replace("&#39;", "'");
    // Trim each line, then collapse 2+ blank lines into one.
    let mut result = String::new();
    let mut blank_run = 0;
    for line in out.lines() {
        let t = line.trim();
        if t.is_empty() {
            blank_run += 1;
            if blank_run > 1 { continue; }
        } else {
            blank_run = 0;
        }
        result.push_str(t);
        result.push('\n');
    }
    result.trim().to_string()
}

/// Substitute every `{{KEY}}` in `tpl` with its value from `vars` (e.g. key
/// `"IP"` replaces the literal `{{IP}}`). Unknown placeholders are left as-is
/// (so a typo in a template is visible, not silently blanked). Order-independent;
/// a value is never re-scanned for further keys.
pub fn render_template(tpl: &str, vars: &[(&str, &str)]) -> String {
    let mut out = tpl.to_string();
    for (key, val) in vars {
        let placeholder = ["{{", key, "}}"].concat();
        out = out.replace(&placeholder, val);
    }
    out
}

#[cfg(test)]
mod template_tests {
    use super::*;

    #[test]
    fn render_substitutes_known_keys() {
        let body = render_template(
            "salgy: {{IP}}, sene: {{Date}}",
            &[("IP", "1.2.3.4"), ("Date", "2026-07-03")],
        );
        assert_eq!(body, "salgy: 1.2.3.4, sene: 2026-07-03");
    }

    #[test]
    fn render_leaves_unknown_keys_untouched() {
        assert_eq!(render_template("hi {{nope}}", &[("IP", "x")]), "hi {{nope}}");
    }

    #[test]
    fn required_key_detection() {
        assert!(template_has_required_key("... {{IP}} ..."));
        assert!(!template_has_required_key("no key here"));
        assert!(!template_has_required_key("wrong case {{ip}}"));
        assert!(template_has_required_key(DEFAULT_MAIL_BODY));
    }

    #[test]
    fn default_body_renders_the_ip() {
        let out = render_template(DEFAULT_MAIL_BODY, &[("IP", "9.9.9.9"), ("Date", "2026-01-01")]);
        assert!(out.contains("9.9.9.9"), "default body should carry the ip");
        assert!(!out.contains("{{IP}}"), "no unsubstituted {{IP}} left");
        assert!(out.contains('<'), "default body is HTML");
    }

    #[test]
    fn html_to_text_strips_tags_and_keeps_content() {
        let rendered = render_template(DEFAULT_MAIL_BODY, &[("IP", "5.5.5.5"), ("Date", "2026-01-01")]);
        let text = html_to_text(&rendered);
        assert!(!text.contains('<'), "plain fallback must not contain tags");
        assert!(text.contains("5.5.5.5"), "plain fallback keeps the ip");
        assert!(text.contains("Salam!"), "plain fallback keeps the greeting");
        assert!(!text.contains("\n\n\n"), "blank runs collapsed");
    }
}
