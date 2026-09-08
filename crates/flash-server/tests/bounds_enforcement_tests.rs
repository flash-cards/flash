//! The limits in `flash_server::bounds` bite where they are meant to:
//! at the edge (a request past a bound is a 400 with a sentence naming
//! the field), at the store (a tag list the edge forgot to bound is
//! refused where it would be written), and on stored state (decks and
//! media objects per account). bounded_input_tests proves every request
//! field names a bound; this proves the bounds do something.

mod common;

use axum::http::StatusCode;
use common::*;
use flash_core::{validate_card_text, UserId};
use flash_server::bounds;
use flash_server::service::now_ms;
use flash_store::media::MediaKind;
use serde_json::json;

fn signed_up(t: &TestApp, name: &str) -> (UserId, String) {
    let user = member(&t.store, name, &format!("{name}@example.com"));
    (user, api_bearer(&t.store, user))
}

#[tokio::test]
async fn a_request_past_a_bound_is_a_400_naming_the_field() {
    let t = AppBuilder::new("bounds-edge").build();
    let (user, bearer) = signed_up(&t, "flashtester");
    let deck = t.store.create_deck(user, "Pharm", "", now_ms()).unwrap();

    let long_name = "x".repeat(bounds::DECK_NAME + 1);
    let reply = send(
        &t.app,
        json_post(
            "/api/v1/decks",
            Some(&bearer),
            &json!({ "name": long_name }),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert!(
        reply.text().contains("deck name is longer than"),
        "{}",
        reply.text()
    );

    let many_tags: Vec<String> = (0..=bounds::TAGS_PER_CARD).map(|i| i.to_string()).collect();
    let reply = send(
        &t.app,
        json_post(
            &format!("/api/v1/decks/{}/cards", deck.0),
            Some(&bearer),
            &json!({ "front": "q", "back": "a", "tags": many_tags }),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert!(
        reply.text().contains("more than 50 tags"),
        "{}",
        reply.text()
    );

    let long_search = "x".repeat(bounds::SEARCH + 1);
    let reply = send(
        &t.app,
        json_get(
            &format!("/api/v1/decks/{}/cards?q={long_search}", deck.0),
            Some(&bearer),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.text());

    // Within the bounds, the same requests work.
    let reply = send(
        &t.app,
        json_post(
            &format!("/api/v1/decks/{}/cards", deck.0),
            Some(&bearer),
            &json!({ "front": "q", "back": "a", "tags": ["exam-2"] }),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text());
}

#[test]
fn the_store_refuses_a_tag_list_the_edge_forgot_to_bound() {
    let t = AppBuilder::new("bounds-store").build();
    let user = member(&t.store, "Ada", "ada@example.com");
    let deck = t.store.create_deck(user, "D", "", now_ms()).unwrap();
    let text = validate_card_text("q", "a").unwrap();

    let too_many: Vec<String> = (0..=bounds::TAGS_PER_CARD).map(|i| i.to_string()).collect();
    let err = t
        .store
        .create_cards(user, deck, &[(text.clone(), too_many)], None, now_ms())
        .unwrap_err();
    assert!(err.to_string().contains("at most 50 tags"), "{err}");

    let too_long = vec!["x".repeat(bounds::TAG + 1)];
    let err = t
        .store
        .create_cards(user, deck, &[(text, too_long)], None, now_ms())
        .unwrap_err();
    assert!(err.to_string().contains("at most 64 bytes"), "{err}");
}

#[test]
fn an_account_holds_at_most_the_deck_cap() {
    let t = AppBuilder::new("bounds-decks").build();
    let user = member(&t.store, "Ada", "ada@example.com");
    for i in 0..bounds::DECKS_PER_USER {
        t.services
            .create_deck(user, &format!("Deck {i}"), "", now_ms())
            .unwrap();
    }
    let err = t
        .services
        .create_deck(user, "One more", "", now_ms())
        .unwrap_err();
    assert!(err.to_string().contains("at most 500 decks"), "{err}");
    // An import naming a new destination makes its deck through the
    // same store method, so it meets the same cap (the third review
    // found it walking around a service-level check).
    let rows = flash_store::import::parse_csv(b"front,back\nq,a\n")
        .unwrap()
        .rows;
    let err = t
        .services
        .import_cards(user, "Imported", rows, false, None, now_ms())
        .unwrap_err();
    assert!(err.to_string().contains("at most 500 decks"), "{err}");
    assert_eq!(t.store.deck_count(user).unwrap(), bounds::DECKS_PER_USER);
}

#[test]
fn an_account_holds_at_most_the_media_object_cap() {
    let t = AppBuilder::new("bounds-media").build();
    let user = member(&t.store, "Ada", "ada@example.com");
    for i in 0..bounds::MEDIA_OBJECTS_PER_USER {
        t.store
            .create_media(
                user,
                &format!("{i:064x}"),
                "x.png",
                "image/png",
                MediaKind::Image,
                12,
                now_ms(),
            )
            .unwrap();
    }
    // A real, tiny, valid upload is refused on count, not bytes.
    let png: &[u8] = &[
        0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0x0D, b'I', b'H', b'D', b'R',
    ];
    let disk =
        flash_server::media_store::DiskStore::new(flash_server::media::media_dir(&t.data_dir));
    let err =
        flash_server::media::ingest_media(&t.services, &disk, user, false, "x.png", png, now_ms())
            .unwrap_err();
    assert!(err.to_string().contains("media file limit"), "{err}");
    let _ = std::fs::remove_dir_all(&t.data_dir);
}
