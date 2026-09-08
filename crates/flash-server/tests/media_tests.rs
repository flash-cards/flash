//! Media ingest and serving: the validation gate (magic bytes, SVG ban,
//! per-file cap), content-addressed blob storage on every plan, byte-range
//! serving, and the hardened authenticated serving route.

mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use common::*;
use flash_server::media::{ingest_media, media_dir, validate_media};
use flash_server::media_store::{DiskStore, MediaStore};
use flash_server::service::{now_ms, Services};
use flash_store::Store;
use tower::ServiceExt;

const PNG: &[u8] = &[
    0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0, 1, 2, 3,
];

struct Harness {
    app: axum::Router,
    store: Arc<Store>,
    services: Services,
    media: DiskStore,
    user: flash_core::UserId,
    session_cookie: String,
    data_dir: std::path::PathBuf,
}

fn harness(tag: &str) -> Harness {
    let t = AppBuilder::new(&format!("media-http-{tag}")).build();
    let (user, session_cookie) = signed_in(&t.store, "Med", "med@example.com");
    Harness {
        app: t.app,
        store: t.store,
        services: t.services,
        media: DiskStore::new(media_dir(&t.data_dir)),
        user,
        session_cookie,
        data_dir: t.data_dir,
    }
}

// ---- validation gate ----

#[test]
fn validation_rejects_masquerading_and_scriptable_files() {
    // HTML pretending to be an image: the stored-XSS vector. Dead here.
    assert!(
        validate_media("evil.jpg", b"<html><script>alert(1)</script></html>")
            .unwrap_err()
            .contains("does not match")
    );
    // SVG can script; not accepted at all.
    assert!(validate_media("chart.svg", b"<svg xmlns='...'></svg>")
        .unwrap_err()
        .contains("not supported"));
    assert!(validate_media("x.exe", PNG)
        .unwrap_err()
        .contains("not supported"));
    assert!(validate_media("empty.png", b"").is_err());

    let ok = validate_media("../../../etc/cat.png", PNG).unwrap();
    assert_eq!(ok.filename, "cat.png", "path bits stripped");
    assert_eq!(ok.mime, "image/png");
    assert_eq!(ok.sha256.len(), 64);
}

#[test]
fn validation_enforces_the_per_file_size_cap() {
    assert_eq!(
        flash_store::media::MAX_FILE_BYTES,
        100 * 1024 * 1024,
        "AnkiWeb parity"
    );
    let mut big = PNG.to_vec();
    big.resize((flash_store::media::MAX_FILE_BYTES + 1) as usize, 0);
    assert!(validate_media("big.png", &big)
        .unwrap_err()
        .contains("limit"));
}

// ---- ingest pipeline ----

#[test]
fn ingest_is_universal_and_content_addressed() {
    let h = harness("ingest");

    // A brand-new free account stores media: it's a native feature.
    let id = ingest_media(
        &h.services,
        &h.media,
        h.user,
        false,
        "cat.png",
        PNG,
        now_ms(),
    )
    .unwrap();
    let again = ingest_media(
        &h.services,
        &h.media,
        h.user,
        false,
        "cat.png",
        PNG,
        now_ms(),
    )
    .unwrap();
    assert_eq!(id, again, "idempotent for identical files");
    let admin_id = ingest_media(
        &h.services,
        &h.media,
        h.user,
        true,
        "cat.png",
        PNG,
        now_ms(),
    )
    .unwrap();
    assert_eq!(id, admin_id, "same file, same row");

    // Blob under its sha256, not its filename.
    let row = h.store.get_media(h.user, id).unwrap().unwrap();
    let blob = media_dir(&h.data_dir)
        .join(&row.sha256[..2])
        .join(&row.sha256);
    assert_eq!(std::fs::read(&blob).unwrap(), PNG);
    assert_eq!(h.store.media_bytes_used(h.user).unwrap(), PNG.len() as u64);

    // Deleting the last reference removes the blob.
    flash_server::media::delete_media(&h.services, &h.media, h.user, id).unwrap();
    assert!(!blob.exists());
    let _ = std::fs::remove_dir_all(&h.data_dir);
}

#[test]
fn soft_cap_is_an_abuse_valve_only() {
    let h = harness("softcap");
    // Fake a user already sitting at the valve: rows count, blobs needn't exist.
    let cap = flash_store::media::SOFT_CAP_BYTES;
    h.store
        .create_media(
            h.user,
            &"a".repeat(64),
            "huge.mp4",
            "video/mp4",
            flash_store::media::MediaKind::Video,
            cap,
            now_ms(),
        )
        .unwrap();
    let err = ingest_media(
        &h.services,
        &h.media,
        h.user,
        false,
        "cat.png",
        PNG,
        now_ms(),
    )
    .unwrap_err();
    let err = err.to_string();
    assert!(err.contains("limit reached"), "{err}");
    assert!(!err.contains("Pro"), "never framed as a plan matter: {err}");
    // Admins are exempt.
    ingest_media(
        &h.services,
        &h.media,
        h.user,
        true,
        "cat.png",
        PNG,
        now_ms(),
    )
    .unwrap();
    let _ = std::fs::remove_dir_all(&h.data_dir);
}

// ---- serving route ----

async fn get_media(
    h: &Harness,
    id: i64,
    cookie: Option<&str>,
    range: Option<&str>,
) -> (StatusCode, Vec<(String, String)>, Vec<u8>) {
    let mut req = Request::builder().uri(format!("{BASE}/media/{id}"));
    if let Some(c) = cookie {
        req = req.header(header::COOKIE, c);
    }
    if let Some(r) = range {
        req = req.header(header::RANGE, r);
    }
    let response = h
        .app
        .clone()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response
        .headers()
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    let bytes = axum::body::to_bytes(response.into_body(), 10_000_000)
        .await
        .unwrap();
    (status, headers, bytes.to_vec())
}

fn header_of<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

#[tokio::test]
async fn media_serving_is_owner_only_and_hardened() {
    let h = harness("serve");
    let id = ingest_media(
        &h.services,
        &h.media,
        h.user,
        false,
        "cat.png",
        PNG,
        now_ms(),
    )
    .unwrap();

    // Owner: 200 with the hardening headers.
    let (status, headers, body) = get_media(&h, id.0, Some(&h.session_cookie), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, PNG);
    assert_eq!(header_of(&headers, "content-type"), Some("image/png"));
    assert_eq!(
        header_of(&headers, "x-content-type-options"),
        Some("nosniff")
    );
    let csp = header_of(&headers, "content-security-policy").unwrap();
    assert!(csp.contains("default-src 'none'"), "{csp}");
    assert!(header_of(&headers, "cache-control")
        .unwrap()
        .contains("private"));
    assert_eq!(header_of(&headers, "accept-ranges"), Some("bytes"));

    // Another logged-in user: indistinguishable from nonexistent.
    let intruder = h.store.create_user("I", None, "member", now_ms()).unwrap();
    let cookie = web_session(&h.store, intruder);
    let (status, _, _) = get_media(&h, id.0, Some(&cookie), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Unknown id for the owner: 404.
    let (status, _, _) = get_media(&h, 424242, Some(&h.session_cookie), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Anonymous: no content served.
    let (status, _, _) = get_media(&h, id.0, None, None).await;
    assert_ne!(status, StatusCode::OK);
    let _ = std::fs::remove_dir_all(&h.data_dir);
}

/// With a store that hands out presigned URLs (the hosted bucket), the
/// web route authorizes and redirects; the API's byte route still sends
/// bytes (a native downloader would carry the bearer through a redirect)
/// and tells the app where to fetch directly; HEAD answers from the
/// record; the page policy names the store's origin.
#[tokio::test]
async fn presigning_stores_redirect_after_authorizing() {
    let dir = data_dir("media-presign");
    let t = AppBuilder::new("media-presign")
        .media(Arc::new(PresigningStore::new(media_dir(&dir))))
        .build();
    let (user, cookie) = signed_in(&t.store, "Med", "med@example.com");
    let bearer = api_bearer(&t.store, user);
    let disk = DiskStore::new(media_dir(&dir));
    let id = ingest_media(&t.services, &disk, user, false, "cat.png", PNG, now_ms()).unwrap();
    let sha = t.store.get_media(user, id).unwrap().unwrap().sha256;

    // Web: 302 to the store, cached briefly, nothing sniffable, no body.
    let r = send(&t.app, get(&format!("/media/{}", id.0), Some(&cookie))).await;
    assert_eq!(r.status, StatusCode::FOUND);
    assert_eq!(
        r.location(),
        format!("{PRESIGN_ORIGIN}/{sha}?sig=test&mime=image/png&ttl=900")
    );
    assert_eq!(r.header("cache-control"), Some("private, max-age=600"));
    assert_eq!(r.header("x-content-type-options"), Some("nosniff"));
    assert!(r.bytes.is_empty());
    // A ranged request redirects too; the store honours Range itself.
    let ranged = send(
        &t.app,
        Request::get(format!("/media/{}", id.0))
            .header(header::COOKIE, &cookie)
            .header(header::RANGE, "bytes=0-3")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(ranged.status, StatusCode::FOUND);
    // HEAD: the record, no redirect, no blob.
    let head = send(
        &t.app,
        Request::head(format!("/media/{}", id.0))
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(head.status, StatusCode::OK);
    assert_eq!(head.header("content-type"), Some("image/png"));
    assert_eq!(head.header("content-length"), Some(&*PNG.len().to_string()));

    // Authorization comes first: a stranger and another user get no URL.
    let anon = send(&t.app, get(&format!("/media/{}", id.0), None)).await;
    assert_eq!(anon.status, StatusCode::SEE_OTHER);
    let other = member(&t.store, "Other", "other@example.com");
    let other_cookie = web_session(&t.store, other);
    let foreign = send(
        &t.app,
        get(&format!("/media/{}", id.0), Some(&other_cookie)),
    )
    .await;
    assert_eq!(foreign.status, StatusCode::NOT_FOUND);

    // API: bytes, never a redirect; and the location endpoint.
    let api = send(
        &t.app,
        json_get(&format!("/api/v1/media/{}", id.0), Some(&bearer)),
    )
    .await;
    assert_eq!(api.status, StatusCode::OK);
    assert_eq!(api.bytes, PNG);
    let location = send(
        &t.app,
        json_get(&format!("/api/v1/media/{}/url", id.0), Some(&bearer)),
    )
    .await;
    assert_eq!(location.status, StatusCode::OK);
    let json = location.json();
    assert_eq!(
        json["url"],
        format!("{PRESIGN_ORIGIN}/{sha}?sig=test&mime=image/png&ttl=900")
    );
    assert_eq!(json["mime"], "image/png");
    assert_eq!(json["expires_in"], 900);
    let foreign_bearer = api_bearer(&t.store, other);
    let foreign_location = send(
        &t.app,
        json_get(
            &format!("/api/v1/media/{}/url", id.0),
            Some(&foreign_bearer),
        ),
    )
    .await;
    assert_eq!(foreign_location.status, StatusCode::NOT_FOUND);

    // The page policy names the origin, so the redirect targets load.
    let page = send(&t.app, get("/decks", Some(&cookie))).await;
    let csp = page.header("content-security-policy").unwrap();
    assert!(
        csp.contains(&format!("img-src 'self' data: {PRESIGN_ORIGIN}")),
        "{csp}"
    );
    assert!(
        csp.contains(&format!("media-src 'self' {PRESIGN_ORIGIN}")),
        "{csp}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Without presigning (the disk store), the location endpoint says so
/// and the page policy stays self-only.
#[tokio::test]
async fn disk_stores_report_no_direct_url() {
    let h = harness("no-presign");
    let bearer = api_bearer(&h.store, h.user);
    let id = ingest_media(
        &h.services,
        &h.media,
        h.user,
        false,
        "cat.png",
        PNG,
        now_ms(),
    )
    .unwrap();
    let location = send(
        &h.app,
        json_get(&format!("/api/v1/media/{}/url", id.0), Some(&bearer)),
    )
    .await;
    assert_eq!(location.status, StatusCode::OK);
    assert!(location.json()["url"].is_null());
    assert_eq!(location.json()["mime"], "image/png");
    let page = send(&h.app, get("/decks", Some(&h.session_cookie))).await;
    let csp = page.header("content-security-policy").unwrap();
    assert!(csp.contains("img-src 'self' data:;"), "{csp}");
    assert!(csp.contains("media-src 'self';"), "{csp}");
    let _ = std::fs::remove_dir_all(&h.data_dir);
}

#[tokio::test]
async fn byte_ranges_serve_partial_content() {
    let h = harness("range");
    let id = ingest_media(
        &h.services,
        &h.media,
        h.user,
        false,
        "cat.png",
        PNG,
        now_ms(),
    )
    .unwrap();
    let total = PNG.len();

    let (status, headers, body) =
        get_media(&h, id.0, Some(&h.session_cookie), Some("bytes=0-3")).await;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(body, &PNG[..4]);
    assert_eq!(
        header_of(&headers, "content-range"),
        Some(format!("bytes 0-3/{total}").as_str())
    );
    assert_eq!(header_of(&headers, "content-type"), Some("image/png"));

    let (status, headers, body) =
        get_media(&h, id.0, Some(&h.session_cookie), Some("bytes=-2")).await;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(body, &PNG[total - 2..]);
    assert_eq!(
        header_of(&headers, "content-range"),
        Some(format!("bytes {}-{}/{total}", total - 2, total - 1).as_str())
    );

    let (status, _, _) = get_media(&h, id.0, Some(&h.session_cookie), Some("bytes=999-")).await;
    assert_eq!(status, StatusCode::RANGE_NOT_SATISFIABLE);

    // Junk range units are ignored, not errors.
    let (status, _, body) = get_media(&h, id.0, Some(&h.session_cookie), Some("items=0-1")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, PNG);
    let _ = std::fs::remove_dir_all(&h.data_dir);
}

#[tokio::test]
async fn shared_blob_survives_one_users_delete() {
    let h = harness("shared");
    let other = h.store.create_user("O", None, "member", now_ms()).unwrap();

    let mine = ingest_media(
        &h.services,
        &h.media,
        h.user,
        false,
        "cat.png",
        PNG,
        now_ms(),
    )
    .unwrap();
    let theirs = ingest_media(
        &h.services,
        &h.media,
        other,
        false,
        "cat.png",
        PNG,
        now_ms(),
    )
    .unwrap();
    let sha = h.store.get_media(h.user, mine).unwrap().unwrap().sha256;
    let blob = media_dir(&h.data_dir).join(&sha[..2]).join(&sha);
    assert!(blob.exists());

    flash_server::media::delete_media(&h.services, &h.media, h.user, mine).unwrap();
    assert!(blob.exists(), "other user still references the blob");
    flash_server::media::delete_media(&h.services, &h.media, other, theirs).unwrap();
    assert!(!blob.exists(), "last reference removes the blob");
    let _ = std::fs::remove_dir_all(&h.data_dir);
}

// ---- deck deletion GC + export bundling ----

#[test]
fn deck_deletion_drops_unreferenced_media_and_reports_orphans() {
    let h = harness("deckgc");
    let deck = h.store.create_deck(h.user, "Pics", "", now_ms()).unwrap();
    let cards = h
        .store
        .create_cards(
            h.user,
            deck,
            &[(flash_core::validate_card_text("f", "b").unwrap(), vec![])],
            None,
            now_ms(),
        )
        .unwrap();
    // Uploaded two hours ago: the deck GC leaves rows younger than an
    // hour alone (an upload another tab is about to link), and only the
    // daily sweep takes those if they never get linked.
    let id = ingest_media(
        &h.services,
        &h.media,
        h.user,
        false,
        "cat.png",
        PNG,
        now_ms() - 2 * 3_600_000,
    )
    .unwrap();
    h.store.link_card_media(h.user, cards[0], id).unwrap();
    let sha = h.store.get_media(h.user, id).unwrap().unwrap().sha256;

    let deleted = h.services.delete_deck(h.user, deck).unwrap();
    assert_eq!(deleted.cards, 1);
    assert_eq!(deleted.orphan_blobs, vec![sha.clone()]);
    assert!(
        h.store.get_media(h.user, id).unwrap().is_none(),
        "media row gone"
    );
    assert_eq!(
        h.store.media_bytes_used(h.user).unwrap(),
        0,
        "no phantom usage"
    );
    // The caller removes the blob (as the web/MCP handlers do via remove_orphans).
    h.media.delete(&sha).unwrap();
    assert!(!media_dir(&h.data_dir).join(&sha[..2]).join(&sha).exists());
    let _ = std::fs::remove_dir_all(&h.data_dir);
}

#[test]
fn export_bundles_media_with_anki_markup() {
    let h = harness("export");
    let deck = h.store.create_deck(h.user, "Pics", "", now_ms()).unwrap();
    let id = ingest_media(
        &h.services,
        &h.media,
        h.user,
        false,
        "cat.png",
        PNG,
        now_ms(),
    )
    .unwrap();
    let html = format!(r#"<p>A cat <img src="/media/{}" alt="cat.png"></p>"#, id.0);
    let rows = vec![flash_store::import::ImportRow {
        reviews: vec![],
        media: vec![flash_store::media::MediaRef {
            filename: "cat.png".into(),
            kind: flash_store::media::MediaKind::Image,
        }],
        front: "A cat".into(),
        back: "meow".into(),
        front_html: Some(flash_store::richtext::sanitize_with_media(&html)),
        back_html: None,
        tags: vec![],
        deck: None,
        suspended: false,
        cloze_text: None,
        cloze_index: None,
        type_answer: None,
    }];
    let resolve: std::collections::HashMap<String, flash_core::MediaId> =
        [("cat.png".to_string(), id)].into_iter().collect();
    let _ = deck;
    h.services
        .import_cards(h.user, "Pics", rows, false, Some(&resolve), now_ms())
        .unwrap();

    let cards = h.services.export_cards(h.user).unwrap();
    let plan = flash_server::media::plan_export_media(&h.services, h.user);
    assert!(!plan.truncated);
    assert_eq!(plan.media.entries.len(), 1);
    assert_eq!(plan.media.entries[0].name, "cat.png");
    assert_eq!(plan.media.entries[0].size, PNG.len() as u64);
    // The builder pulls each blob through the store right before writing it.
    let mut reads = 0;
    let mut read = |entry: &flash_store::export::ExportMediaEntry| {
        reads += 1;
        h.media
            .get(&entry.sha256, None)
            .map(|b| b.bytes)
            .map_err(|e| e.to_string())
    };
    let apkg = flash_store::export::build_apkg_into(
        &cards,
        &plan.media,
        &mut read,
        now_ms(),
        std::io::Cursor::new(Vec::new()),
    )
    .unwrap()
    .into_inner();
    assert_eq!(reads, 1);

    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(apkg)).unwrap();
    let manifest: serde_json::Value = {
        let mut f = zip.by_name("media").unwrap();
        let mut s = String::new();
        std::io::Read::read_to_string(&mut f, &mut s).unwrap();
        serde_json::from_str(&s).unwrap()
    };
    assert_eq!(manifest["0"], "cat.png");
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut zip.by_name("0").unwrap(), &mut bytes).unwrap();
    assert_eq!(bytes, PNG);
    // And the note's HTML points at the filename, not /media/N.
    let db_bytes = {
        let mut b = Vec::new();
        std::io::Read::read_to_end(&mut zip.by_name("collection.anki2").unwrap(), &mut b).unwrap();
        b
    };
    let path =
        std::env::temp_dir().join(format!("flash-export-check-{}.anki2", std::process::id()));
    std::fs::write(&path, db_bytes).unwrap();
    let conn = rusqlite::Connection::open(&path).unwrap();
    let flds: String = conn
        .query_row("SELECT flds FROM notes", [], |r| r.get(0))
        .unwrap();
    assert!(
        flds.contains(r#"<img src="cat.png" alt="cat.png">"#),
        "{flds}"
    );
    assert!(!flds.contains("/media/"), "{flds}");
    drop(conn);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&h.data_dir);
}

/// Over HTTP the package is built on disk and streamed with its length
/// known; the file is gone once the body has been consumed.
#[tokio::test]
async fn export_streams_from_disk_and_leaves_nothing_behind() {
    let h = harness("export-stream");
    let mut resolve = std::collections::HashMap::new();
    let mut html = String::new();
    for (i, name) in ["one.png", "two.png", "three.png"].iter().enumerate() {
        let mut bytes = PNG.to_vec();
        bytes.extend(std::iter::repeat_n(i as u8, 100 * (i + 1)));
        let id =
            ingest_media(&h.services, &h.media, h.user, false, name, &bytes, now_ms()).unwrap();
        html.push_str(&format!(r#"<img src="/media/{}" alt="{name}">"#, id.0));
        resolve.insert(name.to_string(), id);
    }
    let mut row = rich_row("Three pics", html, "one.png");
    row.media = ["one.png", "two.png", "three.png"]
        .iter()
        .map(|name| flash_store::media::MediaRef {
            filename: name.to_string(),
            kind: flash_store::media::MediaKind::Image,
        })
        .collect();
    h.services
        .import_cards(h.user, "Pics", vec![row], false, Some(&resolve), now_ms())
        .unwrap();

    let exports = flash_server::flows::export_flow::exports_dir_in(&h.data_dir);
    let response = h
        .app
        .clone()
        .oneshot(
            Request::get(format!("{BASE}/export.apkg"))
                .header(header::COOKIE, &h.session_cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let declared: usize = response
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .expect("a streamed export still declares its length");
    assert!(
        response.headers()[header::CONTENT_DISPOSITION]
            .to_str()
            .unwrap()
            .starts_with("attachment; filename=\"flash-"),
        "download, never inline"
    );
    // The file exists while the body is being sent...
    let pending: Vec<_> = std::fs::read_dir(&exports)
        .map(|d| d.flatten().collect())
        .unwrap_or_default();
    assert_eq!(pending.len(), 1, "one package on disk during the download");
    let body = axum::body::to_bytes(response.into_body(), 50_000_000)
        .await
        .unwrap();
    assert_eq!(body.len(), declared);
    // ...and is removed once the body has been consumed.
    let left: Vec<_> = std::fs::read_dir(&exports)
        .map(|d| d.flatten().collect())
        .unwrap_or_default();
    assert!(left.is_empty(), "{left:?}");

    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(body.to_vec())).unwrap();
    let manifest: serde_json::Value = {
        let mut f = zip.by_name("media").unwrap();
        let mut s = String::new();
        std::io::Read::read_to_string(&mut f, &mut s).unwrap();
        serde_json::from_str(&s).unwrap()
    };
    let mut names: Vec<_> = manifest
        .as_object()
        .unwrap()
        .values()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    names.sort();
    assert_eq!(names, ["one.png", "three.png", "two.png"]);
    for i in 0..3 {
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut zip.by_name(&i.to_string()).unwrap(), &mut bytes).unwrap();
        assert!(bytes.starts_with(PNG));
    }
    let _ = std::fs::remove_dir_all(&h.data_dir);
}

/// A package a crash left behind is swept by housekeeping; one a slow
/// client is still downloading is not.
#[test]
fn housekeeping_sweeps_abandoned_exports_only() {
    let h = harness("export-sweep");
    let exports = flash_server::flows::export_flow::exports_dir_in(&h.data_dir);
    std::fs::create_dir_all(&exports).unwrap();
    let stale = exports.join("1-stale.apkg");
    let fresh = exports.join("1-fresh.apkg");
    std::fs::write(&stale, b"old").unwrap();
    std::fs::write(&fresh, b"new").unwrap();
    let two_hours_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 60 * 60);
    std::fs::File::options()
        .write(true)
        .open(&stale)
        .unwrap()
        .set_modified(two_hours_ago)
        .unwrap();

    flash_server::scheduler::housekeeping(&h.services, &h.media, &h.data_dir, now_ms());

    assert!(!stale.exists(), "abandoned package removed");
    assert!(fresh.exists(), "live download kept");
    // A missing directory (no export ever ran) is not an error.
    assert_eq!(
        flash_server::flows::export_flow::sweep_stale_exports(&h.data_dir.join("nowhere")),
        0
    );
    let _ = std::fs::remove_dir_all(&h.data_dir);
}

// ---- study screen warm-up ----

fn rich_row(front: &str, back_html: String, media: &str) -> flash_store::import::ImportRow {
    flash_store::import::ImportRow {
        reviews: vec![],
        media: vec![flash_store::media::MediaRef {
            filename: media.into(),
            kind: flash_store::media::MediaKind::Image,
        }],
        front: front.into(),
        back: "back".into(),
        front_html: None,
        back_html: Some(flash_store::richtext::sanitize_with_media(&back_html)),
        tags: vec![],
        deck: None,
        suspended: false,
        cloze_text: None,
        cloze_index: None,
        type_answer: None,
    }
}

async fn study_request(h: &Harness, method: &str, path: &str, body: &str) -> String {
    let mut req = Request::builder()
        .method(method)
        .uri(format!("{BASE}{path}"))
        .header(header::COOKIE, &h.session_cookie)
        .header("sec-fetch-site", "same-origin");
    if method == "POST" {
        req = req.header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
    }
    let response = h
        .app
        .clone()
        .oneshot(req.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK, "{method} {path}");
    let bytes = axum::body::to_bytes(response.into_body(), 10_000_000)
        .await
        .unwrap();
    String::from_utf8_lossy(&bytes).into_owned()
}

fn hx_value(html: &str, key: &str) -> String {
    let marker = format!("\"{key}\": ");
    let at = html
        .find(&marker)
        .unwrap_or_else(|| panic!("{key} in {html}"))
        + marker.len();
    html[at..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect()
}

#[tokio::test]
async fn study_front_preloads_its_back_and_prefetches_the_next_card() {
    let h = harness("warmup");
    let a = ingest_media(&h.services, &h.media, h.user, false, "a.png", PNG, now_ms()).unwrap();
    let mut other = PNG.to_vec();
    other.push(0xFF);
    let b = ingest_media(
        &h.services,
        &h.media,
        h.user,
        false,
        "b.png",
        &other,
        now_ms(),
    )
    .unwrap();
    let resolve: std::collections::HashMap<String, flash_core::MediaId> =
        [("a.png".to_string(), a), ("b.png".to_string(), b)]
            .into_iter()
            .collect();
    h.services
        .import_cards(
            h.user,
            "Pics",
            vec![
                rich_row(
                    "one",
                    format!(r#"<p><img src="/media/{}" alt="a.png"></p>"#, a.0),
                    "a.png",
                ),
                rich_row(
                    "two",
                    format!(r#"<p><img src="/media/{}" alt="b.png"></p>"#, b.0),
                    "b.png",
                ),
            ],
            false,
            Some(&resolve),
            now_ms(),
        )
        .unwrap();

    // Front of card one: its own back preloaded, card two prefetched, back hidden.
    let page = study_request(&h, "GET", "/study", "").await;
    assert!(
        page.contains(&format!(
            r#"<link rel="preload" as="image" href="/media/{}">"#,
            a.0
        )),
        "{page}"
    );
    assert!(
        page.contains(&format!(r#"<link rel="prefetch" href="/media/{}">"#, b.0)),
        "{page}"
    );
    assert!(
        !page.contains(&format!(r#"<img src="/media/{}""#, a.0)),
        "back stays hidden until reveal"
    );
    let session = hx_value(&page, "session");
    let card = hx_value(&page, "card");

    // Reveal: the image is in the markup now; no preload hint repeated.
    let revealed = study_request(
        &h,
        "POST",
        "/study/reveal",
        &format!("session={session}&card={card}&remaining=2&total=2"),
    )
    .await;
    assert!(
        revealed.contains(&format!(r#"<img src="/media/{}""#, a.0)),
        "{revealed}"
    );
    assert!(!revealed.contains(r#"rel="preload""#), "{revealed}");

    // Grade: card two's front preloads its back; nothing left to prefetch.
    let next = study_request(
        &h,
        "POST",
        "/study/review",
        &format!("session={session}&card={card}&rating=3&total=2"),
    )
    .await;
    assert!(
        next.contains(&format!(
            r#"<link rel="preload" as="image" href="/media/{}">"#,
            b.0
        )),
        "{next}"
    );
    assert!(!next.contains(r#"rel="prefetch""#), "{next}");
    let _ = std::fs::remove_dir_all(&h.data_dir);
}
