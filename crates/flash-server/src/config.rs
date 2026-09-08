//! Environment-based configuration. All public URLs derive from base_url.

use std::net::SocketAddr;
use std::path::PathBuf;

/// Outbound email (Resend). Present => self-serve signup is enabled.
#[derive(Debug, Clone)]
pub struct MailConfig {
    pub resend_api_key: String,
    /// Verified sender, e.g. "Flash <no-reply@flash.example.com>".
    pub from: String,
}

/// An S3-compatible bucket for media blobs (AWS S3, Cloudflare R2, MinIO,
/// Backblaze B2, …). Absent => blobs live on local disk under
/// data_dir/media, which is what most installs want. All four must be set
/// together.
#[derive(Debug, Clone)]
pub struct MediaR2Config {
    /// Scheme + host of the S3 endpoint, e.g. https://s3.us-east-1.amazonaws.com
    pub endpoint: String,
    pub bucket: String,
    pub access_key_id: String,
    pub secret_access_key: String,
}

#[derive(Debug, Clone)]
pub struct Config {
    /// Loopback by default: a reverse proxy or tunnel in front terminates TLS.
    pub bind: SocketAddr,
    /// FLASH_CLIENT_IP_HEADER: the request header that carries the real
    /// client address when a proxy sits in front (`cf-connecting-ip`,
    /// `x-real-ip`, …). Unset => the TCP peer address is used. Set it only
    /// when nothing but that proxy can reach the port, since any header
    /// is forgeable by a direct caller.
    pub client_ip_header: Option<String>,
    /// Canonical public origin, e.g. https://cards.example.com
    pub base_url: String,
    /// FLASH_SUPPORT_EMAIL: printed wherever a page shows a contact
    /// address. Unset hides those lines.
    pub support_email: Option<String>,
    pub data_dir: PathBuf,
    pub mail: Option<MailConfig>,
    /// FLASH_DEV_MAIL_LOG=1: a mailer without a provider — every message
    /// and its link go to the log instead (dev / self-host convenience).
    pub dev_mail_log: bool,
    pub media_r2: Option<MediaR2Config>,
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        let bind = std::env::var("FLASH_BIND")
            .unwrap_or_else(|_| "127.0.0.1:8437".to_string())
            .parse()
            .map_err(|e| format!("FLASH_BIND: {e}"))?;
        let base_url = std::env::var("FLASH_BASE_URL")
            .unwrap_or_else(|_| "http://localhost:8437".to_string())
            .trim_end_matches('/')
            .to_string();
        let support_email = env_opt("FLASH_SUPPORT_EMAIL");
        let data_dir =
            PathBuf::from(std::env::var("FLASH_DATA_DIR").unwrap_or_else(|_| "./data".to_string()));

        let mail = match (env_opt("RESEND_API_KEY"), env_opt("FLASH_EMAIL_FROM")) {
            (Some(resend_api_key), Some(from)) => Some(MailConfig {
                resend_api_key,
                from,
            }),
            (None, None) => None,
            _ => return Err("RESEND_API_KEY and FLASH_EMAIL_FROM must be set together".to_string()),
        };
        let dev_mail_log = std::env::var("FLASH_DEV_MAIL_LOG").is_ok_and(|v| v == "1");

        let r2_vars = [
            env_opt("FLASH_MEDIA_R2_ENDPOINT"),
            env_opt("FLASH_MEDIA_R2_BUCKET"),
            env_opt("FLASH_MEDIA_R2_ACCESS_KEY_ID"),
            env_opt("FLASH_MEDIA_R2_SECRET_ACCESS_KEY"),
        ];
        let media_r2 = match r2_vars {
            [Some(endpoint), Some(bucket), Some(access_key_id), Some(secret_access_key)] => {
                Some(MediaR2Config {
                    endpoint,
                    bucket,
                    access_key_id,
                    secret_access_key,
                })
            }
            [None, None, None, None] => None,
            _ => {
                return Err("FLASH_MEDIA_R2_ENDPOINT, FLASH_MEDIA_R2_BUCKET, \
                            FLASH_MEDIA_R2_ACCESS_KEY_ID, and FLASH_MEDIA_R2_SECRET_ACCESS_KEY \
                            must be set together"
                    .to_string())
            }
        };

        // Header names are case-insensitive on the wire; http::HeaderMap
        // wants them lowercase for lookup.
        let client_ip_header = env_opt("FLASH_CLIENT_IP_HEADER").map(|h| h.to_ascii_lowercase());

        Ok(Self {
            bind,
            client_ip_header,
            base_url,
            support_email,
            data_dir,
            mail,
            dev_mail_log,
            media_r2,
        })
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("flash.db")
    }
}

/// An env var, with blank treated as unset.
pub fn env_opt(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

/// A comma-separated list; unset or blank => empty.
pub fn env_list(name: &str) -> Vec<String> {
    env_opt(name)
        .map(|v| {
            v.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}
