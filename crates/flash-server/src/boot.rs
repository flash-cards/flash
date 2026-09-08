//! What every binary does around `build_app`: the health probe and the
//! first-boot admin invite.

use axum::{extract::State, Json};

use crate::auth::{hash_token, new_token};
use crate::service::{now_ms, Services};
use crate::state::AppState;

/// The rate limiters key on the client's address. Behind a reverse proxy
/// every request arrives from the proxy, so without a configured header
/// the whole instance shares one bucket and twenty bad sign-ins a
/// minute from anyone lock everyone out. A loopback bind is the
/// documented proxy deployment, so the two together are worth a warning
/// on every boot until the header is set.
pub fn warn_about_shared_buckets(config: &crate::config::Config) {
    // The session cookie is always Secure, so over plain http the web UI
    // signs nobody in except on localhost, which browsers treat as a
    // secure context. Better said at boot than discovered at the login
    // form.
    let base = config.base_url.as_str();
    let plain_http = base.starts_with("http://");
    let loopback_base = base.starts_with("http://localhost")
        || base.starts_with("http://127.0.0.1")
        || base.starts_with("http://[::1]");
    if plain_http && !loopback_base {
        tracing::warn!(
            base_url = %config.base_url,
            "FLASH_BASE_URL is plain http on a non-loopback host: the web UI's session \
             cookie is Secure and browsers will drop it, so nobody can sign in through a \
             browser. Put the server behind HTTPS, or use http://localhost for a trial."
        );
    }
    if config.client_ip_header.is_none() && config.bind.ip().is_loopback() {
        tracing::warn!(
            bind = %config.bind,
            "FLASH_CLIENT_IP_HEADER is unset while listening on loopback: behind a proxy \
             every client shares one rate-limit bucket. Set it to the header your proxy \
             fills (x-real-ip, cf-connecting-ip) once nothing but the proxy can reach the port."
        );
    }
}

/// `/healthz`: a real DB round-trip so "healthy" means "serving".
pub async fn healthz(State(state): State<AppState>) -> Json<serde_json::Value> {
    let db_ok = tokio::task::spawn_blocking({
        let services = state.services.clone();
        move || services.store().health_check().is_ok()
    })
    .await
    .unwrap_or(false);
    Json(serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "db": if db_ok { "ok" } else { "error" },
        "uptime_ms": now_ms() - state.started_at_ms,
    }))
}

/// First boot on an empty database: mint a one-time admin enrollment link
/// and print it to the journal. Nothing else can create the first user.
pub fn bootstrap_invite(services: &Services, base_url: &str) {
    let store = services.store();
    match store.user_count() {
        Ok(0) => {
            let token = new_token();
            let now = now_ms();
            let day_ms = 24 * 60 * 60 * 1000;
            if let Err(e) = store.create_invite(
                &hash_token(&token),
                "Admin",
                "admin",
                None,
                now + day_ms,
                now,
            ) {
                tracing::error!("bootstrap invite: {e}");
                return;
            }
            tracing::info!("no users yet — enroll the first admin within 24h at:");
            tracing::info!("{base_url}/enroll/{token}");
        }
        Ok(_) => {}
        Err(e) => tracing::error!("user_count: {e}"),
    }
}
