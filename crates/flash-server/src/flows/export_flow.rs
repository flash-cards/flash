//! Export: never gated ("your cards are always yours"), even over the
//! free cap. Both surfaces serve the same bytes.
//!
//! The .apkg is built on disk, one media blob in memory at a time, then
//! streamed to the client from the file; the file lives exactly as long
//! as the download (`ExportFile` removes it on drop). A daily sweep
//! removes anything a crash mid-download left behind.

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context, Poll};

use axum::body::Body;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use tokio::io::{AsyncRead, ReadBuf};

use crate::auth::new_token;
use crate::config::Config;
use crate::media_store::MediaStore;
use crate::service::{Result, ServiceError, Services};
use crate::state::HeavyPermit;

/// Where finished packages wait to be sent.
pub fn exports_dir(config: &Config) -> PathBuf {
    exports_dir_in(&config.data_dir)
}

pub fn exports_dir_in(data_dir: &Path) -> PathBuf {
    data_dir.join("exports")
}

/// A leftover older than this was abandoned by a crash, not by a slow
/// download.
const STALE_EXPORT_MS: u128 = 60 * 60 * 1000;

/// A finished package on disk. Dropping it deletes the file, so whoever
/// holds it decides when the download is over.
pub struct ExportFile {
    pub path: PathBuf,
    pub size: u64,
}

impl Drop for ExportFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// The Anki package with every live card and its media, built on disk. A
/// heavy job: the caller holds a permit; the peak in memory is one blob.
pub fn apkg(
    permit: &HeavyPermit,
    services: &Services,
    media: &dyn MediaStore,
    config: &Config,
    now_ms: i64,
) -> Result<ExportFile> {
    let user = permit.user();
    let cards = services.export_cards(user)?;
    let plan = crate::media::plan_export_media(services, user);
    if plan.truncated {
        tracing::warn!(
            user = user.raw(),
            bundled = plan.media.entries.len(),
            "export media over the package budget; the rest export as filenames"
        );
    }
    let dir = exports_dir(config);
    std::fs::create_dir_all(&dir)
        .map_err(|e| ServiceError::Internal(format!("exports dir: {e}")))?;
    let path = dir.join(format!("{}-{}.apkg", user.raw(), new_token()));
    let file = std::fs::File::create(&path)
        .map_err(|e| ServiceError::Internal(format!("export file: {e}")))?;
    // From here the file exists; an early return drops the guard and
    // removes it.
    let mut export = ExportFile { path, size: 0 };
    let mut read = |entry: &flash_store::export::ExportMediaEntry| {
        media
            .get(&entry.sha256, None)
            .map(|blob| blob.bytes)
            .map_err(|e| format!("media {}: {e}", entry.sha256))
    };
    // A failure to build the package is ours (a temp file, SQLite, zip,
    // the blob store), never the user's: logged in full, shown as
    // "internal error".
    let file = flash_store::export::build_apkg_into(&cards, &plan.media, &mut read, now_ms, file)
        .map_err(ServiceError::Internal)?;
    export.size = file
        .metadata()
        .map_err(|e| ServiceError::Internal(format!("export size: {e}")))?
        .len();
    Ok(export)
}

pub fn csv(permit: &HeavyPermit, services: &Services) -> Result<String> {
    Ok(flash_store::export::build_csv(
        &services.export_cards(permit.user())?,
    ))
}

/// `flash-2026-08-31.apkg` — the attachment name both surfaces use.
pub fn file_name(ext: &str, now_ms: i64) -> String {
    format!("flash-{}.{ext}", crate::web::format_day(now_ms))
}

/// The package as a download: streamed from disk with its length known,
/// the file deleted when the body is dropped (finished or abandoned).
pub async fn attachment_stream(export: ExportFile, content_type: &str, name: String) -> Response {
    let file = match tokio::fs::File::open(&export.path).await {
        Ok(file) => file,
        Err(e) => {
            tracing::error!("export reopen: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response();
        }
    };
    let size = export.size;
    let reader = GuardedFile {
        file,
        _guard: export,
    };
    let mut response = Body::from_stream(tokio_util::io::ReaderStream::new(reader)).into_response();
    let h = response.headers_mut();
    for (key, value) in [
        (header::CONTENT_TYPE, content_type.to_string()),
        (header::CONTENT_LENGTH, size.to_string()),
        (
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{name}\""),
        ),
    ] {
        if let Ok(v) = value.parse() {
            h.insert(key, v);
        }
    }
    response
}

/// A file being sent, holding the guard that deletes it once the reader
/// is dropped.
struct GuardedFile {
    file: tokio::fs::File,
    _guard: ExportFile,
}

impl AsyncRead for GuardedFile {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().file).poll_read(cx, buf)
    }
}

/// Removes packages older than `STALE_EXPORT_MS`: downloads that a crash
/// or a killed process left behind. Live downloads are younger than that.
pub fn sweep_stale_exports(dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age.as_millis() > STALE_EXPORT_MS);
        if stale && std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}
