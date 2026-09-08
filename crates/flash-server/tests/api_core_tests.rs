//! The mobile API's plumbing: the /api/v1 mount sits outside the
//! same-origin guard, speaks one JSON error envelope, and only admits
//! bearer tokens minted for the first-party client.

mod common;

use axum::http::StatusCode;
use common::*;

#[tokio::test]
async fn meta_is_public_and_reports_features() {
    let t = AppBuilder::new("api-meta").build();
    let reply = send(&t.app, json_get("/api/v1/meta", None)).await;
    assert_eq!(reply.status, StatusCode::OK);
    let json = reply.json();
    assert_eq!(json["min_app_version"], "1.0.0");
    assert_eq!(json["features"]["signup"], false);
    assert_eq!(json["features"]["google"], false);
    assert_eq!(json["features"]["billing"], false);
    assert_eq!(json["features"]["passkeys"], true);
}

#[tokio::test]
async fn unknown_api_routes_answer_in_the_envelope() {
    let t = AppBuilder::new("api-404").build();
    let reply = send(&t.app, json_get("/api/v1/nope", None)).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    let json = reply.json();
    assert_eq!(json["error"]["code"], "not_found");
    assert_eq!(json["error"]["what"], "route");
}

#[tokio::test]
async fn protected_routes_need_a_first_party_bearer() {
    let t = AppBuilder::new("api-bearer").build();
    let user = member(&t.store, "FlashTester", "flashtester@example.com");

    // No token: 401 in the envelope, with the challenge header.
    let reply = send(&t.app, json_get("/api/v1/badge", None)).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    assert_eq!(reply.json()["error"]["code"], "unauthorized");
    assert_eq!(reply.header("www-authenticate"), Some("Bearer"));

    // An MCP grant is a valid OAuth token but not the app's.
    let mcp = oauth_bearer(&t.store, user);
    let reply = send(&t.app, json_get("/api/v1/badge", Some(&mcp))).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);

    // Garbage is just garbage.
    let reply = send(&t.app, json_get("/api/v1/badge", Some("nope"))).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);

    // The app's own grant works, and a POST needs no Origin header.
    let bearer = api_bearer(&t.store, user);
    let reply = send(&t.app, json_get("/api/v1/badge", Some(&bearer))).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.json()["badge"], 0);
}

#[tokio::test]
async fn api_replies_carry_no_store_and_security_headers() {
    let t = AppBuilder::new("api-headers").build();
    let reply = send(&t.app, json_get("/api/v1/badge", None)).await;
    assert_eq!(reply.header("cache-control"), Some("no-store"));
    assert_eq!(reply.header("x-content-type-options"), Some("nosniff"));
}
