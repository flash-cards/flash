//! Import over the API: a CSV preview (parked on disk under a token),
//! the commit that inserts it, the deck-name and expired-token refusals.

mod common;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use common::*;
use serde_json::json;

fn upload(path: &str, bearer: &str, filename: &str, bytes: &[u8], deck: &str) -> Request<Body> {
    let boundary = "flashboundary";
    let mut body = Vec::new();
    body.extend_from_slice(
        format!("--{boundary}\r\nContent-Disposition: form-data; name=\"deck\"\r\n\r\n{deck}\r\n")
            .as_bytes(),
    );
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\nContent-Type: text/csv\r\n\r\n"
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

const CSV: &[u8] =
    b"front,back,tags,deck\nCell,Unit of life,bio,Bio::Cells\nATP,Energy,bio,Bio::Energy\n";

#[tokio::test]
async fn a_new_preview_replaces_the_previous_one() {
    // One pending import per user: repeated previews cannot pile packages
    // up on the disk that also holds the database.
    let t = AppBuilder::new("api-import-replace").build();
    let flashtester = member(&t.store, "FlashTester", "flashtester@example.com");
    let bearer = api_bearer(&t.store, flashtester);

    let first = send(
        &t.app,
        upload("/api/v1/import/preview", &bearer, "one.csv", CSV, ""),
    )
    .await
    .json()["token"]
        .as_str()
        .unwrap()
        .to_string();
    let second = send(
        &t.app,
        upload("/api/v1/import/preview", &bearer, "two.csv", CSV, ""),
    )
    .await
    .json()["token"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(first, second);

    let stale = send(
        &t.app,
        json_post(
            "/api/v1/import/commit",
            Some(&bearer),
            &json!({"token": first, "deck": "Biology"}),
        ),
    )
    .await;
    assert_eq!(stale.status, StatusCode::BAD_REQUEST, "{}", stale.text());
    let live = send(
        &t.app,
        json_post(
            "/api/v1/import/commit",
            Some(&bearer),
            &json!({"token": second, "deck": "Biology"}),
        ),
    )
    .await;
    assert_eq!(live.status, StatusCode::OK, "{}", live.text());
}

#[tokio::test]
async fn preview_then_commit() {
    let t = AppBuilder::new("api-import").build();
    let flashtester = member(&t.store, "FlashTester", "flashtester@example.com");
    let bearer = api_bearer(&t.store, flashtester);

    let reply = send(
        &t.app,
        upload("/api/v1/import/preview", &bearer, "cards.csv", CSV, ""),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let p = reply.json();
    assert_eq!(p["total"], 2);
    assert_eq!(p["file_name"], "cards.csv");
    assert_eq!(p["deck"], "Bio");
    assert!(p["deck_note"].as_str().unwrap().contains("2 decks"));
    assert_eq!(p["sample"][0]["front"], "Cell");
    assert_eq!(p["progress_reviews"], 0);
    let token = p["token"].as_str().unwrap().to_string();

    // A blank deck name is refused; the preview survives the refusal.
    let reply = send(
        &t.app,
        json_post(
            "/api/v1/import/commit",
            Some(&bearer),
            &json!({"token": token, "deck": "  "}),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);

    let reply = send(
        &t.app,
        json_post(
            "/api/v1/import/commit",
            Some(&bearer),
            &json!({"token": token, "deck": "Biology", "colors": "keep"}),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(reply.json()["imported"], 2);
    let decks = send(&t.app, json_get("/api/v1/decks", Some(&bearer)))
        .await
        .json();
    assert_eq!(decks["items"][0]["name"], "Biology");

    // The parked preview is gone after commit.
    let reply = send(
        &t.app,
        json_post(
            "/api/v1/import/commit",
            Some(&bearer),
            &json!({"token": token, "deck": "Biology"}),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert!(reply.json()["error"]["message"]
        .as_str()
        .unwrap()
        .contains("expired"));

    // Garbage files say why.
    let reply = send(
        &t.app,
        upload("/api/v1/import/preview", &bearer, "x.apkg", b"nope", ""),
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(reply.json()["error"]["code"], "invalid");
}
