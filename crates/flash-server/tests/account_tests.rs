//! Self-serve account deletion against the real router: the confirmation
//! gates (typed DELETE, current password, last admin) and the purge
//! itself. What deletion does to a subscription is a downstream
//! extension's concern, tested there.

mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use common::*;
use flash_server::auth::urlencode;
use flash_server::password::hash_password;
use flash_server::service::now_ms;
use flash_store::Store;
use tower::ServiceExt;

const PASSWORD: &str = "hunter42hunter42";

struct Harness {
    app: axum::Router,
    store: Arc<Store>,
    data_dir: std::path::PathBuf,
}

fn build(tag: &str) -> Harness {
    let t = AppBuilder::new(&format!("account-{tag}")).build();
    Harness {
        app: t.app,
        store: t.store,
        data_dir: t.data_dir,
    }
}

/// A signed-in user; `password` = Some sets a password hash.
fn user_with_session(
    h: &Harness,
    role: &str,
    email: &str,
    password: Option<&str>,
) -> (flash_core::UserId, String) {
    let user = h
        .store
        .create_user("Tester", Some(email), role, now_ms())
        .unwrap();
    if let Some(pw) = password {
        let phc = hash_password(pw).unwrap();
        h.store.set_password_hash(user, Some(&phc)).unwrap();
    }
    (user, web_session(&h.store, user))
}

async fn delete_account(
    h: &Harness,
    cookie: &str,
    body: &str,
) -> (StatusCode, Vec<(String, String)>) {
    let r = send(
        &h.app,
        form_post("/settings/delete-account", Some(cookie), body),
    )
    .await;
    (r.status, r.headers)
}

fn location(headers: &[(String, String)]) -> &str {
    header_of(headers, "location").unwrap_or("")
}

async fn page(h: &Harness, path: &str, cookie: &str) -> (StatusCode, String) {
    let r = send(&h.app, get(path, Some(cookie))).await;
    (r.status, r.text())
}

fn confirm_body(password: Option<&str>) -> String {
    match password {
        Some(pw) => format!("confirm=DELETE&current={}", urlencode(pw)),
        None => "confirm=DELETE".to_string(),
    }
}

#[tokio::test]
async fn confirmation_gates_hold() {
    let h = build("gates");
    let (user, cookie) = user_with_session(&h, "member", "m@example.com", Some(PASSWORD));

    let (status, headers) = delete_account(&h, &cookie, "confirm=delete&current=x").await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location(&headers), "/settings?delerr=confirm");

    let (_, headers) = delete_account(&h, &cookie, "confirm=DELETE&current=wrong-password").await;
    assert_eq!(location(&headers), "/settings?delerr=password");

    let (_, headers) = delete_account(&h, &cookie, "confirm=DELETE").await;
    assert_eq!(
        location(&headers),
        "/settings?delerr=password",
        "missing password counts as wrong"
    );

    assert!(h.store.get_user_info(user).is_ok(), "nothing was deleted");
    let (status, html) = page(&h, "/settings?delerr=password", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("that password is incorrect"));
    assert!(html.contains("/settings/delete-account"));

    // No Origin: the same-origin guard refuses before the handler runs.
    let response = h
        .app
        .clone()
        .oneshot(
            Request::post("/settings/delete-account")
                .header(header::COOKIE, cookie.clone())
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(confirm_body(Some(PASSWORD))))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(h.store.get_user_info(user).is_ok());
}

#[tokio::test]
async fn last_admin_cannot_delete_themselves() {
    let h = build("admin");
    let (admin, cookie) = user_with_session(&h, "admin", "a@example.com", None);
    let (_, headers) = delete_account(&h, &cookie, &confirm_body(None)).await;
    assert_eq!(location(&headers), "/settings?delerr=admin");
    assert!(h.store.get_user_info(admin).is_ok());

    // A second admin lifts the guard.
    h.store
        .create_user("Other admin", None, "admin", now_ms())
        .unwrap();
    let (_, headers) = delete_account(&h, &cookie, &confirm_body(None)).await;
    assert_eq!(location(&headers), "/login?deleted=1");
    assert!(h.store.get_user_info(admin).is_err());
}

#[tokio::test]
async fn deletion_purges_the_account_and_ends_the_session() {
    let h = build("purge");
    let (user, cookie) = user_with_session(&h, "member", "gone@example.com", Some(PASSWORD));
    let deck = h.store.create_deck(user, "D", "", now_ms()).unwrap();
    h.store
        .create_cards(
            user,
            deck,
            &[(flash_core::validate_card_text("f", "b").unwrap(), vec![])],
            None,
            now_ms(),
        )
        .unwrap();
    // An orphan blob on disk goes away with the account.
    let sha = "c".repeat(64);
    let root = flash_server::media::media_dir(&h.data_dir);
    flash_server::media_store::MediaStore::put(
        &flash_server::media_store::DiskStore::new(&root),
        &sha,
        "image/png",
        b"png",
    )
    .unwrap();
    h.store
        .create_media(
            user,
            &sha,
            "x.png",
            "image/png",
            flash_store::media::MediaKind::Image,
            3,
            now_ms(),
        )
        .unwrap();

    let (status, headers) = delete_account(&h, &cookie, &confirm_body(Some(PASSWORD))).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location(&headers), "/login?deleted=1");
    let cleared = headers.iter().any(|(k, v)| {
        k.eq_ignore_ascii_case("set-cookie")
            && v.starts_with("__Host-flash=")
            && v.contains("Max-Age=0")
    });
    assert!(cleared, "session cookie cleared: {headers:?}");

    assert!(h.store.get_user_info(user).is_err());
    assert!(!h.store.email_taken("gone@example.com").unwrap());
    // Blob removal runs in the background after the response; give it a moment.
    let blob = root.join("cc").join(&sha);
    for _ in 0..50 {
        if !blob.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;
    }
    assert!(!blob.exists(), "orphan blob removed");

    // The old cookie is dead; the login page shows the neutral notice.
    let (status, _) = page(&h, "/settings", &cookie).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (status, html) = page(&h, "/login?deleted=1", "").await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("have been deleted"));
}
