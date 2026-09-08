//! Deck management over the API: create/rename/limits/delete, the paged
//! and searchable card list, quick-add and the free cap, the rich
//! editor's create/seed/save, and the usage meter.

mod common;

use axum::http::StatusCode;
use common::*;
use flash_core::UserId;
use flash_server::service::now_ms;
use serde_json::json;

fn signed_up(t: &TestApp, name: &str) -> (UserId, String) {
    let user = member(&t.store, name, &format!("{name}@example.com"));
    (user, api_bearer(&t.store, user))
}

#[tokio::test]
async fn deck_lifecycle_create_detail_rename_limits_delete() {
    let t = AppBuilder::new("api-decks").build();
    let (_, bearer) = signed_up(&t, "flashtester");

    let blank = send(
        &t.app,
        json_post("/api/v1/decks", Some(&bearer), &json!({ "name": "  " })),
    )
    .await;
    assert_eq!(blank.status, StatusCode::BAD_REQUEST);

    let created = send(
        &t.app,
        json_post(
            "/api/v1/decks",
            Some(&bearer),
            &json!({ "name": "Pharm", "description": "NCLEX" }),
        ),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.text());
    let deck = created.json();
    assert_eq!(deck["name"], "Pharm");
    assert_eq!(deck["description"], "NCLEX");
    let id = deck["id"].as_i64().unwrap();

    // A second deck with the same name is a conflict, blank names a 400.
    let dup = send(
        &t.app,
        json_post("/api/v1/decks", Some(&bearer), &json!({ "name": "Pharm" })),
    )
    .await;
    assert_eq!(dup.status, StatusCode::CONFLICT);

    let list = send(&t.app, json_get("/api/v1/decks", Some(&bearer)))
        .await
        .json();
    assert_eq!(list["items"][0]["id"], id);

    let detail = send(
        &t.app,
        json_get(&format!("/api/v1/decks/{id}"), Some(&bearer)),
    )
    .await;
    assert_eq!(detail.status, StatusCode::OK);
    let d = detail.json();
    assert_eq!(d["deck"]["name"], "Pharm");
    assert!(d["limits"]["new_per_day"].is_null(), "inherits by default");
    assert_eq!(d["limits"]["default_new"], 20);
    assert_eq!(d["limits"]["default_reviews"], 200);
    assert_eq!(d["card_count"], 0);

    let renamed = send(
        &t.app,
        json_patch(
            &format!("/api/v1/decks/{id}"),
            Some(&bearer),
            &json!({ "name": "Pharmacology" }),
        ),
    )
    .await;
    assert_eq!(renamed.status, StatusCode::OK);
    assert_eq!(renamed.json()["name"], "Pharmacology");

    let limits = send(
        &t.app,
        json_put(
            &format!("/api/v1/decks/{id}/limits"),
            Some(&bearer),
            &json!({ "new_per_day": 5, "reviews_per_day": null }),
        ),
    )
    .await;
    assert_eq!(limits.status, StatusCode::NO_CONTENT);
    let d = send(
        &t.app,
        json_get(&format!("/api/v1/decks/{id}"), Some(&bearer)),
    )
    .await
    .json();
    assert_eq!(d["limits"]["new_per_day"], 5);
    assert!(d["limits"]["reviews_per_day"].is_null());
    let too_big = send(
        &t.app,
        json_put(
            &format!("/api/v1/decks/{id}/limits"),
            Some(&bearer),
            &json!({ "new_per_day": 99999 }),
        ),
    )
    .await;
    assert_eq!(too_big.status, StatusCode::BAD_REQUEST);

    let boosted = send(
        &t.app,
        json_post(
            &format!("/api/v1/decks/{id}/boost"),
            Some(&bearer),
            &json!({ "extra": 10 }),
        ),
    )
    .await;
    assert_eq!(boosted.json()["boost_today"], 10);

    let deleted = send(
        &t.app,
        json_delete(&format!("/api/v1/decks/{id}"), Some(&bearer)),
    )
    .await;
    assert_eq!(deleted.status, StatusCode::OK);
    assert_eq!(deleted.json()["cards_deleted"], 0);
    let gone = send(
        &t.app,
        json_get(&format!("/api/v1/decks/{id}"), Some(&bearer)),
    )
    .await;
    assert_eq!(gone.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn cards_page_search_quick_add_and_delete() {
    let t = AppBuilder::new("api-cards").build();
    let (user, bearer) = signed_up(&t, "flashtester");
    let deck = t.services.create_deck(user, "Pharm", "", now_ms()).unwrap();
    let base = format!("/api/v1/decks/{}", deck.0);

    // Quick-add with tags, then 29 more so the list paginates.
    let added = send(
        &t.app,
        json_post(
            &format!("{base}/cards"),
            Some(&bearer),
            &json!({ "front": "Warfarin antidote?", "back": "Vitamin K", "tags": ["exam-2", "cardiac"] }),
        ),
    )
    .await;
    assert_eq!(added.status, StatusCode::CREATED, "{}", added.text());
    let first_id = added.json()["card_ids"][0].as_i64().unwrap();
    let rows: Vec<(String, String, Vec<String>)> = (1..30)
        .map(|i| (format!("Question {i}"), format!("Answer {i}"), vec![]))
        .collect();
    t.services
        .create_cards_in_deck(user, deck, &rows, now_ms())
        .unwrap();

    let page1 = send(&t.app, json_get(&format!("{base}/cards"), Some(&bearer))).await;
    assert_eq!(page1.status, StatusCode::OK);
    let p = page1.json();
    assert_eq!(p["total"], 30);
    assert_eq!(p["pages"], 2);
    assert_eq!(p["page"], 1);
    assert_eq!(p["items"].as_array().unwrap().len(), 25);
    // Newest first, like the web; the quick-added card is on page 2.
    assert_eq!(p["items"][0]["front"], "Question 29");
    let page2 = send(
        &t.app,
        json_get(&format!("{base}/cards?page=2"), Some(&bearer)),
    )
    .await
    .json();
    let first = page2["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == first_id)
        .expect("the quick-added card is listed");
    assert_eq!(first["front"], "Warfarin antidote?");
    assert_eq!(first["tags"], json!(["cardiac", "exam-2"]));
    assert!(first["kind"].is_null());

    let page2 = send(
        &t.app,
        json_get(&format!("{base}/cards?page=2"), Some(&bearer)),
    )
    .await
    .json();
    assert_eq!(page2["items"].as_array().unwrap().len(), 5);
    let beyond = send(
        &t.app,
        json_get(&format!("{base}/cards?page=9"), Some(&bearer)),
    )
    .await
    .json();
    assert_eq!(beyond["page"], 2, "clamped to the last page");

    let found = send(
        &t.app,
        json_get(&format!("{base}/cards?q=warfarin"), Some(&bearer)),
    )
    .await
    .json();
    assert_eq!(found["total"], 1);
    assert_eq!(found["items"][0]["id"], first_id);

    let one = send(
        &t.app,
        json_get(&format!("/api/v1/cards/{first_id}"), Some(&bearer)),
    )
    .await;
    assert_eq!(one.status, StatusCode::OK);
    assert_eq!(one.json()["deck_id"], deck.0);

    let updated = send(
        &t.app,
        json_put(
            &format!("/api/v1/cards/{first_id}"),
            Some(&bearer),
            &json!({ "front": "Warfarin reversal?", "back": "Vitamin K" }),
        ),
    )
    .await;
    assert_eq!(updated.status, StatusCode::NO_CONTENT);

    let removed = send(
        &t.app,
        json_delete(&format!("/api/v1/cards/{first_id}"), Some(&bearer)),
    )
    .await;
    assert_eq!(removed.status, StatusCode::NO_CONTENT);
    let after = send(&t.app, json_get(&format!("{base}/cards"), Some(&bearer)))
        .await
        .json();
    assert_eq!(after["total"], 29);

    // Someone else's deck is simply not found — cards, quick-add, detail.
    let (_, ada) = signed_up(&t, "ada");
    for reply in [
        send(&t.app, json_get(&format!("{base}/cards"), Some(&ada))).await,
        send(
            &t.app,
            json_post(
                &format!("{base}/cards"),
                Some(&ada),
                &json!({ "front": "x", "back": "y" }),
            ),
        )
        .await,
        send(&t.app, json_get(&base, Some(&ada))).await,
    ] {
        assert_eq!(reply.status, StatusCode::NOT_FOUND);
    }
}

#[tokio::test]
async fn editor_creates_seeds_and_saves_notes() {
    let t = AppBuilder::new("api-editor").build();
    let (user, bearer) = signed_up(&t, "flashtester");
    let deck = t
        .services
        .create_deck(user, "Anatomy", "", now_ms())
        .unwrap();

    let meta = send(&t.app, json_get("/api/v1/editor/meta", Some(&bearer)))
        .await
        .json();
    assert_eq!(meta["note_types"].as_array().unwrap().len(), 4);
    assert_eq!(meta["hues"][0], "red");

    let created = send(
        &t.app,
        json_post(
            &format!("/api/v1/decks/{}/notes", deck.0),
            Some(&bearer),
            &json!({
                "note_type": "cloze",
                "front_html": "The {{c1::heart}} pumps {{c2::blood}}",
                "back_html": "<b>Extra</b> notes",
                "tags": ["anatomy"]
            }),
        ),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.text());
    let ids = created.json()["card_ids"].as_array().unwrap().clone();
    assert_eq!(ids.len(), 2, "one card per cloze index");
    let card_id = ids[0].as_i64().unwrap();

    let card = send(
        &t.app,
        json_get(&format!("/api/v1/cards/{card_id}"), Some(&bearer)),
    )
    .await
    .json();
    assert_eq!(card["kind"], "cloze 1");
    assert!(card["front_html"].as_str().unwrap().contains("cloze-blank"));

    let seed = send(
        &t.app,
        json_get(&format!("/api/v1/cards/{card_id}/editor"), Some(&bearer)),
    )
    .await;
    assert_eq!(seed.status, StatusCode::OK);
    let seed = seed.json();
    assert_eq!(seed["note_type"], "cloze");
    assert_eq!(seed["sibling_count"], 2);
    assert_eq!(seed["tags"], json!(["anatomy"]));

    let saved = send(
        &t.app,
        json_put(
            &format!("/api/v1/cards/{card_id}/editor"),
            Some(&bearer),
            &json!({
                "note_type": "cloze",
                "front_html": "The {{c1::heart}} pumps <script>x</script>{{c2::blood}}",
                "back_html": "",
                "tags": []
            }),
        ),
    )
    .await;
    assert_eq!(saved.status, StatusCode::OK, "{}", saved.text());
    let s = saved.json();
    assert_eq!(s["card_ids"].as_array().unwrap().len(), 2);
    assert!(
        !s["front_html"].as_str().unwrap().contains("script"),
        "sanitized on the way in"
    );

    let bad = send(
        &t.app,
        json_post(
            &format!("/api/v1/decks/{}/notes", deck.0),
            Some(&bearer),
            &json!({ "note_type": "haiku", "front_html": "x", "back_html": "y" }),
        ),
    )
    .await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
    let nocloze = send(
        &t.app,
        json_post(
            &format!("/api/v1/decks/{}/notes", deck.0),
            Some(&bearer),
            &json!({ "note_type": "cloze", "front_html": "no deletions here", "back_html": "" }),
        ),
    )
    .await;
    assert_eq!(nocloze.status, StatusCode::BAD_REQUEST);
    assert_eq!(nocloze.json()["error"]["code"], "invalid");
}
