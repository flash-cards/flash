//! Outbound email: the password-reset loop here, the hosted product's
//! signup loop on top. One trait, three implementations — Resend
//! (production), journal logging (dev/self-host), and capture (tests).
//! Sync by design: senders run inside spawn_blocking like every other
//! blocking call in this codebase.

use crate::config::MailConfig;

#[derive(Debug, Clone)]
pub struct OutgoingEmail {
    pub to: String,
    pub subject: String,
    pub text: String,
    pub html: String,
}

pub trait Mailer: Send + Sync {
    fn send(&self, mail: &OutgoingEmail) -> Result<(), String>;
}

// ---- production: Resend HTTPS API ----

pub struct ResendMailer {
    api_key: String,
    from: String,
    agent: ureq::Agent,
}

impl ResendMailer {
    pub fn new(config: &MailConfig) -> Self {
        Self {
            api_key: config.resend_api_key.clone(),
            from: config.from.clone(),
            agent: crate::http::agent(std::time::Duration::from_secs(15), true),
        }
    }
}

impl Mailer for ResendMailer {
    fn send(&self, mail: &OutgoingEmail) -> Result<(), String> {
        let response = self
            .agent
            .post("https://api.resend.com/emails")
            .header("Authorization", &format!("Bearer {}", self.api_key))
            .send_json(serde_json::json!({
                "from": self.from,
                "to": [mail.to],
                "subject": mail.subject,
                "text": mail.text,
                "html": mail.html,
            }))
            .map_err(|e| format!("resend: {e}"))?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(format!("resend: HTTP {}", response.status()))
        }
    }
}

// ---- dev / self-host: the link lands in the journal ----
// Mirrors the first-boot bootstrap invite, which is also journal-delivered.

pub struct LogMailer;

impl Mailer for LogMailer {
    fn send(&self, mail: &OutgoingEmail) -> Result<(), String> {
        tracing::info!("dev mail to {}: {}", mail.to, mail.subject);
        tracing::info!("{}", mail.text);
        Ok(())
    }
}

// ---- tests: capture instead of sending ----

#[derive(Default)]
pub struct CaptureMailer(pub parking_lot::Mutex<Vec<OutgoingEmail>>);

impl Mailer for CaptureMailer {
    fn send(&self, mail: &OutgoingEmail) -> Result<(), String> {
        self.0.lock().push(mail.clone());
        Ok(())
    }
}

// ---- message builders ----

pub fn password_reset_email(base_url: &str, token: &str) -> OutgoingEmail {
    let link = format!("{base_url}/reset/{token}");
    OutgoingEmail {
        to: String::new(), // caller fills in
        subject: "Reset your Flash password".to_string(),
        text: format!(
            "Someone (probably you) asked to reset the password for this Flash account.\n\n\
             Set a new password here:\n\n{link}\n\nThe link works once and expires in \
             1 hour. If this wasn't you, ignore this email — nothing changes.\n"
        ),
        html: format!(
            "<p>Someone (probably you) asked to reset the password for this Flash \
             account.</p><p>Set a new password here:</p><p><a href=\"{link}\">{link}</a></p>\
             <p>The link works once and expires in 1 hour. If this wasn't you, ignore \
             this email — nothing changes.</p>"
        ),
    }
}

/// The five HTML-significant characters, for text dropped into a mail body.
pub fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_reset_email_carries_the_reset_link() {
        let mail = password_reset_email("https://flash.example.com", "tok456");
        assert!(mail.text.contains("https://flash.example.com/reset/tok456"));
        assert!(mail.html.contains("https://flash.example.com/reset/tok456"));
    }
}
