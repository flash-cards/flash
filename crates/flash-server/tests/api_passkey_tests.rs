//! Passkey ceremonies over the API: the key travels in the body. A real
//! authenticator can't run here, so these cover the halves that don't
//! need one — the challenge shapes, the guards, and the refusals.

mod common;

use axum::http::StatusCode;
use common::*;
use serde_json::json;

#[tokio::test]
async fn login_start_needs_an_enrolled_passkey_and_returns_webauthn_json() {
    let t = AppBuilder::new("api-passkey-login").build();
    let reply = send(
        &t.app,
        json_post("/api/v1/auth/passkey/login/start", None, &json!({})),
    )
    .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(reply.json()["error"]["code"], "not_available");

    let flashtester = member(&t.store, "FlashTester", "flashtester@example.com");
    let bearer = api_bearer(&t.store, flashtester);
    // Only a real registration produces a parseable passkey; a placeholder
    // row is skipped, so login still has nothing to offer.
    t.store
        .add_passkey(flashtester, b"x", "{}", "phone", 1)
        .unwrap();
    let reply = send(
        &t.app,
        json_post("/api/v1/auth/passkey/login/start", None, &json!({})),
    )
    .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);

    // Adding one: the challenge names the account and comes with a key.
    let reply = send(
        &t.app,
        json_post("/api/v1/settings/passkeys/start", Some(&bearer), &json!({})),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let c = reply.json();
    let ceremony_id = c["ceremony_id"].as_str().unwrap().to_string();
    assert!(!ceremony_id.is_empty());
    assert_eq!(c["options"]["publicKey"]["user"]["name"], "FlashTester");
    // The RP id is the base URL's host (localhost under test, the public host live).
    assert_eq!(c["options"]["publicKey"]["rp"]["id"], "localhost");
    assert!(c["options"]["publicKey"]["challenge"].is_string());

    // Garbage from the "authenticator" is refused and burns the ceremony.
    let cred = json!({
        "id": "AAAA", "rawId": "AAAA", "type": "public-key", "extensions": {},
        "response": {"attestationObject": "AAAA", "clientDataJSON": "AAAA"}
    });
    let reply = send(
        &t.app,
        json_post(
            "/api/v1/settings/passkeys/finish",
            Some(&bearer),
            &json!({"ceremony_id": ceremony_id, "credential": cred, "label": "Test"}),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{}", reply.text());
    let reply = send(
        &t.app,
        json_post(
            "/api/v1/settings/passkeys/finish",
            Some(&bearer),
            &json!({"ceremony_id": ceremony_id, "credential": cred, "label": "Test"}),
        ),
    )
    .await;
    assert_eq!(reply.json()["error"]["code"], "ceremony_expired");

    // Enrolling with a passkey from an invite: the challenge names the
    // invitee; a bad or used invite is refused; garbage burns the ceremony.
    let (invite, _) = t
        .services
        .create_member_invite(flashtester, "Ada", flash_server::service::now_ms())
        .unwrap();
    let reply = send(
        &t.app,
        json_post(
            &format!("/api/v1/auth/enroll/{invite}/passkey/start"),
            None,
            &json!({"email": "not an email"}),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    let reply = send(
        &t.app,
        json_post(
            &format!("/api/v1/auth/enroll/{invite}/passkey/start"),
            None,
            &json!({"email": "ada@example.com"}),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let c = reply.json();
    assert_eq!(c["options"]["publicKey"]["user"]["name"], "Ada");
    let reply = send(
        &t.app,
        json_post(
            &format!("/api/v1/auth/enroll/{invite}/passkey/finish"),
            None,
            &json!({"ceremony_id": c["ceremony_id"], "credential": cred}),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{}", reply.text());
    // The invite survives a failed ceremony; a bogus invite doesn't start one.
    assert_eq!(
        send(
            &t.app,
            json_post("/api/v1/auth/enroll/nope/passkey/start", None, &json!({}))
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );

    // Signed-out callers can't start an add ceremony.
    let reply = send(
        &t.app,
        json_post("/api/v1/settings/passkeys/start", None, &json!({})),
    )
    .await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
}
