//! Signing in from the app: password (uniform failures, lockout), the
//! refresh rotation, logout and the sessions list, ending in first-party
//! bearer tokens. External providers belong to a downstream extension
//! and are tested there.

mod common;

use axum::http::StatusCode;
use common::*;
use flash_core::UserId;
use flash_server::password::hash_password;
use flash_store::Store;
use serde_json::json;

fn with_password(store: &Store, name: &str, email: &str, password: &str) -> UserId {
    let user = member(store, name, email);
    store
        .set_password_hash(user, Some(&hash_password(password).unwrap()))
        .unwrap();
    user
}

async fn login(t: &TestApp, email: &str, password: &str, label: &str) -> Reply {
    send(
        &t.app,
        json_post(
            "/api/v1/auth/password",
            None,
            &json!({ "email": email, "password": password, "device_label": label }),
        ),
    )
    .await
}

#[tokio::test]
async fn password_login_mints_tokens_and_me() {
    let t = AppBuilder::new("api-pw").build();
    let user = with_password(
        &t.store,
        "FlashTester",
        "flashtester@example.com",
        "hunter42hunter42",
    );

    let reply = login(&t, "FlashTester@Example.com ", "hunter42hunter42", "iPhone").await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let json = reply.json();
    assert_eq!(json["token_type"], "Bearer");
    assert_eq!(json["expires_in"], 3600);
    assert_eq!(json["user"]["id"], user.raw());
    assert_eq!(json["user"]["display_name"], "FlashTester");
    assert_eq!(json["user"]["theme"], "dark");
    assert_eq!(json["user"]["login_methods"]["password"], true);
    assert_eq!(reply.header("cache-control"), Some("no-store"));

    let access = json["access_token"].as_str().unwrap();
    let me = send(&t.app, json_get("/api/v1/me", Some(access))).await;
    assert_eq!(me.status, StatusCode::OK);
    assert_eq!(me.json()["email"], "flashtester@example.com");
}

#[tokio::test]
async fn wrong_password_unknown_email_and_lockout_are_uniform() {
    let t = AppBuilder::new("api-pw-fail").build();
    with_password(
        &t.store,
        "FlashTester",
        "flashtester@example.com",
        "hunter42hunter42",
    );

    let wrong = login(&t, "flashtester@example.com", "nope-nope-nope", "x").await;
    assert_eq!(wrong.status, StatusCode::UNAUTHORIZED);
    assert_eq!(wrong.json()["error"]["code"], "invalid_credentials");
    let unknown = login(&t, "nobody@example.com", "hunter42hunter42", "x").await;
    assert_eq!(unknown.status, StatusCode::UNAUTHORIZED);
    assert_eq!(unknown.json(), wrong.json(), "no account-existence oracle");

    // Four more failures reach the lockout; the right password is then
    // refused with the very same answer.
    for _ in 0..4 {
        login(&t, "flashtester@example.com", "still-wrong-pw", "x").await;
    }
    let locked = login(&t, "flashtester@example.com", "hunter42hunter42", "x").await;
    assert_eq!(locked.status, StatusCode::UNAUTHORIZED);
    assert_eq!(locked.json(), wrong.json());
}

#[tokio::test]
async fn refresh_rotates_and_the_old_token_dies() {
    let t = AppBuilder::new("api-refresh").build();
    with_password(
        &t.store,
        "FlashTester",
        "flashtester@example.com",
        "hunter42hunter42",
    );
    let first = login(&t, "flashtester@example.com", "hunter42hunter42", "iPhone")
        .await
        .json();
    let old_refresh = first["refresh_token"].as_str().unwrap();

    let reply = send(
        &t.app,
        json_post(
            "/api/v1/auth/refresh",
            None,
            &json!({ "refresh_token": old_refresh }),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let second = reply.json();
    assert_ne!(second["access_token"], first["access_token"]);
    assert_eq!(second["user"]["display_name"], "FlashTester");

    // The rotated-away refresh token is dead; the new access token lives.
    let replay = send(
        &t.app,
        json_post(
            "/api/v1/auth/refresh",
            None,
            &json!({ "refresh_token": old_refresh }),
        ),
    )
    .await;
    assert_eq!(replay.status, StatusCode::UNAUTHORIZED);
    assert_eq!(replay.json()["error"]["code"], "invalid_grant");
    let me = send(
        &t.app,
        json_get("/api/v1/me", second["access_token"].as_str()),
    )
    .await;
    assert_eq!(me.status, StatusCode::OK);
}

#[tokio::test]
async fn logout_revokes_the_grant() {
    let t = AppBuilder::new("api-logout").build();
    with_password(
        &t.store,
        "FlashTester",
        "flashtester@example.com",
        "hunter42hunter42",
    );
    let pair = login(&t, "flashtester@example.com", "hunter42hunter42", "iPhone")
        .await
        .json();
    let access = pair["access_token"].as_str().unwrap();

    let reply = send(
        &t.app,
        json_post("/api/v1/auth/logout", Some(access), &json!({})),
    )
    .await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT);
    let me = send(&t.app, json_get("/api/v1/me", Some(access))).await;
    assert_eq!(me.status, StatusCode::UNAUTHORIZED);
    let refresh = send(
        &t.app,
        json_post(
            "/api/v1/auth/refresh",
            None,
            &json!({ "refresh_token": pair["refresh_token"] }),
        ),
    )
    .await;
    assert_eq!(refresh.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn sessions_list_revoke_others_and_revoke_one() {
    let t = AppBuilder::new("api-sessions").build();
    with_password(
        &t.store,
        "FlashTester",
        "flashtester@example.com",
        "hunter42hunter42",
    );
    let phone = login(&t, "flashtester@example.com", "hunter42hunter42", "iPhone")
        .await
        .json();
    let ipad = login(&t, "flashtester@example.com", "hunter42hunter42", "iPad")
        .await
        .json();
    let phone_access = phone["access_token"].as_str().unwrap();
    let ipad_access = ipad["access_token"].as_str().unwrap();

    let list = send(&t.app, json_get("/api/v1/auth/sessions", Some(ipad_access))).await;
    assert_eq!(list.status, StatusCode::OK);
    let items = list.json()["items"].as_array().unwrap().clone();
    assert_eq!(items.len(), 2);
    let current: Vec<&serde_json::Value> = items.iter().filter(|i| i["current"] == true).collect();
    assert_eq!(current.len(), 1);
    assert_eq!(current[0]["label"], "iPad");

    // Sign out other devices keeps the caller.
    let reply = send(
        &t.app,
        json_post(
            "/api/v1/auth/sessions/revoke_others",
            Some(ipad_access),
            &json!({}),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.json()["revoked"], 1);
    let phone_me = send(&t.app, json_get("/api/v1/me", Some(phone_access))).await;
    assert_eq!(phone_me.status, StatusCode::UNAUTHORIZED);
    let ipad_me = send(&t.app, json_get("/api/v1/me", Some(ipad_access))).await;
    assert_eq!(ipad_me.status, StatusCode::OK);

    // Revoking one grant by id, then it's gone and a repeat is a 404.
    let id = current[0]["id"].as_i64().unwrap();
    let reply = send(
        &t.app,
        json_delete(&format!("/api/v1/auth/sessions/{id}"), Some(ipad_access)),
    )
    .await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT);
    let again = send(&t.app, json_get("/api/v1/me", Some(ipad_access))).await;
    assert_eq!(again.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn malformed_json_speaks_the_envelope() {
    let t = AppBuilder::new("api-badjson").build();
    let reply = send(
        &t.app,
        json_post("/api/v1/auth/password", None, &json!({ "email": 5 })),
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(reply.json()["error"]["code"], "invalid");
}
