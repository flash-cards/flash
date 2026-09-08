//! Cross-user isolation, exhaustively: every `Services` method that takes
//! a resource id is called by one user with another user's ids and must
//! refuse, leaving the owner's rows untouched. A source scan over
//! `service.rs` lists every such method and fails when one is missing
//! from the coverage list below, so a new method cannot skip the check.
//! The same is then confirmed at the HTTP surface for the API.

use std::path::Path;

use axum::http::StatusCode;
use flash_core::queue::StudyScope;
use flash_core::{CardId, DeckId, Rating, SessionId};
use flash_scan::{compact, crate_at, parse_source, FnKind, Vis};
use flash_server::api::MOBILE_CLIENT_ID;
use flash_server::auth::hash_token;
use flash_server::service::{now_ms, CardFront, NoteInput, ReviewOrigin, Services};
use flash_server::testing::{
    api_bearer, json_delete, json_get, json_post, member, send, AppBuilder,
};
use flash_store::notes::NoteType;

/// Every `Services` method with a resource-id parameter, each exercised
/// below as the wrong user.
const COVERED: &[&str] = &[
    "deck_limits",
    "set_deck_limits",
    "rename_deck",
    "boost_new_today",
    "boost_today",
    "start_session",
    "submit_review",
    "reveal",
    "study_card",
    "reveal_card",
    "session_scope",
    "end_session",
    "queue_counts",
    "deck_by_id",
    "deck_detail",
    "create_cards_in_deck",
    "deck_cards_page",
    "card",
    "import_cards",
    "update_card",
    "delete_card",
    "create_note",
    "save_card_editor",
    "editor_seed",
    "list_cards_page",
    "delete_deck",
    "list_cards",
];

const ID_TYPES: &[&str] = &[
    "DeckId",
    "CardId",
    "NoteId",
    "MediaId",
    "SessionId",
    "StudyScope",
];

/// The public `Services` methods, in any file of the crate, whose
/// parameters name a resource id (a returned id is not a resource the
/// caller chose: `create_deck` returns one).
fn methods_with_resource_ids() -> Vec<String> {
    let mut names = Vec::new();
    for file in crate_at(Path::new(env!("CARGO_MANIFEST_DIR"))).source_files() {
        for function in &file.functions {
            let on_services = matches!(&function.kind, FnKind::Method { owner, trait_: None } if owner == "Services");
            let takes_id = function
                .params
                .iter()
                .any(|p| p.ty.idents.iter().any(|i| ID_TYPES.contains(&i.as_str())));
            if on_services && function.vis == Vis::Pub && takes_id {
                names.push(function.name.clone());
            }
        }
    }
    names.sort();
    names.dedup();
    names
}

#[test]
fn every_service_method_with_a_resource_id_is_covered() {
    let missing: Vec<String> = methods_with_resource_ids()
        .into_iter()
        .filter(|name| !COVERED.contains(&name.as_str()))
        .collect();
    assert!(
        missing.is_empty(),
        "service methods taking a resource id that the isolation test does not call:\n{}",
        missing.join("\n")
    );
    let stale: Vec<&&str> = COVERED
        .iter()
        .filter(|name| !methods_with_resource_ids().iter().any(|m| m == *name))
        .collect();
    assert!(
        stale.is_empty(),
        "covered names that are no longer methods: {stale:?}"
    );
    // A name on the list is a claim that the test below calls it as the
    // wrong user; the claim is checked against this file's own source,
    // so listing a method is not the same as covering it.
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/isolation_tests.rs");
    let this_file = parse_source(
        "isolation_tests.rs",
        &std::fs::read_to_string(path).expect("this test's source"),
    );
    // A call sits either in parsed code or inside an `assert!`, whose
    // body is tokens; both are read.
    let called_with_b = |name: &str| {
        this_file
            .method_calls
            .iter()
            .any(|m| m.method == name && m.args.first().map(String::as_str) == Some("b"))
            || this_file
                .macros
                .iter()
                .any(|m| compact(&m.tokens).contains(&format!(".{name}(b,")))
    };
    let unexercised: Vec<&&str> = COVERED.iter().filter(|name| !called_with_b(name)).collect();
    assert!(
        unexercised.is_empty(),
        "covered names never called with the wrong user first (`s.<name>(b, …)`): {unexercised:?}"
    );
}

struct Owned {
    deck: DeckId,
    card: CardId,
    session: SessionId,
}

fn seed(services: &Services, owner: flash_core::UserId) -> Owned {
    let now = now_ms();
    let deck = services.create_deck(owner, "Owned", "", now).unwrap();
    let cards = services
        .create_cards_in_deck(
            owner,
            deck,
            &[("front".into(), "back".into(), vec!["tag".into()])],
            now,
        )
        .unwrap();
    let session = services
        .start_session(owner, StudyScope::Deck(deck), now)
        .unwrap()
        .session_id;
    Owned {
        deck,
        card: cards[0],
        session,
    }
}

fn note_input() -> NoteInput {
    NoteInput {
        note_type: NoteType::Basic,
        front_html: "<p>x</p>".into(),
        back_html: "<p>y</p>".into(),
        tags: vec![],
    }
}

#[tokio::test]
async fn another_user_cannot_touch_owned_resources_through_services() {
    let t = AppBuilder::new("isolation-services").build();
    let a = member(&t.store, "FlashTester", "flashtester@example.com");
    let b = member(&t.store, "Other", "other@example.com");
    let s = &t.services;
    let now = now_ms();
    let owned = seed(s, a);
    let before_cards = s.list_cards(a, Some(owned.deck), None, 500).unwrap();
    let before_decks = s.list_decks(a, now).unwrap();
    let before_state = t.store.get_card_state(a, owned.card).unwrap();

    // Decks.
    assert!(s.deck_limits(b, owned.deck).is_err());
    assert!(s.set_deck_limits(b, owned.deck, Some(1), Some(1)).is_err());
    assert!(s.rename_deck(b, owned.deck, "stolen").is_err());
    assert!(s.boost_new_today(b, Some(owned.deck), 5, now).is_err());
    assert!(s.boost_today(b, Some(owned.deck), now).is_err());
    assert!(s.deck_by_id(b, owned.deck, now).is_err());
    assert!(s.deck_detail(b, owned.deck, now).is_err());
    assert!(s.deck_cards_page(b, owned.deck, None, 1, 25, now).is_err());
    assert!(s
        .create_cards_in_deck(b, owned.deck, &[("f".into(), "b".into(), vec![])], now)
        .is_err());
    assert!(s.create_note(b, owned.deck, &note_input(), now).is_err());
    assert!(s.delete_deck(b, owned.deck).is_err());
    // Listing and counting under another user's deck yields nothing.
    assert!(s
        .list_cards(b, Some(owned.deck), None, 50)
        .unwrap()
        .is_empty());
    if let Ok((rows, total)) = s.list_cards_page(b, owned.deck, None, 1, 25) {
        assert!(rows.is_empty() && total == 0);
    }
    let counts = s
        .queue_counts(b, &StudyScope::Deck(owned.deck), now)
        .unwrap();
    assert_eq!((counts.due, counts.new_available), (0, 0));
    assert!(s
        .start_session(b, StudyScope::Deck(owned.deck), now)
        .is_err());

    // Cards.
    assert!(s.card(b, owned.card).is_err());
    assert!(s.reveal(b, owned.card).is_err());
    assert!(s.reveal_card(b, owned.card, None).is_err());
    assert!(s
        .study_card(
            b,
            CardFront {
                card_id: owned.card,
                front: String::new()
            },
            Some(owned.card)
        )
        .is_err());
    assert!(s.update_card(b, owned.card, "x", "y", now).is_err());
    assert!(s
        .save_card_editor(b, owned.card, &note_input(), now)
        .is_err());
    assert!(s.editor_seed(b, owned.card).is_err());
    assert!(s.delete_card(b, owned.card, now).is_err());

    // Sessions and reviews.
    assert!(s.session_scope(b, owned.session).is_err());
    assert!(s
        .submit_review(
            b,
            owned.session,
            owned.card,
            Rating::Good,
            ReviewOrigin::WEB,
            now
        )
        .is_err());
    assert!(s.end_session(b, owned.session, now).is_err());

    // Import with a foreign media id: the id must not link.
    let foreign_media = t
        .store
        .create_media(
            a,
            "0000000000000000000000000000000000000000000000000000000000000000",
            "pic.png",
            "image/png",
            flash_store::media::MediaKind::Image,
            4,
            now,
        )
        .unwrap();
    let media_map = [("pic.png".to_string(), foreign_media)]
        .into_iter()
        .collect();
    let row = flash_store::import::ImportRow {
        reviews: vec![],
        media: vec![],
        front: "front".into(),
        back: "b".into(),
        front_html: Some(flash_store::richtext::sanitize_with_media(
            r#"<p><img src="pic.png"></p>"#,
        )),
        back_html: None,
        tags: vec![],
        deck: None,
        suspended: false,
        cloze_text: None,
        cloze_index: None,
        type_answer: None,
    };
    let _ = s.import_cards(b, "Imported", vec![row], false, Some(&media_map), now);
    let linked: i64 = t
        .store
        .with_conn(|conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM card_media cm JOIN cards c ON c.id = cm.card_id
                 WHERE c.user_id = ?1 AND cm.media_id = ?2",
                rusqlite::params![b.raw(), foreign_media.0],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(
        linked, 0,
        "another user's media must not link into B's cards"
    );

    // Nothing of A's moved.
    assert_eq!(
        s.list_cards(a, Some(owned.deck), None, 500).unwrap(),
        before_cards
    );
    assert_eq!(s.list_decks(a, now).unwrap(), before_decks);
    assert_eq!(t.store.get_card_state(a, owned.card).unwrap(), before_state);
}

#[tokio::test]
async fn another_users_bearer_is_turned_away_by_the_api() {
    let t = AppBuilder::new("isolation-api").build();
    let a = member(&t.store, "FlashTester", "flashtester@example.com");
    let b = member(&t.store, "Other", "other@example.com");
    let owned = seed(&t.services, a);
    let a_bearer = api_bearer(&t.store, a);
    let b_bearer = api_bearer(&t.store, b);

    for path in [
        format!("/api/v1/decks/{}", owned.deck.0),
        format!("/api/v1/decks/{}/cards", owned.deck.0),
        format!("/api/v1/cards/{}", owned.card.0),
        format!("/api/v1/cards/{}/editor", owned.card.0),
    ] {
        let reply = send(&t.app, json_get(&path, Some(&b_bearer))).await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{path}");
    }
    let review = send(
        &t.app,
        json_post(
            &format!("/api/v1/study/sessions/{}/reviews", owned.session.0),
            Some(&b_bearer),
            &serde_json::json!({"card_id": owned.card.0, "rating": 3}),
        ),
    )
    .await;
    assert!(review.status.is_client_error(), "{}", review.status);
    let deleted = send(
        &t.app,
        json_delete(&format!("/api/v1/cards/{}", owned.card.0), Some(&b_bearer)),
    )
    .await;
    assert_eq!(deleted.status, StatusCode::NOT_FOUND);
    assert!(t.services.card(a, owned.card).is_ok());

    // B cannot revoke A's app session by id; A's bearer keeps working.
    let a_token = t
        .store
        .list_tokens_for_user(a, MOBILE_CLIENT_ID, now_ms())
        .unwrap()[0]
        .id;
    let revoke = send(
        &t.app,
        json_delete(&format!("/api/v1/auth/sessions/{a_token}"), Some(&b_bearer)),
    )
    .await;
    assert_eq!(revoke.status, StatusCode::NOT_FOUND);
    assert!(t
        .store
        .lookup_api_token(&hash_token(&a_bearer), now_ms())
        .unwrap()
        .is_some());
}
