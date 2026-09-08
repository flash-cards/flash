//! Notes: the editable unit behind sibling cards. Covers creation for
//! every note type, in-place regeneration (scheduling survives, vanished
//! slots soft-delete, new slots insert fresh), adoption of note-less and
//! imported-cloze cards, the MCP detach rule, tag/media replacement, cap
//! enforcement, and deck-page pagination.

use flash_core::{validate_card_text, CardState, NoteId, Phase, UserId};
use flash_store::notes::{GeneratedCard, NoteType};
use flash_store::richtext::{sanitize_with_media, SanitizedHtml};
use flash_store::NoteSave;
use flash_store::{CardExtras, CardRow, Store, StoreError};

const NOW: i64 = 1_700_000_000_000;

/// The tests write fields as strings; the editor path sanitizes them
/// first, so the tests do the same before generation.
fn generate_cards(kind: NoteType, front: &str, back: &str) -> Result<Vec<GeneratedCard>, String> {
    flash_store::notes::generate_cards(kind, sanitized(front), sanitized(back))
}

/// Sanitized and leaked: a `NoteSave` borrows its fields for the test's
/// lifetime, and a test process does not outlive its leaks.
fn sanitized(html: &str) -> &'static SanitizedHtml {
    Box::leak(Box::new(sanitize_with_media(html)))
}

fn store_with_user() -> (Store, UserId) {
    let store = Store::open_in_memory().expect("open + migrate");
    let user = store
        .create_user("Test User", Some("t@example.com"), "member", NOW)
        .unwrap();
    (store, user)
}

fn save<'a>(
    kind: NoteType,
    front: &'a str,
    back: &'a str,
    generated: &'a [flash_store::notes::GeneratedCard],
    tags: &'a [String],
    cap: Option<u32>,
) -> NoteSave<'a> {
    NoteSave {
        note_type: kind,
        front_html: sanitized(front),
        back_html: sanitized(back),
        generated,
        tags,
        cap,
        now_ms: NOW,
    }
}

fn tags(list: &[&str]) -> Vec<String> {
    list.iter().map(|t| t.to_string()).collect()
}

fn live_cards(store: &Store, user: UserId, deck: flash_core::DeckId) -> Vec<CardRow> {
    let mut cards = store.list_cards(user, Some(deck), None, 100).unwrap();
    cards.sort_by_key(|c| c.id);
    cards
}

#[test]
fn schema_is_at_v9_with_note_columns() {
    let (store, user) = store_with_user();
    let deck = store.create_deck(user, "D", "", NOW).unwrap();
    let ids = store
        .create_cards(
            user,
            deck,
            &[(validate_card_text("a", "b").unwrap(), vec![])],
            None,
            NOW,
        )
        .unwrap();
    let card = store.get_card(user, ids[0]).unwrap().unwrap();
    assert_eq!(card.note_id, None, "quick-add cards are note-less");
    assert_eq!(card.ord, 0);
    assert_eq!(card.cloze_index, None);
}

#[test]
fn basic_note_creates_one_card_with_tags_and_html() {
    let (store, user) = store_with_user();
    let deck = store.create_deck(user, "D", "", NOW).unwrap();
    let gen = generate_cards(NoteType::Basic, "<b>Q</b>", "A").unwrap();
    let (note, ids) = store
        .create_note(
            user,
            deck,
            &save(
                NoteType::Basic,
                "<b>Q</b>",
                "A",
                &gen,
                &tags(&["Exam", "geo"]),
                None,
            ),
        )
        .unwrap();
    assert_eq!(ids.len(), 1);
    let card = store.get_card(user, ids[0]).unwrap().unwrap();
    assert_eq!(card.note_id, Some(note));
    assert_eq!(card.front, "Q");
    assert_eq!(card.front_html.as_deref(), Some("<b>Q</b>"));
    assert_eq!(card.tags, vec!["exam", "geo"], "tags lowercased, sorted");
    let row = store.get_note(user, note).unwrap().unwrap();
    assert_eq!(row.note_type, NoteType::Basic);
    assert_eq!(row.front_html, "<b>Q</b>");
    assert_eq!(row.cards, vec![(0, ids[0])]);
    assert_eq!(row.tags, vec!["exam", "geo"]);
    assert_eq!(store.note_for_card(user, ids[0]).unwrap().unwrap().id, note);
}

#[test]
fn reversed_and_typed_notes() {
    let (store, user) = store_with_user();
    let deck = store.create_deck(user, "D", "", NOW).unwrap();
    let gen = generate_cards(NoteType::BasicReversed, "Q", "A").unwrap();
    let (_, ids) = store
        .create_note(
            user,
            deck,
            &save(NoteType::BasicReversed, "Q", "A", &gen, &[], None),
        )
        .unwrap();
    assert_eq!(ids.len(), 2);
    let second = store.get_card(user, ids[1]).unwrap().unwrap();
    assert_eq!(
        (second.ord, second.front.as_str(), second.back.as_str()),
        (1, "A", "Q")
    );

    let gen = generate_cards(NoteType::BasicTyped, "Q", "A").unwrap();
    let (_, ids) = store
        .create_note(
            user,
            deck,
            &save(NoteType::BasicTyped, "Q", "A", &gen, &[], None),
        )
        .unwrap();
    let card = store.get_card(user, ids[0]).unwrap().unwrap();
    assert_eq!(card.type_answer.as_deref(), Some("A"));
}

#[test]
fn cloze_note_fans_out_and_exports_as_one_note() {
    let (store, user) = store_with_user();
    let deck = store.create_deck(user, "D", "", NOW).unwrap();
    let text = "{{c1::Paris}} is in {{c2::France}}; {{c3::Europe}}";
    let gen = generate_cards(NoteType::Cloze, text, "Extra").unwrap();
    let (note, ids) = store
        .create_note(
            user,
            deck,
            &save(NoteType::Cloze, text, "Extra", &gen, &[], None),
        )
        .unwrap();
    assert_eq!(ids.len(), 3);
    for (i, id) in ids.iter().enumerate() {
        let card = store.get_card(user, *id).unwrap().unwrap();
        assert_eq!(card.ord as usize, i);
        assert_eq!(card.cloze_index, Some(i as u32 + 1));
        assert_eq!(card.note_id, Some(note));
        assert!(card.front_html.as_deref().unwrap().contains("cloze-blank"));
    }
    let exported = store.export_cards(user).unwrap();
    let sources: std::collections::HashSet<_> = exported
        .iter()
        .map(|c| c.cloze.as_ref().unwrap().0.clone())
        .collect();
    assert_eq!(sources.len(), 1, "siblings share one cloze source");
}

#[test]
fn editing_a_cloze_note_keeps_scheduling_on_surviving_slots() {
    let (store, user) = store_with_user();
    let deck = store.create_deck(user, "D", "", NOW).unwrap();
    let text = "{{c1::A}} {{c2::B}} {{c3::C}}";
    let gen = generate_cards(NoteType::Cloze, text, "").unwrap();
    let (note, ids) = store
        .create_note(
            user,
            deck,
            &save(NoteType::Cloze, text, "", &gen, &[], None),
        )
        .unwrap();
    // Review c1 and c3 so they carry state worth preserving.
    let mut reviewed = CardState::new_card(NOW);
    reviewed.phase = Phase::Review;
    reviewed.stability = Some(12.5);
    reviewed.difficulty = Some(5.0);
    reviewed.due_ms = NOW + 86_400_000;
    reviewed.last_review_ms = Some(NOW);
    reviewed.reps = 3;
    store.update_card_state(user, ids[0], &reviewed).unwrap();
    store.update_card_state(user, ids[2], &reviewed).unwrap();

    // Drop c2, reword c1, add c4.
    let text2 = "{{c1::A2}} B {{c3::C}} {{c4::D}}";
    let gen2 = generate_cards(NoteType::Cloze, text2, "").unwrap();
    let new_ids = store
        .update_note(
            user,
            note,
            &save(NoteType::Cloze, text2, "", &gen2, &[], None),
        )
        .unwrap();
    assert_eq!(new_ids.len(), 3);
    assert_eq!(new_ids[0], ids[0], "ord 0 updated in place");
    assert_eq!(new_ids[1], ids[2], "ord 2 updated in place");
    assert!(!ids.contains(&new_ids[2]), "ord 3 is a fresh card");

    assert_eq!(store.get_card_state(user, ids[0]).unwrap(), reviewed);
    assert_eq!(store.get_card_state(user, ids[2]).unwrap(), reviewed);
    assert_eq!(
        store.get_card_state(user, new_ids[2]).unwrap().phase,
        Phase::New
    );
    assert!(
        store.get_card(user, ids[1]).unwrap().is_none(),
        "vanished slot is soft-deleted"
    );
    let c1 = store.get_card(user, ids[0]).unwrap().unwrap();
    assert_eq!(c1.back, "A2");
    assert_eq!(c1.front, "[...] B C D");
    let live = live_cards(&store, user, deck);
    assert_eq!(live.len(), 3);
    let row = store.get_note(user, note).unwrap().unwrap();
    assert_eq!(
        row.cards.iter().map(|(o, _)| *o).collect::<Vec<_>>(),
        vec![0, 2, 3]
    );
}

#[test]
fn changing_note_type_adds_or_removes_slots() {
    let (store, user) = store_with_user();
    let deck = store.create_deck(user, "D", "", NOW).unwrap();
    let gen = generate_cards(NoteType::Basic, "Q", "A").unwrap();
    let (note, ids) = store
        .create_note(
            user,
            deck,
            &save(NoteType::Basic, "Q", "A", &gen, &[], None),
        )
        .unwrap();
    let gen = generate_cards(NoteType::BasicReversed, "Q", "A").unwrap();
    let ids2 = store
        .update_note(
            user,
            note,
            &save(NoteType::BasicReversed, "Q", "A", &gen, &[], None),
        )
        .unwrap();
    assert_eq!(ids2[0], ids[0]);
    assert_eq!(ids2.len(), 2);
    let gen = generate_cards(NoteType::BasicTyped, "Q", "A").unwrap();
    let ids3 = store
        .update_note(
            user,
            note,
            &save(NoteType::BasicTyped, "Q", "A", &gen, &[], None),
        )
        .unwrap();
    assert_eq!(ids3, vec![ids[0]]);
    assert!(store.get_card(user, ids2[1]).unwrap().is_none());
    assert_eq!(
        store
            .get_card(user, ids[0])
            .unwrap()
            .unwrap()
            .type_answer
            .as_deref(),
        Some("A")
    );
    // Reversed again: ord 1 comes back as a brand-new card, not the old row.
    let gen = generate_cards(NoteType::BasicReversed, "Q", "A").unwrap();
    let ids4 = store
        .update_note(
            user,
            note,
            &save(NoteType::BasicReversed, "Q", "A", &gen, &[], None),
        )
        .unwrap();
    assert_ne!(ids4[1], ids2[1]);
}

#[test]
fn tags_and_media_are_replaced_on_every_save() {
    let (store, user) = store_with_user();
    let deck = store.create_deck(user, "D", "", NOW).unwrap();
    let m1 = store
        .create_media(
            user,
            "a".repeat(64).as_str(),
            "a.png",
            "image/png",
            flash_store::media::MediaKind::Image,
            10,
            NOW,
        )
        .unwrap();
    let m2 = store
        .create_media(
            user,
            "b".repeat(64).as_str(),
            "b.mp3",
            "audio/mpeg",
            flash_store::media::MediaKind::Audio,
            10,
            NOW,
        )
        .unwrap();
    let front = format!("<img src=\"/media/{}\"> Q", m1.0);
    let gen = generate_cards(NoteType::Basic, &front, "A").unwrap();
    let (note, ids) = store
        .create_note(
            user,
            deck,
            &save(NoteType::Basic, &front, "A", &gen, &tags(&["one"]), None),
        )
        .unwrap();
    assert_eq!(
        store.get_card(user, ids[0]).unwrap().unwrap().tags,
        vec!["one"]
    );
    let linked: Vec<_> = store
        .media_for_export(user)
        .unwrap()
        .into_iter()
        .map(|m| m.id)
        .collect();
    assert_eq!(linked, vec![m1]);

    let back = format!("<audio controls src=\"/media/{}\"></audio>", m2.0);
    let gen = generate_cards(NoteType::Basic, "Q", &back).unwrap();
    store
        .update_note(
            user,
            note,
            &save(
                NoteType::Basic,
                "Q",
                &back,
                &gen,
                &tags(&["two", "three"]),
                None,
            ),
        )
        .unwrap();
    let card = store.get_card(user, ids[0]).unwrap().unwrap();
    assert_eq!(card.tags, vec!["three", "two"], "old tag gone");
    let linked: Vec<_> = store
        .media_for_export(user)
        .unwrap()
        .into_iter()
        .map(|m| m.id)
        .collect();
    assert_eq!(linked, vec![m2], "old media link gone");
}

#[test]
fn foreign_media_ids_never_link() {
    let (store, user) = store_with_user();
    let other = store.create_user("O", None, "member", NOW).unwrap();
    let theirs = store
        .create_media(
            other,
            "c".repeat(64).as_str(),
            "c.png",
            "image/png",
            flash_store::media::MediaKind::Image,
            10,
            NOW,
        )
        .unwrap();
    let deck = store.create_deck(user, "D", "", NOW).unwrap();
    let front = format!("<img src=\"/media/{}\">", theirs.0);
    let gen = generate_cards(NoteType::Basic, &front, "A").unwrap();
    store
        .create_note(
            user,
            deck,
            &save(NoteType::Basic, &front, "A", &gen, &[], None),
        )
        .unwrap();
    assert!(store.media_for_export(user).unwrap().is_empty());
}

#[test]
fn cap_counts_only_newly_inserted_slots() {
    let (store, user) = store_with_user();
    let deck = store.create_deck(user, "D", "", NOW).unwrap();
    let gen = generate_cards(NoteType::BasicReversed, "Q", "A").unwrap();
    let err = store
        .create_note(
            user,
            deck,
            &save(NoteType::BasicReversed, "Q", "A", &gen, &[], Some(1)),
        )
        .unwrap_err();
    assert!(matches!(
        err,
        StoreError::CapExceeded { current: 0, cap: 1 }
    ));
    assert!(
        live_cards(&store, user, deck).is_empty(),
        "atomic: nothing inserted"
    );

    let gen = generate_cards(NoteType::Basic, "Q", "A").unwrap();
    let (note, _) = store
        .create_note(
            user,
            deck,
            &save(NoteType::Basic, "Q", "A", &gen, &[], Some(1)),
        )
        .unwrap();
    // Editing in place at the cap is fine (no new rows).
    let gen = generate_cards(NoteType::Basic, "Q2", "A2").unwrap();
    store
        .update_note(
            user,
            note,
            &save(NoteType::Basic, "Q2", "A2", &gen, &[], Some(1)),
        )
        .unwrap();
    // Growing to reversed needs one more slot than the cap allows.
    let gen = generate_cards(NoteType::BasicReversed, "Q2", "A2").unwrap();
    let err = store
        .update_note(
            user,
            note,
            &save(NoteType::BasicReversed, "Q2", "A2", &gen, &[], Some(1)),
        )
        .unwrap_err();
    assert!(matches!(
        err,
        StoreError::CapExceeded { current: 1, cap: 1 }
    ));
    assert_eq!(live_cards(&store, user, deck).len(), 1);
}

#[test]
fn plain_update_detaches_card_and_reaps_empty_note() {
    let (store, user) = store_with_user();
    let deck = store.create_deck(user, "D", "", NOW).unwrap();
    let gen = generate_cards(NoteType::BasicReversed, "<b>Q</b>", "A").unwrap();
    let (note, ids) = store
        .create_note(
            user,
            deck,
            &save(NoteType::BasicReversed, "<b>Q</b>", "A", &gen, &[], None),
        )
        .unwrap();
    store
        .update_card(
            user,
            ids[0],
            &validate_card_text("plain", "edit").unwrap(),
            NOW,
        )
        .unwrap();
    let card = store.get_card(user, ids[0]).unwrap().unwrap();
    assert_eq!(card.note_id, None);
    assert!(card.front_html.is_none());
    let row = store.get_note(user, note).unwrap().unwrap();
    assert_eq!(row.cards, vec![(1, ids[1])], "sibling stays on the note");

    store.delete_card(user, ids[1], NOW).unwrap();
    assert!(
        store.get_note(user, note).unwrap().is_none(),
        "note with no live cards is gone"
    );
    // A further note update is a NotFound, not a resurrection.
    let gen = generate_cards(NoteType::Basic, "x", "y").unwrap();
    assert!(matches!(
        store.update_note(
            user,
            note,
            &save(NoteType::Basic, "x", "y", &gen, &[], None)
        ),
        Err(StoreError::NotFound("note"))
    ));
}

#[test]
fn adopting_a_plain_card_then_editing_it() {
    let (store, user) = store_with_user();
    let deck = store.create_deck(user, "D", "", NOW).unwrap();
    let ids = store
        .create_cards(
            user,
            deck,
            &[(validate_card_text("old", "card").unwrap(), vec!["t".into()])],
            None,
            NOW,
        )
        .unwrap();
    let note = store
        .adopt_card_into_note(
            user,
            ids[0],
            NoteType::Basic,
            sanitized("old"),
            sanitized("card"),
            NOW,
        )
        .unwrap();
    assert_eq!(
        store.get_card(user, ids[0]).unwrap().unwrap().note_id,
        Some(note)
    );
    // Idempotent.
    assert_eq!(
        store
            .adopt_card_into_note(
                user,
                ids[0],
                NoteType::Basic,
                sanitized("x"),
                sanitized("y"),
                NOW
            )
            .unwrap(),
        note
    );
    let gen = generate_cards(NoteType::Basic, "<u>new</u>", "card").unwrap();
    let out = store
        .update_note(
            user,
            note,
            &save(
                NoteType::Basic,
                "<u>new</u>",
                "card",
                &gen,
                &tags(&["t"]),
                None,
            ),
        )
        .unwrap();
    assert_eq!(out, ids, "same row, now rich");
    let card = store.get_card(user, ids[0]).unwrap().unwrap();
    assert_eq!(card.front_html.as_deref(), Some("<u>new</u>"));
    assert_eq!(live_cards(&store, user, deck).len(), 1);
}

#[test]
fn adopting_an_imported_cloze_card_brings_its_siblings() {
    let (store, user) = store_with_user();
    let deck = store.create_deck(user, "D", "", NOW).unwrap();
    // Simulate the importer: three rows sharing one cloze source, no note.
    let source = "{{c1::A}} {{c2::B}} {{c3::C}}\u{1f}";
    let ids = store
        .create_cards(
            user,
            deck,
            &[
                (validate_card_text("[...] B C", "A").unwrap(), vec![]),
                (validate_card_text("A [...] C", "B").unwrap(), vec![]),
                (validate_card_text("A B [...]", "C").unwrap(), vec![]),
            ],
            None,
            NOW,
        )
        .unwrap();
    let extras: Vec<_> = ids
        .iter()
        .enumerate()
        .map(|(i, id)| {
            (
                *id,
                CardExtras {
                    cloze_text: Some(source.to_string()),
                    cloze_index: Some(i as u32 + 1),
                    ..CardExtras::default()
                },
            )
        })
        .collect();
    store.set_card_extras(user, &extras).unwrap();

    // Edit the middle sibling: all three join the note at their slots.
    let note = store
        .adopt_card_into_note(
            user,
            ids[1],
            NoteType::Cloze,
            sanitized("{{c1::A}} {{c2::B}} {{c3::C}}"),
            sanitized(""),
            NOW,
        )
        .unwrap();
    let row = store.get_note(user, note).unwrap().unwrap();
    assert_eq!(row.cards, vec![(0, ids[0]), (1, ids[1]), (2, ids[2])]);

    // Regenerating with c2 removed touches exactly the right rows.
    let text = "{{c1::A}} B {{c3::C}}";
    let gen = generate_cards(NoteType::Cloze, text, "").unwrap();
    let out = store
        .update_note(
            user,
            note,
            &save(NoteType::Cloze, text, "", &gen, &[], None),
        )
        .unwrap();
    assert_eq!(out, vec![ids[0], ids[2]]);
    assert!(store.get_card(user, ids[1]).unwrap().is_none());
    assert_eq!(
        live_cards(&store, user, deck).len(),
        2,
        "no duplicate siblings"
    );
}

#[test]
fn note_ids_are_ownership_scoped() {
    let (store, user) = store_with_user();
    let other = store.create_user("O", None, "member", NOW).unwrap();
    let deck = store.create_deck(user, "D", "", NOW).unwrap();
    let gen = generate_cards(NoteType::Basic, "Q", "A").unwrap();
    let (note, ids) = store
        .create_note(
            user,
            deck,
            &save(NoteType::Basic, "Q", "A", &gen, &[], None),
        )
        .unwrap();
    assert!(store.get_note(other, note).unwrap().is_none());
    assert!(store.note_for_card(other, ids[0]).unwrap().is_none());
    assert!(matches!(
        store.update_note(
            other,
            note,
            &save(NoteType::Basic, "h", "j", &gen, &[], None)
        ),
        Err(StoreError::NotFound("note"))
    ));
    assert!(matches!(
        store.create_note(
            other,
            deck,
            &save(NoteType::Basic, "h", "j", &gen, &[], None)
        ),
        Err(StoreError::NotFound("deck"))
    ));
    assert!(store
        .adopt_card_into_note(
            other,
            ids[0],
            NoteType::Basic,
            sanitized(""),
            sanitized(""),
            NOW
        )
        .is_err());
    assert_eq!(store.get_note(user, NoteId(999)).unwrap(), None);
}

#[test]
fn deleting_a_deck_removes_its_notes() {
    let (store, user) = store_with_user();
    let deck = store.create_deck(user, "D", "", NOW).unwrap();
    let gen = generate_cards(NoteType::Cloze, "{{c1::x}} y", "").unwrap();
    let (note, _) = store
        .create_note(
            user,
            deck,
            &save(NoteType::Cloze, "{{c1::x}} y", "", &gen, &[], None),
        )
        .unwrap();
    store.delete_deck(user, deck).unwrap();
    assert!(store.get_note(user, note).unwrap().is_none());
    // The name is free again and a new deck starts clean.
    let deck2 = store.create_deck(user, "D", "", NOW).unwrap();
    assert!(live_cards(&store, user, deck2).is_empty());
}

#[test]
fn pagination_counts_and_offsets() {
    let (store, user) = store_with_user();
    let deck = store.create_deck(user, "D", "", NOW).unwrap();
    let batch: Vec<_> = (0..30)
        .map(|i| {
            (
                validate_card_text(
                    &format!("front {i:02}"),
                    if i % 3 == 0 { "fizz" } else { "b" },
                )
                .unwrap(),
                vec![],
            )
        })
        .collect();
    store.create_cards(user, deck, &batch, None, NOW).unwrap();
    assert_eq!(store.count_cards(user, Some(deck), None).unwrap(), 30);
    assert_eq!(
        store.count_cards(user, Some(deck), Some("fizz")).unwrap(),
        10
    );
    assert_eq!(store.count_cards(user, None, Some("front 2")).unwrap(), 10);

    let page1 = store
        .list_cards_page(user, Some(deck), None, 25, 0)
        .unwrap();
    let page2 = store
        .list_cards_page(user, Some(deck), None, 25, 25)
        .unwrap();
    assert_eq!((page1.len(), page2.len()), (25, 5));
    assert_eq!(page1[0].front, "front 29", "newest first");
    assert_eq!(page2[4].front, "front 00");
    assert!(page1.iter().all(|c| !page2.iter().any(|d| d.id == c.id)));
    assert!(store
        .list_cards_page(user, Some(deck), None, 25, 50)
        .unwrap()
        .is_empty());

    let fizz = store
        .list_cards_page(user, Some(deck), Some("fizz"), 4, 4)
        .unwrap();
    assert_eq!(fizz.len(), 4);
    assert!(fizz.iter().all(|c| c.back == "fizz"));
    // LIKE wildcards in the query are literal.
    assert_eq!(store.count_cards(user, Some(deck), Some("%")).unwrap(), 0);
    assert_eq!(
        store
            .count_cards(user, Some(deck), Some("front _1"))
            .unwrap(),
        0
    );
}
