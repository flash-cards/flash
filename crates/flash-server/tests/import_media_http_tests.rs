//! End-to-end media import over HTTP with a real Anki package: a brand-new
//! free account uploads an .apkg that carries images, previews, commits,
//! and then sees its pictures served from /media/{id} and bundled back
//! into an export. Uses the local test fixture in target/deck-tests (built
//! by tools/deck-testing); skips quietly when it isn't present.

mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use common::*;
use flash_server::service::now_ms;
use flash_store::Store;

/// A real .apkg with referenced images, supplied by whoever runs the test:
/// `FLASH_MEDIA_FIXTURE_APKG=/path/to/deck.apkg cargo test ...`. Skipped
/// when unset, so CI and a fresh clone never depend on a particular deck.
fn fixture_path() -> Option<std::path::PathBuf> {
    std::env::var_os("FLASH_MEDIA_FIXTURE_APKG").map(Into::into)
}

struct Harness {
    app: axum::Router,
    store: Arc<Store>,
    user: flash_core::UserId,
    cookie: String,
    data_dir: std::path::PathBuf,
}

fn harness() -> Harness {
    let t = AppBuilder::new("import-media").build();
    let (user, cookie) = signed_in(&t.store, "Free", "free@example.com");
    Harness {
        app: t.app,
        store: t.store,
        user,
        cookie,
        data_dir: t.data_dir,
    }
}

async fn send(h: &Harness, req: Request<Body>) -> (StatusCode, Vec<(String, String)>, Vec<u8>) {
    let r = common::send(&h.app, req).await;
    (r.status, r.headers, r.bytes)
}

fn multipart(deck: &str, filename: &str, file: &[u8]) -> (String, Vec<u8>) {
    let boundary = "----flashtestboundary7MA4YWxk";
    let mut body = Vec::new();
    body.extend_from_slice(
        format!("--{boundary}\r\nContent-Disposition: form-data; name=\"deck\"\r\n\r\n{deck}\r\n")
            .as_bytes(),
    );
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(file);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

#[tokio::test]
async fn free_user_imports_a_real_deck_with_images_and_gets_them_back() {
    let Some(package) = fixture_path().and_then(|p| std::fs::read(p).ok()) else {
        eprintln!("FLASH_MEDIA_FIXTURE_APKG not set or unreadable; skipping");
        return;
    };
    let h = harness();

    // Preview: the package is kept for commit because it has referenced media.
    let (ctype, body) = multipart("Pharm", "fixture.apkg", &package);
    let (status, _, html) = send(
        &h,
        Request::post(format!("{BASE}/import/preview"))
            .header(header::COOKIE, &h.cookie)
            .header("sec-fetch-site", "same-origin")
            .header(header::CONTENT_TYPE, ctype)
            .body(Body::from(body))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let html = String::from_utf8_lossy(&html);
    let token = html
        .split("name=\"token\" value=\"")
        .nth(1)
        .and_then(|s| s.split('"').next())
        .expect("preview page carries the commit token")
        .to_string();
    assert!(
        html.contains("images"),
        "preview mentions the media it found: {html}"
    );
    assert!(
        html.contains(r#"name="deck" value="Pharm""#),
        "typed name prefills the destination: {html}"
    );
    assert!(
        html.contains("fixture.apkg") && html.contains(" MB"),
        "preview names the uploaded file and its size: {html}"
    );

    // Commit as the free user.
    let (status, headers, _) = send(
        &h,
        Request::post(format!("{BASE}/import/commit"))
            .header(header::COOKIE, &h.cookie)
            .header("sec-fetch-site", "same-origin")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(format!("token={token}&deck=Pharm")))
            .unwrap(),
    )
    .await;
    assert!(
        status.is_success() || status.is_redirection(),
        "commit: {status} {headers:?}"
    );

    // Every card landed in the deck the user typed, not the file's name.
    let decks = h.store.list_decks(h.user, now_ms()).unwrap();
    assert_eq!(decks.len(), 1, "{decks:?}");
    assert_eq!(decks[0].name, "Pharm");

    // Media rows exist, blobs are on disk, and cards point at /media/{id}.
    let rows = h.store.media_for_export(h.user).unwrap();
    assert!(!rows.is_empty(), "the somemedia fixture carries images");
    let used = h.store.media_bytes_used(h.user).unwrap();
    assert!(used > 0);
    let first = &rows[0];
    let (status, headers, bytes) = send(
        &h,
        Request::get(format!("{BASE}/media/{}", first.id.0))
            .header(header::COOKIE, &h.cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes.len() as u64, first.size);
    assert!(headers
        .iter()
        .any(|(k, v)| k == "content-type" && v == &first.mime));

    // The export bundles every referenced image and rewrites the refs.
    let (status, _, apkg) = send(
        &h,
        Request::get(format!("{BASE}/export.apkg"))
            .header(header::COOKIE, &h.cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let apkg_bytes = apkg.clone();
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(apkg)).unwrap();
    let manifest: serde_json::Value = {
        let mut f = zip.by_name("media").unwrap();
        let mut s = String::new();
        std::io::Read::read_to_string(&mut f, &mut s).unwrap();
        serde_json::from_str(&s).unwrap()
    };
    let bundled = manifest.as_object().unwrap().len();
    assert_eq!(
        bundled,
        rows.len(),
        "one zip entry per referenced media row"
    );
    assert!(zip.by_name("0").is_ok());
    let parsed = flash_store::import::parse_apkg(&apkg_bytes).unwrap();
    assert_eq!(
        parsed.media.referenced(),
        bundled as u32,
        "our own importer sees every image again"
    );
    assert!(
        !parsed
            .rows
            .iter()
            .any(|r| r.front_html.as_deref().unwrap_or("").contains("/media/")),
        "no server-local /media/ refs leak into the package"
    );
    let _ = std::fs::remove_dir_all(&h.data_dir);
}
