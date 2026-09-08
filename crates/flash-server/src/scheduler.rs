//! The background housekeeping no request path was ever a good home
//! for: daily purges of what has expired. Always on, in every binary.

use std::time::Duration;

use crate::service::{now_ms, Services};

const DAY_MS: i64 = 24 * 60 * 60 * 1000;

/// How old an unlinked media row must be before the sweep takes it: an
/// editor upload is linked when its note is saved, which is minutes
/// away at most.
const UNLINKED_MEDIA_GRACE_MS: i64 = 60 * 60 * 1000;

/// Daily purges: tokens revoked or expired long ago, spent password
/// resets, export packages a crash left on disk. Cheap, idempotent, safe
/// to run any time.
pub fn housekeeping(
    services: &Services,
    media: &dyn crate::media_store::MediaStore,
    data_dir: &std::path::Path,
    now_ms: i64,
) {
    let exports = crate::flows::export_flow::exports_dir_in(data_dir);
    let swept = crate::flows::export_flow::sweep_stale_exports(&exports);
    if swept > 0 {
        tracing::info!("removed {swept} abandoned export files");
    }
    let store = services.store();
    // Media no card links any more (a deleted or plain-edited card, an
    // editor upload never saved into a note): the rows go once they are
    // an hour old, and the blobs nobody else holds go with them.
    match store.sweep_unlinked_media(now_ms - UNLINKED_MEDIA_GRACE_MS) {
        Ok(orphans) => {
            let mut removed = 0u32;
            for sha in &orphans {
                match media.delete(sha) {
                    Ok(()) => removed += 1,
                    Err(e) => tracing::warn!(sha = ?sha, "orphan blob: {e}"),
                }
            }
            if removed > 0 {
                tracing::info!("removed {removed} orphaned media blobs");
            }
        }
        Err(e) => tracing::warn!("sweep media: {e}"),
    }
    match store.purge_expired_tokens(now_ms) {
        Ok(n) if n > 0 => tracing::info!("purged {n} expired tokens"),
        Ok(_) => {}
        Err(e) => tracing::warn!("purge tokens: {e}"),
    }
    if let Err(e) = store.purge_expired_password_resets(now_ms) {
        tracing::warn!("purge password resets: {e}");
    }
    match store.purge_expired_oauth_codes(now_ms) {
        Ok(n) if n > 0 => tracing::info!("purged {n} expired OAuth codes"),
        Ok(_) => {}
        Err(e) => tracing::warn!("purge OAuth codes: {e}"),
    }
    match store.purge_unused_oauth_clients(now_ms) {
        Ok(n) if n > 0 => tracing::info!("purged {n} unused OAuth clients"),
        Ok(_) => {}
        Err(e) => tracing::warn!("purge OAuth clients: {e}"),
    }
}

/// Housekeeping once at boot and then daily. Every binary spawns this.
pub async fn run_housekeeping(
    services: Services,
    media: std::sync::Arc<dyn crate::media_store::MediaStore>,
    data_dir: std::path::PathBuf,
) {
    let mut interval = tokio::time::interval(Duration::from_millis(DAY_MS as u64));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        let services = services.clone();
        let media = media.clone();
        let data_dir = data_dir.clone();
        let _ = tokio::task::spawn_blocking(move || {
            housekeeping(&services, media.as_ref(), &data_dir, now_ms())
        })
        .await;
    }
}
