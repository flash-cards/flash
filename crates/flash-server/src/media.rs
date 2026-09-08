//! Media ingest, deletion, export bundling, and serving. Blob bytes live in
//! a `MediaStore` (local disk, or an S3-compatible bucket), named by their
//! sha256 (content-addressed): a hostile filename from an .apkg never
//! becomes a path or key, identical files dedupe across users, and a blob
//! is removed only when the last DB row naming its hash goes away.
//!
//! Media is a native feature on every plan. The only storage rule is a
//! silent per-user abuse valve (`MEDIA_SOFT_CAP_BYTES`); nothing in the UI
//! shows a quota, matching AnkiWeb.
//!
//! Serving hardening: every response is ownership-scoped, sends only an
//! allowlisted MIME decided at ingest (from magic bytes, never from the
//! request), carries nosniff + a deny-all CSP so even a hostile blob can't
//! script in our origin, and honours single byte ranges so video seeks and
//! a 100 MiB file never has to be buffered whole for a scrub.

use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;

use axum::extract::{Multipart, Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use flash_core::{MediaId, UserId};
use flash_store::export::{ExportMediaEntry, ExportMediaPlan};
use flash_store::media::{
    classify, content_matches, sanitize_filename, MediaKind,
    MAX_FILE_BYTES as MEDIA_MAX_FILE_BYTES, SOFT_CAP_BYTES as MEDIA_SOFT_CAP_BYTES,
};
use sha2::{Digest, Sha256};

use crate::auth::AnyUser;
use crate::flows::Failure;
use crate::media_store::{hex, ByteRange, MediaStore, MediaStoreError};
use crate::service::Services;
use crate::state::{AppState, HeavyPermit};

/// Default on-disk root when no bucket is configured.
pub fn media_dir(data_dir: &FsPath) -> PathBuf {
    data_dir.join("media")
}

#[derive(Debug)]
pub struct ValidatedMedia {
    pub filename: String,
    pub kind: MediaKind,
    pub mime: &'static str,
    pub sha256: String,
}

/// The gate every ingested file passes: sane filename, a format we accept
/// (SVG and unknowns are rejected — scriptable content must never be
/// served from our origin), size caps, and magic bytes that actually match
/// the claimed format so HTML-as-.jpg dies here.
pub fn validate_media(filename: &str, bytes: &[u8]) -> Result<ValidatedMedia, String> {
    let filename = sanitize_filename(filename);
    let (kind, mime) = classify(&filename);
    if kind == MediaKind::Unsupported {
        return Err(format!("{filename}: format not supported"));
    }
    if bytes.is_empty() {
        return Err(format!("{filename}: empty file"));
    }
    if bytes.len() as u64 > MEDIA_MAX_FILE_BYTES {
        return Err(format!(
            "{filename}: over the {} MB per-file limit",
            MEDIA_MAX_FILE_BYTES / (1024 * 1024)
        ));
    }
    if !content_matches(kind, bytes) {
        return Err(format!("{filename}: content does not match its extension"));
    }
    let sha256 = hex(&Sha256::digest(bytes));
    Ok(ValidatedMedia {
        filename,
        kind,
        mime,
        sha256,
    })
}

/// Full ingest pipeline: abuse valve, content validation, blob write,
/// metadata row. Idempotent for identical files.
pub fn ingest_media(
    services: &Services,
    store: &dyn MediaStore,
    user: UserId,
    is_admin: bool,
    filename: &str,
    bytes: &[u8],
    now_ms: i64,
) -> Result<MediaId, Failure> {
    let validated = validate_media(filename, bytes).map_err(Failure::User)?;
    if !is_admin {
        if services.store().media_count(user)? >= crate::bounds::MEDIA_OBJECTS_PER_USER {
            tracing::warn!(user = user.raw(), "media object cap reached");
            return Err(Failure::user(
                "media file limit reached — remove some files first",
            ));
        }
        let used = services.store().media_bytes_used(user)?;
        if used.saturating_add(bytes.len() as u64) > MEDIA_SOFT_CAP_BYTES {
            tracing::warn!(
                user = user.raw(),
                used_mb = used / (1024 * 1024),
                "media soft cap reached"
            );
            return Err(Failure::user(
                "media storage limit reached — contact support",
            ));
        }
    }
    if !services
        .store()
        .blob_known(&validated.sha256)
        .unwrap_or(false)
    {
        store.put(&validated.sha256, validated.mime, bytes)?;
    }
    Ok(services.store().create_media(
        user,
        &validated.sha256,
        &validated.filename,
        validated.mime,
        validated.kind,
        bytes.len() as u64,
        now_ms,
    )?)
}

/// `POST /media` (multipart, field `file`): the editor's paste/drop
/// upload. Runs the same gate as import media and answers JSON the editor
/// turns into an `<img>`/`<audio>`/`<video>` element. The row is linked
/// to a card only once a note referencing it is saved.
/// The web editor's upload: session cookie or bearer, behind the
/// same-origin guard the web router applies.
pub async fn upload_media(
    State(state): State<AppState>,
    AnyUser { id: user, is_admin }: AnyUser,
    multipart: Multipart,
) -> Response {
    upload_media_for(state, user, is_admin, multipart).await
}

/// The API's upload: bearer only. `/api/v1` is mounted outside the
/// same-origin guard, so a handler there must never accept a cookie, or
/// a cross-site form could upload into the victim's account.
pub async fn upload_media_bearer(
    State(state): State<AppState>,
    crate::api::BearerUser {
        id: user, is_admin, ..
    }: crate::api::BearerUser,
    multipart: Multipart,
) -> Response {
    upload_media_for(state, user, is_admin, multipart).await
}

async fn upload_media_for(
    state: AppState,
    user: UserId,
    is_admin: bool,
    mut multipart: Multipart,
) -> Response {
    // See the import handlers: bound in-flight upload bodies before
    // reading this one.
    let Ok(_upload) = state.upload_semaphore.clone().try_acquire_owned() else {
        return upload_error(
            StatusCode::TOO_MANY_REQUESTS,
            "too many uploads at once; try again in a moment",
        );
    };
    let mut file: Option<(String, axum::body::Bytes)> = None;
    while let Ok(Some(field)) = multipart.next_field().await {
        if field.name() == Some("file") {
            let name = field.file_name().unwrap_or("upload").to_string();
            match field.bytes().await {
                Ok(bytes) => file = Some((name, bytes)),
                Err(e) => {
                    tracing::warn!(user = user.raw(), file = ?name, "editor upload failed: {e}");
                    return upload_error(e.status(), &format!("upload: {e}"));
                }
            }
        }
    }
    let Some((filename, bytes)) = file else {
        return upload_error(StatusCode::BAD_REQUEST, "no file in upload");
    };
    if filename.len() > crate::bounds::FILENAME {
        return upload_error(StatusCode::BAD_REQUEST, "the file's name is too long");
    }
    if bytes.len() as u64 > MEDIA_MAX_FILE_BYTES {
        return upload_error(StatusCode::PAYLOAD_TOO_LARGE, "file is too large");
    }
    let services = state.services.clone();
    let store = state.media.clone();
    let result = tokio::task::spawn_blocking(move || {
        let id = ingest_media(
            &services,
            store.as_ref(),
            user,
            is_admin,
            &filename,
            &bytes,
            crate::service::now_ms(),
        )?;
        let row = services
            .store()
            .get_media(user, id)?
            .ok_or_else(|| Failure::internal("media row vanished after insert"))?;
        Ok::<_, Failure>((id, row.kind))
    })
    .await;
    match result {
        Ok(Ok((id, kind))) => Json(serde_json::json!({
            "id": id.0,
            "kind": kind.as_str(),
        }))
        .into_response(),
        Ok(Err(Failure::User(msg))) => upload_error(StatusCode::BAD_REQUEST, &msg),
        Ok(Err(Failure::Internal(detail))) => {
            tracing::error!(user = user.raw(), "media upload failed: {detail}");
            upload_error(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
        }
        Err(e) => {
            tracing::error!("join: {e}");
            upload_error(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
        }
    }
}

fn upload_error(status: StatusCode, msg: &str) -> Response {
    (status, Json(serde_json::json!({ "error": msg }))).into_response()
}

/// Ingests every media file an import's cards actually reference, from
/// the uploaded .apkg. Extraction, validation, and the blob upload run on
/// a small worker pool (a bucket PUT is ~200 ms; a deck has hundreds),
/// then the metadata rows are written in one pass. Best-effort per file
/// (unsupported/oversized/corrupt files are skipped with a log line; their
/// chips stay chips). Returns filename -> media id for the ones that made it.
pub fn ingest_package_media(
    permit: &HeavyPermit,
    services: &Services,
    store: &dyn MediaStore,
    is_admin: bool,
    package: &[u8],
    rows: &[flash_store::import::ImportRow],
    now_ms: i64,
) -> std::collections::HashMap<String, MediaId> {
    let user = permit.user();
    // Two, not more: each worker holds a decompressed file (up to the
    // per-file cap) next to the whole package, and the process shares
    // 1 GiB with everything else.
    const WORKERS: usize = 2;
    let wanted: std::collections::HashSet<&str> = rows
        .iter()
        .flat_map(|r| r.media.iter())
        .map(|m| m.filename.as_str())
        .collect();
    let entries: Vec<_> = flash_store::import::read_media_manifest(package)
        .unwrap_or_default()
        .into_iter()
        .filter(|e| wanted.contains(e.filename.as_str()))
        .collect();

    // Abuse valve. The manifest's sizes are the package author's claim, so
    // the check runs on the bytes actually decompressed: an up-front pass
    // on the declared total, then a running total each worker adds to
    // before it writes anything.
    let used = if is_admin {
        0
    } else {
        services.store().media_bytes_used(user).unwrap_or(0)
    };
    let budget = if is_admin {
        u64::MAX
    } else {
        MEDIA_SOFT_CAP_BYTES.saturating_sub(used)
    };
    let declared: u64 = entries.iter().map(|e| e.size).sum();
    if declared > budget {
        tracing::warn!(
            user = user.raw(),
            used_mb = used / (1024 * 1024),
            "media soft cap reached"
        );
        return std::collections::HashMap::new();
    }
    // The object cap, for the same reason as the byte cap: a package of
    // a million tiny files is a volume's inodes, not its bytes.
    if !is_admin {
        let have = services.store().media_count(user).unwrap_or(0);
        if have.saturating_add(entries.len() as u64) > crate::bounds::MEDIA_OBJECTS_PER_USER {
            tracing::warn!(user = user.raw(), have, "media object cap reached");
            return std::collections::HashMap::new();
        }
    }
    let written = std::sync::atomic::AtomicU64::new(0);

    // Phase 1 (parallel): extract + validate + upload. Order is kept so
    // row ids are assigned deterministically.
    let next = std::sync::atomic::AtomicUsize::new(0);
    type Slot = parking_lot::Mutex<Option<Result<(ValidatedMedia, u64), String>>>;
    let results: Vec<Slot> = entries
        .iter()
        .map(|_| parking_lot::Mutex::new(None))
        .collect();
    std::thread::scope(|scope| {
        for _ in 0..WORKERS.min(entries.len().max(1)) {
            scope.spawn(|| loop {
                let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let Some(entry) = entries.get(i) else { break };
                let outcome =
                    flash_store::import::extract_media_file(package, entry, MEDIA_MAX_FILE_BYTES)
                        .map_err(|e| format!("extract: {e}"))
                        .and_then(|bytes| {
                            let v = validate_media(&entry.filename, &bytes)?;
                            let so_far = written.fetch_add(
                                bytes.len() as u64,
                                std::sync::atomic::Ordering::Relaxed,
                            );
                            if so_far.saturating_add(bytes.len() as u64) > budget {
                                return Err("media storage limit reached".to_string());
                            }
                            // Content-addressed: a blob any user already holds is
                            // already in the store — no upload needed.
                            if !services.store().blob_known(&v.sha256).unwrap_or(false) {
                                store
                                    .put(&v.sha256, v.mime, &bytes)
                                    .map_err(|e| e.to_string())?;
                            }
                            Ok((v, bytes.len() as u64))
                        });
                *results[i].lock() = Some(outcome);
            });
        }
    });

    // Phase 2 (sequential): metadata rows.
    let mut map = std::collections::HashMap::new();
    let mut failed = 0u32;
    for (entry, slot) in entries.iter().zip(results) {
        // A slot is only empty if a worker panicked mid-file; treat that
        // file as failed rather than propagating the panic.
        let outcome = slot
            .into_inner()
            .unwrap_or_else(|| Err("worker did not finish".to_string()));
        match outcome {
            Ok((v, size)) => match services.store().create_media(
                user,
                &v.sha256,
                &v.filename,
                v.mime,
                v.kind,
                size,
                now_ms,
            ) {
                Ok(id) => {
                    map.insert(entry.filename.clone(), id);
                }
                Err(e) => {
                    tracing::warn!(user = user.raw(), file = ?entry.filename, "media row failed: {e}");
                    failed += 1;
                }
            },
            Err(e) => {
                tracing::warn!(user = user.raw(), file = ?entry.filename, "media ingest failed: {e}");
                failed += 1;
            }
        }
    }
    let missing = wanted.len().saturating_sub(entries.len());
    if failed > 0 || missing > 0 {
        tracing::warn!(
            user = user.raw(),
            ingested = map.len(),
            failed,
            missing_from_package = missing,
            "media import incomplete"
        );
    } else {
        tracing::info!(
            user = user.raw(),
            ingested = map.len(),
            "media import complete"
        );
    }
    map
}

/// Deletes a media row and, when no row anywhere still references the
/// blob, the blob itself.
pub fn delete_media(
    services: &Services,
    store: &dyn MediaStore,
    user: UserId,
    id: MediaId,
) -> Result<(), String> {
    let row = services
        .store()
        .get_media(user, id)
        .map_err(|e| e.to_string())?
        .ok_or("media not found")?;
    let remaining = services
        .store()
        .delete_media(user, id)
        .map_err(|e| e.to_string())?;
    if remaining == 0 {
        store.delete(&row.sha256).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Removes blobs the store reported as unreferenced (deck or account
/// deletion) — in the background. The DB rows are already gone, so the
/// user's request never waits on network deletes (a big deck is hundreds
/// of them). Best effort: a failed delete is logged, never surfaced.
pub fn remove_orphans(services: Services, store: Arc<dyn MediaStore>, hashes: &[String]) {
    if hashes.is_empty() {
        return;
    }
    let hashes = hashes.to_vec();
    tokio::task::spawn_blocking(move || {
        let started = std::time::Instant::now();
        let mut failed = 0u32;
        for sha in &hashes {
            // An import that ran meanwhile may have adopted the blob (the
            // store is content-addressed and skips known uploads).
            if services.store().blob_known(sha).unwrap_or(true) {
                continue;
            }
            if let Err(e) = store.delete(sha) {
                failed += 1;
                tracing::warn!("orphan blob {sha}: {e}");
            }
        }
        tracing::info!(
            removed = hashes.len() as u32 - failed,
            failed,
            ms = started.elapsed().as_millis() as u64,
            "orphan media blobs removed"
        );
    });
}

/// Largest total of media one export bundles. It is a budget on the
/// package written to disk, not on memory (blobs pass through one at a
/// time); past it, the remaining refs export as plain filenames.
pub const EXPORT_MEDIA_MAX_TOTAL: u64 = 512 * 1024 * 1024;

/// What an .apkg export must bundle, decided from the rows alone: names
/// made unique per export (Anki keys media by name) and the blob hashes.
/// `truncated` says the budget cut the list short, so the caller can log
/// it once for the account.
pub struct ExportPlan {
    pub media: ExportMediaPlan,
    pub truncated: bool,
}

pub fn plan_export_media(services: &Services, user: UserId) -> ExportPlan {
    let mut plan = ExportPlan {
        media: ExportMediaPlan::default(),
        truncated: false,
    };
    let rows = match services.store().media_for_export(user) {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!("export media rows: {e}");
            return plan;
        }
    };
    let mut total = 0u64;
    let mut taken: std::collections::HashSet<String> = std::collections::HashSet::new();
    for row in rows {
        if total.saturating_add(row.size) > EXPORT_MEDIA_MAX_TOTAL {
            plan.truncated = true;
            break;
        }
        total += row.size;
        let name = unique_name(&row.filename, &taken);
        taken.insert(name.clone());
        plan.media.names.insert(row.id.0, name.clone());
        plan.media.entries.push(ExportMediaEntry {
            name,
            sha256: row.sha256,
            size: row.size,
        });
    }
    plan
}

/// `cat.png`, then `cat-2.png`, `cat-3.png`… when a user has different
/// files under the same name.
fn unique_name(filename: &str, taken: &std::collections::HashSet<String>) -> String {
    if !taken.contains(filename) {
        return filename.to_string();
    }
    let (stem, ext) = match filename.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s, format!(".{e}")),
        _ => (filename, String::new()),
    };
    (2..)
        .map(|n| format!("{stem}-{n}{ext}"))
        .find(|candidate| !taken.contains(candidate))
        .expect("unbounded")
}

/// Parses a single-range `Range` header. Multi-range and non-byte units
/// are ignored (full response), per RFC 9110's "may ignore" allowance.
pub fn parse_range(headers: &HeaderMap) -> Option<ByteRange> {
    let value = headers.get(header::RANGE)?.to_str().ok()?.trim();
    let spec = value.strip_prefix("bytes=")?.trim();
    if spec.contains(',') {
        return None;
    }
    let (start, end) = spec.split_once('-')?;
    if start.is_empty() {
        return end.parse().ok().map(ByteRange::Suffix);
    }
    let start: u64 = start.parse().ok()?;
    let end: Option<u64> = if end.is_empty() {
        None
    } else {
        Some(end.parse().ok()?)
    };
    Some(ByteRange::From { start, end })
}

/// GET /media/{id} — authenticated (session cookie or the app's bearer),
/// ownership-scoped (someone else's id is indistinguishable from a
/// nonexistent one). Content-addressed blobs never change, so the cache
/// header is immutable-private.
pub async fn serve_media(
    State(state): State<AppState>,
    method: axum::http::Method,
    AnyUser { id: user, .. }: AnyUser,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Response {
    // Browsers follow the redirect without carrying credentials, and the
    // page policy names the store's origin.
    serve_media_for(state, method, user, id, headers, true).await
}

/// Bearer-only twin for `/api/v1` (see `upload_media_bearer`).
pub async fn serve_media_bearer(
    State(state): State<AppState>,
    method: axum::http::Method,
    crate::api::BearerUser { id: user, .. }: crate::api::BearerUser,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Response {
    // Never a redirect here: a native downloader would follow it with the
    // bearer still attached. Apps that want the direct URL ask
    // `media_location` and fetch it without headers.
    serve_media_for(state, method, user, id, headers, false).await
}

/// How long a presigned media URL stays valid, and how long a client may
/// cache the redirect to it (shorter, so a cached redirect never points
/// at a dead URL).
pub const PRESIGN_TTL: std::time::Duration = std::time::Duration::from_secs(15 * 60);
pub const REDIRECT_MAX_AGE_SECS: u64 = 10 * 60;

/// Blobs at or above this size count against the server's own read
/// budget when it must send the bytes itself.
const LARGE_BLOB_BYTES: u64 = 4 * 1024 * 1024;

/// What serving a blob resolves to after authorization: a URL the client
/// fetches directly, or the bytes.
pub enum Served {
    Redirect(String),
    Bytes(crate::media_store::Blob),
    /// A HEAD: the record describes the blob, nothing is fetched.
    Described,
}

/// Resolves one authorized blob: the store's presigned URL when it has
/// one, else the bytes, with large reads counted against `media_reads`.
/// Runs on the blocking pool with the row lookup that authorized it.
pub fn resolve_blob(
    state: &AppState,
    sha256: &str,
    mime: &str,
    filename: &str,
    size: u64,
    range: Option<crate::media_store::ByteRange>,
    prefer_redirect: bool,
) -> Result<Served, StatusCode> {
    if prefer_redirect {
        if let Some(url) = state
            .media
            .presigned_get(sha256, mime, filename, PRESIGN_TTL)
        {
            return url
                .map(Served::Redirect)
                .map_err(|e| store_error_status(sha256, e));
        }
    }
    let _read = if size >= LARGE_BLOB_BYTES {
        Some(
            state
                .media_reads
                .clone()
                .try_acquire_owned()
                .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?,
        )
    } else {
        None
    };
    state
        .media
        .get(sha256, range)
        .map(Served::Bytes)
        .map_err(|e| store_error_status(sha256, e))
}

/// The 302 to a presigned URL: cached briefly, never sniffed, no body.
pub fn redirect_response(url: &str, cache_control: &str) -> Response {
    let mut response = StatusCode::FOUND.into_response();
    let h = response.headers_mut();
    if let Ok(v) = url.parse() {
        h.insert(header::LOCATION, v);
    }
    if let Ok(v) = cache_control.parse() {
        h.insert(header::CACHE_CONTROL, v);
    }
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        header::HeaderValue::from_static("nosniff"),
    );
    response
}

/// A HEAD's answer, from the record alone: the type and size a client
/// probes for, with no blob moved and no redirect to follow.
pub fn head_response(mime: &str, size: u64, cache_control: &str) -> Response {
    let mut response = StatusCode::OK.into_response();
    let h = response.headers_mut();
    for (name, value) in [
        (header::CONTENT_TYPE, mime.to_string()),
        (header::CONTENT_LENGTH, size.to_string()),
        (header::ACCEPT_RANGES, "bytes".to_string()),
        (header::CACHE_CONTROL, cache_control.to_string()),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
    ] {
        if let Ok(v) = value.parse() {
            h.insert(name, v);
        }
    }
    response
}

async fn serve_media_for(
    state: AppState,
    method: axum::http::Method,
    user: UserId,
    id: i64,
    headers: HeaderMap,
    prefer_redirect: bool,
) -> Response {
    let range = parse_range(&headers);
    let head = method == axum::http::Method::HEAD;
    let app = state.clone();
    let result = tokio::task::spawn_blocking(
        move || -> Result<(flash_store::MediaRow, Served), StatusCode> {
            let row = app
                .services
                .store()
                .get_media(user, MediaId(id))
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
                .ok_or(StatusCode::NOT_FOUND)?;
            if head {
                return Ok((row, Served::Described));
            }
            let served = resolve_blob(
                &app,
                &row.sha256,
                &row.mime,
                &row.filename,
                row.size,
                range,
                prefer_redirect,
            )?;
            Ok((row, served))
        },
    )
    .await;
    match result {
        Ok(Ok((_, Served::Redirect(url)))) => {
            redirect_response(&url, &format!("private, max-age={REDIRECT_MAX_AGE_SECS}"))
        }
        Ok(Ok((row, Served::Described))) => {
            head_response(&row.mime, row.size, "private, max-age=31536000, immutable")
        }
        Ok(Ok((row, Served::Bytes(blob)))) => blob_response(
            &row.mime,
            &row.filename,
            blob,
            range.is_some(),
            "private, max-age=31536000, immutable",
        ),
        Ok(Err(code)) => status_response(code),
        Err(e) => {
            tracing::error!("media join: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
        }
    }
}

/// `GET /api/v1/media/{id}/url`: where the app should fetch the blob
/// from. `url` is a presigned URL to download with no headers, or null
/// when this server sends bytes itself (download `/api/v1/media/{id}`
/// with the bearer instead).
pub async fn media_location(
    State(state): State<AppState>,
    crate::api::BearerUser { id: user, .. }: crate::api::BearerUser,
    Path(id): Path<i64>,
) -> Response {
    let app = state.clone();
    let result = tokio::task::spawn_blocking(move || -> Result<_, StatusCode> {
        let row = app
            .services
            .store()
            .get_media(user, MediaId(id))
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
            .ok_or(StatusCode::NOT_FOUND)?;
        let url = match app
            .media
            .presigned_get(&row.sha256, &row.mime, &row.filename, PRESIGN_TTL)
        {
            Some(Ok(url)) => Some(url),
            Some(Err(e)) => return Err(store_error_status(&row.sha256, e)),
            None => None,
        };
        Ok((row, url))
    })
    .await;
    match result {
        Ok(Ok((row, url))) => axum::Json(serde_json::json!({
            "url": url,
            "mime": row.mime,
            "filename": row.filename,
            "size": row.size,
            "expires_in": PRESIGN_TTL.as_secs(),
        }))
        .into_response(),
        Ok(Err(StatusCode::NOT_FOUND)) => crate::api::ApiError::not_found("media").into_response(),
        Ok(Err(_)) => crate::api::ApiError::internal().into_response(),
        Err(e) => {
            tracing::error!("media join: {e}");
            crate::api::ApiError::internal().into_response()
        }
    }
}

pub fn store_error_status(sha: &str, e: MediaStoreError) -> StatusCode {
    match e {
        MediaStoreError::NotFound => StatusCode::NOT_FOUND,
        MediaStoreError::RangeUnsatisfiable => StatusCode::RANGE_NOT_SATISFIABLE,
        MediaStoreError::Other(msg) => {
            tracing::error!("media get {sha}: {msg}");
            StatusCode::BAD_GATEWAY
        }
    }
}

/// A blob as an HTTP response: sandboxed, nosniff, immutable, with the
/// 206 framing when the request was ranged.
pub fn blob_response(
    mime: &str,
    filename: &str,
    blob: crate::media_store::Blob,
    ranged: bool,
    cache_control: &str,
) -> Response {
    let len = blob.bytes.len() as u64;
    let mut response = blob.bytes.into_response();
    let h = response.headers_mut();
    let set = |h: &mut HeaderMap, name: header::HeaderName, value: String| {
        if let Ok(v) = value.parse() {
            h.insert(name, v);
        }
    };
    set(h, header::CONTENT_TYPE, mime.to_string());
    set(h, header::X_CONTENT_TYPE_OPTIONS, "nosniff".into());
    set(
        h,
        header::CONTENT_SECURITY_POLICY,
        "default-src 'none'; sandbox".into(),
    );
    set(h, header::CACHE_CONTROL, cache_control.to_string());
    set(
        h,
        header::CONTENT_DISPOSITION,
        format!("inline; filename=\"{filename}\""),
    );
    set(h, header::ACCEPT_RANGES, "bytes".into());
    if ranged {
        let end = blob.start + len.saturating_sub(1);
        set(
            h,
            header::CONTENT_RANGE,
            format!("bytes {}-{}/{}", blob.start, end, blob.total),
        );
        *response.status_mut() = StatusCode::PARTIAL_CONTENT;
    }
    response
}

pub fn status_response(code: StatusCode) -> Response {
    if code == StatusCode::RANGE_NOT_SATISFIABLE {
        return (
            StatusCode::RANGE_NOT_SATISFIABLE,
            [(header::ACCEPT_RANGES, "bytes")],
            "range not satisfiable",
        )
            .into_response();
    }
    (code, "not found").into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_header_parsing() {
        let mut h = HeaderMap::new();
        assert_eq!(parse_range(&h), None);
        h.insert(header::RANGE, "bytes=0-99".parse().unwrap());
        assert_eq!(
            parse_range(&h),
            Some(ByteRange::From {
                start: 0,
                end: Some(99)
            })
        );
        h.insert(header::RANGE, "bytes=100-".parse().unwrap());
        assert_eq!(
            parse_range(&h),
            Some(ByteRange::From {
                start: 100,
                end: None
            })
        );
        h.insert(header::RANGE, "bytes=-500".parse().unwrap());
        assert_eq!(parse_range(&h), Some(ByteRange::Suffix(500)));
        h.insert(header::RANGE, "bytes=0-1,5-9".parse().unwrap());
        assert_eq!(parse_range(&h), None, "multi-range ignored");
        h.insert(header::RANGE, "items=0-1".parse().unwrap());
        assert_eq!(parse_range(&h), None);
    }

    #[test]
    fn export_names_stay_unique() {
        let mut taken = std::collections::HashSet::new();
        assert_eq!(unique_name("cat.png", &taken), "cat.png");
        taken.insert("cat.png".into());
        assert_eq!(unique_name("cat.png", &taken), "cat-2.png");
        taken.insert("cat-2.png".into());
        assert_eq!(unique_name("cat.png", &taken), "cat-3.png");
        taken.insert("noext".into());
        assert_eq!(unique_name("noext", &taken), "noext-2");
    }
}
