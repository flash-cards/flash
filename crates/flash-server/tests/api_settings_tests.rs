//! Settings over the API: the overview, the simple forms, the password
//! lifecycle (set → change revokes other devices → remove guarded by
//! the last-method rule), passkey removal, export, and the last-admin
//! deletion guard. Billing and external providers belong to a
//! downstream extension and are tested there.

mod common;

use axum::http::StatusCode;
use common::*;
use flash_server::password::hash_password;
use serde_json::json;

fn with_password(store: &flash_store::Store, user: flash_core::UserId, password: &str) {
    store
        .set_password_hash(user, Some(&hash_password(password).unwrap()))
        .unwrap();
}

#[tokio::test]
async fn overview_and_simple_forms() {
    let t = AppBuilder::new("api-settings-overview").build();
    let flashtester = member(&t.store, "FlashTester", "flashtester@example.com");
    let bearer = api_bearer(&t.store, flashtester);

    let reply = send(&t.app, json_get("/api/v1/settings", Some(&bearer))).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let s = reply.json();
    assert_eq!(s["display_name"], "FlashTester");
    assert_eq!(s["theme"], "dark");
    assert_eq!(s["grading_mode"], "silent");
    assert_eq!(s["new_per_day"], 20);
    assert_eq!(s["timezone"], "UTC");
    assert_eq!(s["login_methods"]["password"], false);
    // Plans and billing are the hosted product's: absent here.
    assert_eq!(s["plan"], serde_json::Value::Null);
    assert_eq!(s["usage"], serde_json::Value::Null);
    assert_eq!(s["billing"], serde_json::Value::Null);

    for (method, path, body) in [
        (
            "PATCH",
            "/api/v1/settings/profile",
            json!({"display_name": "  FlashTester V. "}),
        ),
        (
            "PUT",
            "/api/v1/settings/grading",
            json!({"mode": "announce"}),
        ),
        ("PUT", "/api/v1/settings/theme", json!({"theme": "clay"})),
        (
            "PUT",
            "/api/v1/settings/limits",
            json!({"new_per_day": 5, "reviews_per_day": 50}),
        ),
        (
            "PUT",
            "/api/v1/settings/study",
            json!({"day_cutoff_hour": 3}),
        ),
    ] {
        let reply = send(&t.app, json_req(method, path, Some(&bearer), Some(&body))).await;
        assert_eq!(
            reply.status,
            StatusCode::NO_CONTENT,
            "{path}: {}",
            reply.text()
        );
    }
    let s = send(&t.app, json_get("/api/v1/settings", Some(&bearer)))
        .await
        .json();
    assert_eq!(s["display_name"], "FlashTester V.");
    assert_eq!(s["grading_mode"], "announce");
    assert_eq!(s["theme"], "clay");
    assert_eq!(s["new_per_day"], 5);
    assert_eq!(s["reviews_per_day"], 50);
    assert_eq!(s["day_cutoff_hour"], 3);

    // Validation speaks the envelope.
    for (method, path, body) in [
        (
            "PATCH",
            "/api/v1/settings/profile",
            json!({"display_name": "   "}),
        ),
        ("PUT", "/api/v1/settings/grading", json!({"mode": "loud"})),
        ("PUT", "/api/v1/settings/theme", json!({"theme": "neon"})),
        (
            "PUT",
            "/api/v1/settings/study",
            json!({"timezone": "Mars/Olympus"}),
        ),
    ] {
        let reply = send(&t.app, json_req(method, path, Some(&bearer), Some(&body))).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{path}");
        assert_eq!(reply.json()["error"]["code"], "invalid");
    }
}

#[tokio::test]
async fn password_set_change_and_remove() {
    let t = AppBuilder::new("api-settings-password").build();
    let flashtester = member(&t.store, "FlashTester", "flashtester@example.com");
    let phone = api_bearer(&t.store, flashtester);
    let tablet = api_bearer(&t.store, flashtester);

    // Too short never reaches the store.
    let reply = send(
        &t.app,
        json_post(
            "/api/v1/settings/password",
            Some(&phone),
            &json!({"password": "short"}),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);

    // Set (no current needed when there is none).
    let reply = send(
        &t.app,
        json_post(
            "/api/v1/settings/password",
            Some(&phone),
            &json!({"password": "hunter42hunter42"}),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT, "{}", reply.text());
    assert!(t.store.get_password_hash(flashtester).unwrap().is_some());

    // Change requires the current one, and a wrong one is a distinct code
    // (not 401, which would make the app refresh its token).
    let reply = send(
        &t.app,
        json_post(
            "/api/v1/settings/password",
            Some(&phone),
            &json!({"password": "newpassword123", "current": "nope"}),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert_eq!(reply.json()["error"]["code"], "wrong_password");
    let reply = send(
        &t.app,
        json_post(
            "/api/v1/settings/password",
            Some(&phone),
            &json!({"password": "newpassword123"}),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);

    // The right one changes it and signs out the other device only.
    let reply = send(
        &t.app,
        json_post(
            "/api/v1/settings/password",
            Some(&phone),
            &json!({"password": "newpassword123", "current": "hunter42hunter42"}),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT, "{}", reply.text());
    assert_eq!(
        send(&t.app, json_get("/api/v1/me", Some(&phone)))
            .await
            .status,
        StatusCode::OK
    );
    assert_eq!(
        send(&t.app, json_get("/api/v1/me", Some(&tablet)))
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );

    // Removing the only login method is refused; with a passkey it works.
    let reply = send(
        &t.app,
        json_req(
            "DELETE",
            "/api/v1/settings/password",
            Some(&phone),
            Some(&json!({"current": "newpassword123"})),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert_eq!(reply.json()["error"]["code"], "last_login_method");
    t.store
        .add_passkey(flashtester, b"cred", "{}", "phone", 1)
        .unwrap();
    let reply = send(
        &t.app,
        json_req(
            "DELETE",
            "/api/v1/settings/password",
            Some(&phone),
            Some(&json!({"current": "newpassword123"})),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT, "{}", reply.text());
    assert!(t.store.get_password_hash(flashtester).unwrap().is_none());
}

#[tokio::test]
async fn passkeys_respect_the_last_method_rule() {
    let t = AppBuilder::new("api-settings-methods").build();
    let flashtester = member(&t.store, "FlashTester", "flashtester@example.com");
    let bearer = api_bearer(&t.store, flashtester);
    t.store
        .add_passkey(flashtester, b"one", "{}", "phone", 1)
        .unwrap();
    let pk = t.store.passkeys_for_user(flashtester).unwrap()[0].id;

    // The only passkey can't go.
    let reply = send(
        &t.app,
        json_delete(&format!("/api/v1/settings/passkeys/{pk}"), Some(&bearer)),
    )
    .await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    // Someone else's passkey is simply not found.
    let ada = member(&t.store, "Ada", "ada@example.com");
    let ada_bearer = api_bearer(&t.store, ada);
    let reply = send(
        &t.app,
        json_delete(
            &format!("/api/v1/settings/passkeys/{pk}"),
            Some(&ada_bearer),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    let s = send(&t.app, json_get("/api/v1/settings", Some(&bearer)))
        .await
        .json();
    assert_eq!(s["login_methods"]["passkeys"][0]["id"], pk);

    // With a password set, the passkey can go.
    with_password(&t.store, flashtester, "hunter42hunter42");
    let reply = send(
        &t.app,
        json_delete(&format!("/api/v1/settings/passkeys/{pk}"), Some(&bearer)),
    )
    .await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT, "{}", reply.text());
    assert!(t.store.passkeys_for_user(flashtester).unwrap().is_empty());
}

#[tokio::test]
async fn export_is_never_gated() {
    let t = AppBuilder::new("api-settings-export").build();
    let flashtester = member(&t.store, "FlashTester", "flashtester@example.com");
    let bearer = api_bearer(&t.store, flashtester);
    t.services.create_deck(flashtester, "Bio", "", 1).unwrap();
    t.services
        .create_cards(
            flashtester,
            "Bio",
            &[("Cell".to_string(), "Unit of life".to_string(), vec![])],
            1,
        )
        .unwrap();
    let reply = send(&t.app, json_get("/api/v1/export/csv", Some(&bearer))).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply
        .header("content-disposition")
        .unwrap()
        .contains("flash-"));
    assert!(reply.text().contains("Unit of life"));
    let reply = send(&t.app, json_get("/api/v1/export/apkg", Some(&bearer))).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.bytes.starts_with(b"PK"));
}

#[tokio::test]
async fn delete_account_refuses_the_last_admin() {
    let t = AppBuilder::new("api-settings-delete-refused").build();
    let admin = t
        .store
        .create_user("Root", Some("root@example.com"), "admin", 1)
        .unwrap();
    let admin_bearer = api_bearer(&t.store, admin);
    let reply = send(
        &t.app,
        json_req(
            "DELETE",
            "/api/v1/account",
            Some(&admin_bearer),
            Some(&json!({"confirm": "DELETE"})),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert_eq!(reply.json()["error"]["code"], "last_admin");
    assert!(t.store.user_by_email("root@example.com").unwrap().is_some());
}
