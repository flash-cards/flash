//! Onboarding without the website: an admin invite → enroll link →
//! tokens, and reset mail → reset link → tokens (which signs every
//! device out). A self-serve signup loop belongs to a downstream
//! extension and is tested there.

mod common;

use std::sync::Arc;

use axum::http::StatusCode;
use common::*;
use flash_server::auth::{hash_token, new_token};
use flash_server::email::CaptureMailer;
use flash_server::password::hash_password;
use flash_server::service::now_ms;
use serde_json::json;

/// The token inside the last mailed link of the given path prefix.
fn token_from(mailer: &CaptureMailer, prefix: &str) -> String {
    let mails = mailer.0.lock();
    let text = &mails.last().expect("a mail").text;
    let start = text.find(prefix).expect("link in mail") + prefix.len();
    text[start..]
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect()
}

#[tokio::test]
async fn invite_then_enroll_with_password() {
    let t = AppBuilder::new("api-onboarding-invite").build();
    let token = new_token();
    t.store
        .create_invite(
            &hash_token(&token),
            "Ada",
            "member",
            None,
            now_ms() + 86_400_000,
            now_ms(),
        )
        .unwrap();

    // The link's invite is readable; an admin invite carries no address.
    let info = send(
        &t.app,
        json_get(&format!("/api/v1/auth/enroll/{token}"), None),
    )
    .await;
    assert_eq!(info.status, StatusCode::OK, "{}", info.text());
    assert_eq!(info.json()["display_name"], "Ada");
    assert_eq!(info.json()["email"], serde_json::Value::Null);

    // A weak password and a missing address each speak the envelope.
    let reply = send(
        &t.app,
        json_post(
            &format!("/api/v1/auth/enroll/{token}/password"),
            None,
            &json!({"password": "short", "email": "ada@example.com"}),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    let reply = send(
        &t.app,
        json_post(
            &format!("/api/v1/auth/enroll/{token}/password"),
            None,
            &json!({"password": "correct horse battery"}),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    let reply = send(
        &t.app,
        json_post(
            &format!("/api/v1/auth/enroll/{token}/password"),
            None,
            &json!({
                "password": "correct horse battery", "email": "Ada@Example.com",
                "device_label": "Ada's iPhone"
            }),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let pair = reply.json();
    assert_eq!(pair["user"]["display_name"], "Ada");
    assert_eq!(pair["user"]["email"], "ada@example.com");
    let bearer = pair["access_token"].as_str().unwrap().to_string();
    assert_eq!(
        send(&t.app, json_get("/api/v1/me", Some(&bearer)))
            .await
            .status,
        StatusCode::OK
    );

    // Single use.
    let again = send(
        &t.app,
        json_get(&format!("/api/v1/auth/enroll/{token}"), None),
    )
    .await;
    assert_eq!(again.status, StatusCode::FORBIDDEN);
    assert_eq!(again.json()["error"]["code"], "link_invalid");
    let garbage = send(
        &t.app,
        json_get("/api/v1/auth/enroll/not%20a%20token!", None),
    )
    .await;
    assert_eq!(garbage.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn reset_signs_out_every_device() {
    let mailer = Arc::new(CaptureMailer::default());
    let t = AppBuilder::new("api-onboarding-reset")
        .mailer(mailer.clone())
        .build();
    let flashtester = member(&t.store, "FlashTester", "flashtester@example.com");
    t.store
        .set_password_hash(flashtester, Some(&hash_password("old password 1").unwrap()))
        .unwrap();
    let old_bearer = api_bearer(&t.store, flashtester);
    let web_cookie = web_session(&t.store, flashtester);

    // Unknown addresses get the same 202 and no mail.
    let reply = send(
        &t.app,
        json_post(
            "/api/v1/auth/reset",
            None,
            &json!({"email": "nobody@example.com"}),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::ACCEPTED);
    assert!(mailer.0.lock().is_empty());

    let reply = send(
        &t.app,
        json_post(
            "/api/v1/auth/reset",
            None,
            &json!({"email": "flashtester@example.com"}),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::ACCEPTED);
    let token = token_from(&mailer, "/reset/");

    let reply = send(
        &t.app,
        json_post(
            &format!("/api/v1/auth/reset/{token}"),
            None,
            &json!({"password": "brand new password"}),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let new_bearer = reply.json()["access_token"].as_str().unwrap().to_string();

    // The old grant and the web session are gone; the new grant works;
    // the new password signs in.
    assert_eq!(
        send(&t.app, json_get("/api/v1/me", Some(&old_bearer)))
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        send(&t.app, json_get("/api/v1/me", Some(&new_bearer)))
            .await
            .status,
        StatusCode::OK
    );
    assert!(
        t.store.delete_sessions_for_user(flashtester, None).unwrap() == 0,
        "web session revoked"
    );
    let _ = web_cookie;
    let reply = send(
        &t.app,
        json_post(
            "/api/v1/auth/password",
            None,
            &json!({"email": "flashtester@example.com", "password": "brand new password"}),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);

    // The link is single-use.
    let reply = send(
        &t.app,
        json_post(
            &format!("/api/v1/auth/reset/{token}"),
            None,
            &json!({"password": "another password"}),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
}
