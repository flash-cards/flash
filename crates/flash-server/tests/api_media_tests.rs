//! Media with a bearer: the app uploads through the same handler the
//! editor uses and fetches blobs (with Range) for its local cache. A
//! bearer failure is the JSON 401, while a plain browser request keeps
//! its login redirect.

mod common;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use common::*;

/// The smallest valid PNG (1×1), as the server sniffs magic bytes.
const PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE,
    0x42, 0x60, 0x82,
];

fn multipart_upload(path: &str, bearer: &str, filename: &str, bytes: &[u8]) -> Request<Body> {
    let boundary = "flashboundary";
    let mut body = Vec::new();
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    Request::post(path)
        .header(header::AUTHORIZATION, format!("Bearer {bearer}"))
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(Body::from(body))
        .unwrap()
}

#[tokio::test]
async fn upload_then_fetch_with_a_bearer_and_a_range() {
    let t = AppBuilder::new("api-media").build();
    let user = member(&t.store, "FlashTester", "flashtester@example.com");
    let bearer = api_bearer(&t.store, user);

    let up = send(
        &t.app,
        multipart_upload("/api/v1/media", &bearer, "dot.png", PNG),
    )
    .await;
    assert_eq!(up.status, StatusCode::OK, "{}", up.text());
    let id = up.json()["id"].as_i64().unwrap();
    assert_eq!(up.json()["kind"], "image");

    let whole = send(
        &t.app,
        json_get(&format!("/api/v1/media/{id}"), Some(&bearer)),
    )
    .await;
    assert_eq!(whole.status, StatusCode::OK);
    assert_eq!(whole.header("content-type"), Some("image/png"));
    assert_eq!(whole.bytes, PNG);
    assert_eq!(whole.header("accept-ranges"), Some("bytes"));

    // The same blob at the web path, and a byte range for media seeking.
    let ranged = send(
        &t.app,
        Request::get(format!("/media/{id}"))
            .header(header::AUTHORIZATION, format!("Bearer {bearer}"))
            .header(header::RANGE, "bytes=0-7")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(ranged.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(ranged.bytes, &PNG[..8]);

    // HEAD carries the type without the body (the app sniffs the
    // extension from it before downloading).
    let head = send(
        &t.app,
        Request::head(format!("/api/v1/media/{id}"))
            .header(header::AUTHORIZATION, format!("Bearer {bearer}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(head.status, StatusCode::OK);
    assert_eq!(head.header("content-type"), Some("image/png"));
    assert!(head.bytes.is_empty());
}

#[tokio::test]
async fn media_is_owner_scoped_and_auth_failures_match_the_surface() {
    let t = AppBuilder::new("api-media-auth").build();
    let flashtester = member(&t.store, "FlashTester", "flashtester@example.com");
    let flashtester_bearer = api_bearer(&t.store, flashtester);
    let up = send(
        &t.app,
        multipart_upload("/api/v1/media", &flashtester_bearer, "dot.png", PNG),
    )
    .await;
    let id = up.json()["id"].as_i64().unwrap();

    let ada = member(&t.store, "Ada", "ada@example.com");
    let ada_bearer = api_bearer(&t.store, ada);
    let other = send(
        &t.app,
        json_get(&format!("/api/v1/media/{id}"), Some(&ada_bearer)),
    )
    .await;
    assert_eq!(other.status, StatusCode::NOT_FOUND);

    // A bad bearer: JSON 401. No credentials at all in a browser: redirect.
    let bad = send(
        &t.app,
        json_get(&format!("/api/v1/media/{id}"), Some("nope")),
    )
    .await;
    assert_eq!(bad.status, StatusCode::UNAUTHORIZED);
    assert_eq!(bad.json()["error"]["code"], "unauthorized");
    let browser = send(&t.app, get(&format!("/media/{id}"), None)).await;
    assert_eq!(browser.status, StatusCode::SEE_OTHER);
    assert!(browser.location().starts_with("/login"));
}
