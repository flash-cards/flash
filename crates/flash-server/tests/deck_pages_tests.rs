//! The deck pages over HTTP: the never-gated export downloads, deck
//! deletion, the settings page with its limits and rename, and the study
//! boosts. A plan cap belongs to a downstream extension and is tested
//! there.

mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use common::*;
use flash_server::service::now_ms;
use flash_store::Store;

struct Harness {
    app: axum::Router,
    store: Arc<Store>,
    user: flash_core::UserId,
    session_cookie: String,
}

fn harness() -> Harness {
    // One data directory per test: an export is a file in it for as long
    // as the download runs, and a parallel test's fresh harness wipes its
    // own directory on construction.
    static CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let t = AppBuilder::new(&format!("deck-pages-{n}")).build();
    let (user, session_cookie) = signed_in(&t.store, "Cap", "cap@example.com");
    Harness {
        app: t.app,
        store: t.store,
        user,
        session_cookie,
    }
}

fn seed_cards(h: &Harness, n: usize) -> flash_core::DeckId {
    let deck = h.store.create_deck(h.user, "Seed", "", now_ms()).unwrap();
    let cards: Vec<_> = (0..n)
        .map(|i| {
            (
                flash_core::validate_card_text(&format!("f{i}"), "b").unwrap(),
                Vec::new(),
            )
        })
        .collect();
    h.store
        .create_cards(h.user, deck, &cards, None, now_ms())
        .unwrap();
    deck
}

async fn request(h: &Harness, req: Request<Body>) -> (StatusCode, Vec<(String, String)>, Vec<u8>) {
    let r = send(&h.app, req).await;
    (r.status, r.headers, r.bytes)
}

#[tokio::test]
async fn export_endpoints_download_every_card() {
    let h = harness();
    seed_cards(&h, 12);

    let (status, headers, bytes) = request(
        &h,
        Request::get("/export.apkg")
            .header(header::COOKIE, h.session_cookie.clone())
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(header_of(&headers, "content-disposition")
        .unwrap()
        .contains("attachment; filename=\"flash-"));
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
    assert!(zip.by_name("collection.anki2").is_ok());
    assert!(zip.by_name("media").is_ok());

    let (status, _, bytes) = request(
        &h,
        Request::get("/export.csv")
            .header(header::COOKIE, h.session_cookie.clone())
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let csv = String::from_utf8_lossy(&bytes);
    assert!(csv.starts_with("front,back,deck,tags"));
    assert_eq!(csv.lines().count(), 1 + 12);
}

#[tokio::test]
async fn export_requires_auth() {
    let h = harness();
    let (status, headers, _) = request(
        &h,
        Request::get("/export.apkg").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(header_of(&headers, "location")
        .unwrap()
        .starts_with("/login"));
}

#[tokio::test]
async fn deck_delete_button_removes_deck_and_redirects() {
    let h = harness();
    let deck = seed_cards(&h, 3);
    let (status, headers, _) = request(
        &h,
        Request::post(format!("/decks/{deck}/delete"))
            .header(header::ORIGIN, BASE)
            .header(header::COOKIE, h.session_cookie.clone())
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(header_of(&headers, "hx-redirect"), Some("/decks"));

    // The deck page is gone; an unknown id 404s the same way.
    let (status, _, _) = request(
        &h,
        Request::get(format!("/decks/{deck}"))
            .header(header::COOKIE, h.session_cookie.clone())
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, _) = request(
        &h,
        Request::post("/decks/999999/delete")
            .header(header::ORIGIN, BASE)
            .header(header::COOKIE, h.session_cookie.clone())
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn deck_settings_page_holds_limits_and_delete() {
    let h = harness();
    let deck = seed_cards(&h, 1);
    let get = |path: String| {
        Request::get(path)
            .header(header::COOKIE, h.session_cookie.clone())
            .body(Body::empty())
            .unwrap()
    };
    let (status, _, body) = request(&h, get(format!("/decks/{deck}"))).await;
    assert_eq!(status, StatusCode::OK);
    let html = String::from_utf8(body).unwrap();
    assert!(html.contains("Add a card"));
    assert!(html.contains(&format!("/decks/{deck}/settings")));
    assert!(!html.contains("Daily limits"));
    assert!(!html.contains("Delete deck"));

    let (status, _, body) = request(&h, get(format!("/decks/{deck}/settings"))).await;
    assert_eq!(status, StatusCode::OK);
    let html = String::from_utf8(body).unwrap();
    assert!(html.contains("Daily limits"));
    assert!(html.contains("Delete deck"));
    assert!(html.contains(&format!("/decks/{deck}/rename")));

    let (status, _, _) = request(&h, get("/decks/999999/settings".into())).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn rename_deck_updates_name_and_rejects_bad_names() {
    let h = harness();
    let deck = seed_cards(&h, 1);
    h.store.create_deck(h.user, "Other", "", now_ms()).unwrap();
    let post = |name: &str| {
        Request::post(format!("/decks/{deck}/rename"))
            .header(header::ORIGIN, BASE)
            .header(header::COOKIE, h.session_cookie.clone())
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(format!("name={name}")))
            .unwrap()
    };

    let (status, headers, _) = request(&h, post("Renamed")).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
        header_of(&headers, "location"),
        Some(&*format!("/decks/{deck}/settings?saved=name"))
    );
    let (_, _, body) = request(
        &h,
        Request::get(format!("/decks/{deck}/settings"))
            .header(header::COOKIE, h.session_cookie.clone())
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert!(String::from_utf8(body)
        .unwrap()
        .contains("value=\"Renamed\""));

    let (status, headers, _) = request(&h, post("Other")).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
        header_of(&headers, "location"),
        Some(&*format!("/decks/{deck}/settings?err=taken"))
    );

    let (status, headers, _) = request(&h, post("+++")).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
        header_of(&headers, "location"),
        Some(&*format!("/decks/{deck}/settings?err=empty"))
    );

    let (status, _, _) = request(
        &h,
        Request::post("/decks/999999/rename")
            .header(header::ORIGIN, BASE)
            .header(header::COOKIE, h.session_cookie.clone())
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from("name=x"))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn boost_from_study_returns_to_the_queue() {
    let h = harness();
    let deck = seed_cards(&h, 1);
    let post = |path: String, body: &'static str| {
        Request::post(path)
            .header(header::ORIGIN, BASE)
            .header(header::COOKIE, h.session_cookie.clone())
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(body))
            .unwrap()
    };
    let (status, headers, _) = request(
        &h,
        post(format!("/decks/{deck}/boost"), "extra=5&back=study"),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
        header_of(&headers, "location"),
        Some(&*format!("/study?deck={deck}"))
    );
    let (status, headers, _) = request(&h, post(format!("/decks/{deck}/boost"), "extra=5")).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
        header_of(&headers, "location"),
        Some(&*format!("/decks/{deck}/settings"))
    );
    let (status, headers, _) = request(&h, post("/study/boost".into(), "extra=5&back=study")).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(header_of(&headers, "location"), Some("/study"));
}
