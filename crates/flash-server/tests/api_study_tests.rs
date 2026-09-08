//! The app's daily loop over the API: Today's counts and decks, a full
//! study session (start, reveal with a typed answer, grade, end), the
//! held-by-limit empty state and the boosts that lift it, and the study
//! clock settings the reminder scheduler depends on.

mod common;

use axum::http::StatusCode;
use common::*;
use flash_core::UserId;
use flash_server::service::{now_ms, NoteInput};
use flash_store::notes::NoteType;
use serde_json::json;

fn seeded(t: &TestApp, name: &str) -> (UserId, String) {
    let user = member(&t.store, name, &format!("{name}@example.com"));
    t.services
        .create_cards(
            user,
            "Pharm",
            &[
                ("Warfarin antidote?".into(), "Vitamin K".into(), vec![]),
                ("Digoxin range?".into(), "0.5-2.0 ng/mL".into(), vec![]),
            ],
            now_ms(),
        )
        .unwrap();
    (user, api_bearer(&t.store, user))
}

async fn start(t: &TestApp, bearer: &str, scope: serde_json::Value) -> serde_json::Value {
    let reply = send(
        &t.app,
        json_post(
            "/api/v1/study/sessions",
            Some(bearer),
            &json!({ "scope": scope }),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    reply.json()
}

async fn grade(t: &TestApp, bearer: &str, sid: i64, card: i64, rating: i64) -> serde_json::Value {
    let reply = send(
        &t.app,
        json_post(
            &format!("/api/v1/study/sessions/{sid}/reviews"),
            Some(bearer),
            &json!({ "card_id": card, "rating": rating }),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    reply.json()
}

/// A review over the API is tagged with the app's platform from the
/// `X-Client` header the app sends; without it the client stays unknown.
#[tokio::test]
async fn reviews_record_the_app_platform() {
    let t = AppBuilder::new("study-platform").build();
    let (_user, bearer) = seeded(&t, "flashtester");
    let started = start(&t, &bearer, json!({ "type": "all" })).await;
    let sid = started["session_id"].as_i64().unwrap();
    let first = started["card"]["card_id"].as_i64().unwrap();

    let mut tagged = json_post(
        &format!("/api/v1/study/sessions/{sid}/reviews"),
        Some(&bearer),
        &json!({ "card_id": first, "rating": 3 }),
    );
    tagged
        .headers_mut()
        .insert("x-client", "flash-ios/1.0.0 (1)".parse().unwrap());
    let reply = send(&t.app, tagged).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let second = reply.json()["next_card"]["card_id"].as_i64().unwrap();
    grade(&t, &bearer, sid, second, 3).await;

    let clients: Vec<(String, Option<String>)> = t
        .store
        .with_conn(|conn| {
            let mut stmt = conn.prepare("SELECT source, client FROM review_log ORDER BY id")?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .unwrap();
    assert_eq!(
        clients,
        [
            ("mobile".to_string(), Some("ios".to_string())),
            ("mobile".to_string(), None)
        ]
    );
}

#[tokio::test]
async fn today_shows_counts_decks_and_the_connect_card() {
    let t = AppBuilder::new("api-today").build();
    let (_, bearer) = seeded(&t, "flashtester");

    let today = send(&t.app, json_get("/api/v1/today", Some(&bearer))).await;
    assert_eq!(today.status, StatusCode::OK);
    let json = today.json();
    assert_eq!(json["due"], 0);
    assert_eq!(json["new_available"], 2);
    assert_eq!(json["reviewed_today"], 0);
    assert_eq!(json["badge"], 0);
    assert_eq!(json["boost_today"], 0);
    assert_eq!(json["decks"][0]["name"], "Pharm");
    assert_eq!(json["decks"][0]["new"], 2);
    assert_eq!(json["show_connect_cta"], true);

    let reply = send(
        &t.app,
        json_post("/api/v1/connect/dismiss", Some(&bearer), &json!({})),
    )
    .await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT);
    let json = send(&t.app, json_get("/api/v1/today", Some(&bearer)))
        .await
        .json();
    assert_eq!(json["show_connect_cta"], false);

    let decks = send(&t.app, json_get("/api/v1/decks", Some(&bearer))).await;
    assert_eq!(decks.json()["items"][0]["name"], "Pharm");
}

#[tokio::test]
async fn a_full_session_start_reveal_grade_end() {
    let t = AppBuilder::new("api-session").build();
    let (_, bearer) = seeded(&t, "flashtester");

    let session = start(&t, &bearer, json!({ "type": "all" })).await;
    let sid = session["session_id"].as_i64().unwrap();
    assert_eq!(session["total"], 2);
    assert_eq!(session["remaining"], 2);
    assert_eq!(session["held_by_limit"], 0);
    assert_eq!(session["grading_mode"], "silent");
    assert!(session["scope_deck"].is_null());
    let first = session["card"]["card_id"].as_i64().unwrap();
    assert_eq!(session["card"]["front"], "Warfarin antidote?");
    assert_eq!(session["card"]["wants_typing"], false);
    assert_eq!(session["card"]["preload_media"], json!([]));

    let reveal = send(
        &t.app,
        json_post(
            &format!("/api/v1/study/sessions/{sid}/reveal"),
            Some(&bearer),
            &json!({ "card_id": first }),
        ),
    )
    .await;
    assert_eq!(reveal.status, StatusCode::OK);
    assert_eq!(reveal.json()["back"], "Vitamin K");
    assert!(reveal.json()["typed_diff"].is_null());

    let after_first = grade(&t, &bearer, sid, first, 3).await;
    assert_eq!(after_first["remaining"], 1);
    let second = after_first["next_card"]["card_id"].as_i64().unwrap();
    assert_ne!(second, first);

    let after_second = grade(&t, &bearer, sid, second, 4).await;
    assert_eq!(after_second["remaining"], 0);
    assert!(after_second["next_card"].is_null());
    assert_eq!(after_second["held_by_limit"], 0);

    let end = send(
        &t.app,
        json_post(
            &format!("/api/v1/study/sessions/{sid}/end"),
            Some(&bearer),
            &json!({}),
        ),
    )
    .await;
    assert_eq!(end.status, StatusCode::OK);
    let summary = end.json();
    assert_eq!(summary["reviewed"], 2);
    assert_eq!(summary["good"], 1);
    assert_eq!(summary["easy"], 1);

    // The reviews carry the app's provenance and count toward today.
    let today = send(&t.app, json_get("/api/v1/today", Some(&bearer)))
        .await
        .json();
    assert_eq!(today["reviewed_today"], 2);

    // A bad rating is refused in the envelope.
    let bad = send(
        &t.app,
        json_post(
            &format!("/api/v1/study/sessions/{sid}/reviews"),
            Some(&bearer),
            &json!({ "card_id": first, "rating": 9 }),
        ),
    )
    .await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
    assert_eq!(bad.json()["error"]["code"], "invalid");
}

#[tokio::test]
async fn typed_cards_ask_for_input_and_diff_the_answer() {
    let t = AppBuilder::new("api-typed").build();
    let user = member(&t.store, "FlashTester", "flashtester@example.com");
    let bearer = api_bearer(&t.store, user);
    let deck = t.services.create_deck(user, "Typed", "", now_ms()).unwrap();
    t.services
        .create_note(
            user,
            deck,
            &NoteInput {
                note_type: NoteType::BasicTyped,
                front_html: "Capital of France?".into(),
                back_html: "Paris".into(),
                tags: vec![],
            },
            now_ms(),
        )
        .unwrap();

    let session = start(&t, &bearer, json!({ "type": "deck", "id": deck.0 })).await;
    assert_eq!(session["scope_deck"], deck.0);
    assert_eq!(session["card"]["wants_typing"], true);
    let sid = session["session_id"].as_i64().unwrap();
    let card = session["card"]["card_id"].as_i64().unwrap();

    let reveal = send(
        &t.app,
        json_post(
            &format!("/api/v1/study/sessions/{sid}/reveal"),
            Some(&bearer),
            &json!({ "card_id": card, "typed": "Parris" }),
        ),
    )
    .await
    .json();
    assert_eq!(reveal["back"], "Paris");
    let spans = reveal["typed_diff"].as_array().unwrap();
    let text: String = spans.iter().map(|s| s["text"].as_str().unwrap()).collect();
    assert_eq!(text, "Parris");
    assert!(
        spans.iter().any(|s| s["ok"] == false),
        "the extra r is wrong"
    );
}

#[tokio::test]
async fn held_by_limit_explains_the_empty_queue_and_boosts_lift_it() {
    let t = AppBuilder::new("api-held").build();
    let (user, bearer) = seeded(&t, "flashtester");
    t.services.set_daily_limits(user, 1, 200).unwrap();

    let session = start(&t, &bearer, json!({ "type": "all" })).await;
    assert_eq!(session["total"], 1);
    assert_eq!(session["held_by_limit"], 1);
    let sid = session["session_id"].as_i64().unwrap();
    let card = session["card"]["card_id"].as_i64().unwrap();
    let done = grade(&t, &bearer, sid, card, 3).await;
    assert!(done["next_card"].is_null());
    assert_eq!(done["held_by_limit"], 1);

    // Account boost from the empty state.
    let reply = send(
        &t.app,
        json_post("/api/v1/study/boost", Some(&bearer), &json!({ "extra": 5 })),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.json()["boost_today"], 5);
    let session = start(&t, &bearer, json!({ "type": "all" })).await;
    assert_eq!(session["total"], 1);
    assert_eq!(session["held_by_limit"], 0);

    // Per-deck boost, and the range check.
    let deck = t.store.find_deck_by_name(user, "Pharm").unwrap().unwrap();
    let reply = send(
        &t.app,
        json_post(
            &format!("/api/v1/decks/{}/boost", deck.0),
            Some(&bearer),
            &json!({ "extra": 10 }),
        ),
    )
    .await;
    assert_eq!(reply.json()["boost_today"], 10);
    let reply = send(
        &t.app,
        json_post("/api/v1/study/boost", Some(&bearer), &json!({ "extra": 0 })),
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn study_settings_validate_and_persist() {
    let t = AppBuilder::new("api-studysettings").build();
    let user = member(&t.store, "FlashTester", "flashtester@example.com");
    let bearer = api_bearer(&t.store, user);

    let bad = send(
        &t.app,
        json_put(
            "/api/v1/settings/study",
            Some(&bearer),
            &json!({ "timezone": "Mars/Olympus" }),
        ),
    )
    .await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
    let bad = send(
        &t.app,
        json_put(
            "/api/v1/settings/study",
            Some(&bearer),
            &json!({ "desired_retention": 0.5 }),
        ),
    )
    .await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);

    let ok = send(
        &t.app,
        json_put(
            "/api/v1/settings/study",
            Some(&bearer),
            &json!({ "timezone": "Europe/Madrid", "day_cutoff_hour": 3 }),
        ),
    )
    .await;
    assert_eq!(ok.status, StatusCode::NO_CONTENT);
    let settings = t.store.get_settings(user).unwrap();
    assert_eq!(settings.timezone, "Europe/Madrid");
    assert_eq!(settings.day_cutoff_hour, 3);
    assert_eq!(settings.desired_retention, 0.9, "untouched fields stay");
}
