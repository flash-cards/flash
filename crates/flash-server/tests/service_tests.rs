//! End-to-end service-layer tests over an in-memory store: the exact code
//! path the web UI and MCP tools share.

use std::sync::Arc;

use flash_core::queue::StudyScope;
use flash_core::Rating;
use flash_store::Store;

use flash_server::service::{ReviewOrigin, Services};
use flash_server::testing::heavy_permit;

const NOW: i64 = 1_700_000_000_000;

/// Services the way the open server builds them: no card cap. The hosted
/// plans' policy is exercised by the downstream extension that defines it.
fn services(store: Arc<Store>) -> Services {
    Services::new(store)
}

fn setup() -> (Services, flash_core::UserId) {
    let store = Store::open_in_memory().unwrap();
    let user = store.create_user("Test", None, "member", NOW).unwrap();
    (services(Arc::new(store)), user)
}

#[test]
fn full_study_session_flow() {
    let (services, user) = setup();
    services
        .create_cards(
            user,
            "Pharm",
            &[
                ("Warfarin antidote?".into(), "Vitamin K".into(), vec![]),
                (
                    "Heparin antidote?".into(),
                    "Protamine sulfate".into(),
                    vec![],
                ),
            ],
            NOW,
        )
        .unwrap();

    let session = services.start_session(user, StudyScope::All, NOW).unwrap();
    assert_eq!(session.cards_due, 2);
    let first = session.first_card.clone().unwrap();
    assert_eq!(first.front, "Warfarin antidote?");
    assert!(
        session.next_card_id.is_some(),
        "second card is announced for warm-up"
    );
    assert_ne!(session.next_card_id, Some(first.card_id));
    assert_eq!(services.reveal(user, first.card_id).unwrap(), "Vitamin K");

    // Good on card 1 -> card 2 comes next.
    let result = services
        .submit_review(
            user,
            session.session_id,
            first.card_id,
            Rating::Good,
            ReviewOrigin::new("mcp", Some("Claude")),
            NOW,
        )
        .unwrap();
    let second = result.next_card.unwrap();
    assert_eq!(second.front, "Heparin antidote?");
    assert_eq!(
        result.next_card_id, None,
        "nothing queued after the last card"
    );

    // Again on card 2 -> it's in a 10-minute learning step; nothing else is
    // due right now, and the queue hands it back (it's all that's left).
    let result = services
        .submit_review(
            user,
            session.session_id,
            second.card_id,
            Rating::Again,
            ReviewOrigin::new("mcp", Some("Claude")),
            NOW,
        )
        .unwrap();
    assert_eq!(result.next_card.map(|c| c.card_id), Some(second.card_id));

    // Ten minutes later the learning step has expired; review it Good.
    let later = NOW + 10 * 60_000;
    let result = services
        .submit_review(
            user,
            session.session_id,
            second.card_id,
            Rating::Good,
            ReviewOrigin::new("mcp", Some("Claude")),
            later,
        )
        .unwrap();
    assert!(result.next_card.is_none(), "queue is empty");
    assert_eq!(result.remaining, 0);

    let stats = services
        .end_session(user, session.session_id, later)
        .unwrap();
    assert_eq!((stats.reviewed, stats.good, stats.again), (3, 2, 1));
}

#[test]
fn new_card_budget_limits_queue() {
    let (services, user) = setup();
    let cards: Vec<(String, String, Vec<String>)> = (0..30)
        .map(|i| (format!("front {i}"), format!("back {i}"), vec![]))
        .collect();
    services.create_cards(user, "Big", &cards, NOW).unwrap();

    // Default budget is 20 new/day.
    let session = services.start_session(user, StudyScope::All, NOW).unwrap();
    assert_eq!(session.cards_due, 20);
}

#[test]
fn per_deck_limits_and_boost() {
    let (services, user) = setup();
    let cards: Vec<(String, String, Vec<String>)> = (0..10)
        .map(|i| (format!("q{i}"), "a".into(), vec![]))
        .collect();
    services.create_cards(user, "A", &cards, NOW).unwrap();
    services.create_cards(user, "B", &cards, NOW).unwrap();
    let decks = services.list_decks(user, NOW).unwrap();
    let a = decks.iter().find(|d| d.name == "A").unwrap().id;
    let b = decks.iter().find(|d| d.name == "B").unwrap().id;
    let new_for = |scope: StudyScope, at: i64| {
        services
            .queue_counts(user, &scope, at)
            .unwrap()
            .new_available
    };

    // Deck A overrides to 3 new/day; B inherits the account default (20).
    services.set_deck_limits(user, a, Some(3), None).unwrap();
    assert_eq!(new_for(StudyScope::Deck(a), NOW), 3);
    assert_eq!(new_for(StudyScope::Deck(b), NOW), 10);
    // All: A contributes 3, B 10 = 13, under the account total of 20.
    assert_eq!(new_for(StudyScope::All, NOW), 13);
    assert_eq!(
        services
            .start_session(user, StudyScope::All, NOW)
            .unwrap()
            .cards_due,
        13
    );
    // Deck summaries show the budgeted count, not the raw 10.
    let decks = services.list_decks(user, NOW).unwrap();
    assert_eq!(decks.iter().find(|d| d.id == a).unwrap().new_count, 3);

    // Studying B consumes B's and the account's allowance, never A's.
    let session = services
        .start_session(user, StudyScope::Deck(b), NOW)
        .unwrap();
    let first = session.first_card.unwrap();
    services
        .submit_review(
            user,
            session.session_id,
            first.card_id,
            Rating::Good,
            ReviewOrigin::new("mcp", Some("Claude")),
            NOW,
        )
        .unwrap();
    assert_eq!(new_for(StudyScope::Deck(a), NOW + 1), 3);
    assert_eq!(new_for(StudyScope::Deck(b), NOW + 1), 9);
    assert_eq!(new_for(StudyScope::All, NOW + 1), 12);

    // Boost A by 2 for today: 5 available now, back to 3 tomorrow.
    assert_eq!(services.boost_new_today(user, Some(a), 2, NOW).unwrap(), 2);
    assert_eq!(services.boost_today(user, Some(a), NOW).unwrap(), 2);
    assert_eq!(new_for(StudyScope::Deck(a), NOW + 2), 5);
    let tomorrow = NOW + 36 * 3_600_000;
    assert_eq!(new_for(StudyScope::Deck(a), tomorrow), 3);
    assert_eq!(services.boost_today(user, Some(a), tomorrow).unwrap(), 0);

    // Account default lowered to 4: B (inheriting) follows, A keeps its 3.
    services.set_daily_limits(user, 4, 200).unwrap();
    assert_eq!(new_for(StudyScope::Deck(b), tomorrow), 4);
    assert_eq!(new_for(StudyScope::Deck(a), tomorrow), 3);
    // An account-wide boost raises the inherited default for B only.
    services.boost_new_today(user, None, 3, tomorrow).unwrap();
    assert_eq!(new_for(StudyScope::Deck(b), tomorrow), 7);
    assert_eq!(new_for(StudyScope::Deck(a), tomorrow), 3);
    // Clearing A's override returns it to the default.
    services.set_deck_limits(user, a, None, None).unwrap();
    assert_eq!(new_for(StudyScope::Deck(a), tomorrow), 7);

    assert!(services.set_daily_limits(user, 10_000, 200).is_err());
    assert!(services.boost_new_today(user, None, 0, NOW).is_err());
}

#[test]
fn rename_deck_trims_and_rejects_duplicates() {
    let (services, user) = setup();
    let a = services.create_deck(user, "A", "", NOW).unwrap();
    let b = services.create_deck(user, "B", "", NOW).unwrap();
    services.rename_deck(user, a, "  Alpha ").unwrap();
    let names: Vec<String> = services
        .list_decks(user, NOW)
        .unwrap()
        .into_iter()
        .map(|d| d.name)
        .collect();
    assert!(names.contains(&"Alpha".to_string()));
    assert!(!names.contains(&"A".to_string()));
    // Renaming to your own current name is a no-op, not a conflict.
    services.rename_deck(user, a, "Alpha").unwrap();
    assert!(matches!(
        services.rename_deck(user, b, "Alpha"),
        Err(ServiceError::Store(flash_store::StoreError::Invalid(_)))
    ));
    assert!(matches!(
        services.rename_deck(user, b, "   "),
        Err(ServiceError::Store(flash_store::StoreError::Invalid(_)))
    ));
    assert!(matches!(
        services.rename_deck(user, flash_core::DeckId(999), "Z"),
        Err(ServiceError::Store(flash_store::StoreError::NotFound(_)))
    ));
}

#[test]
fn review_cap_limits_due_reviews_not_learning() {
    let (services, store, user) = setup_with_store();
    seed_cards(&store, user, 5);
    store
        .raw_execute_for_tests("UPDATE card_state SET phase = 2, due = 1")
        .unwrap();
    assert_eq!(
        services
            .queue_counts(user, &StudyScope::All, NOW)
            .unwrap()
            .due,
        5
    );
    services.set_daily_limits(user, 20, 2).unwrap();
    assert_eq!(
        services
            .queue_counts(user, &StudyScope::All, NOW)
            .unwrap()
            .due,
        2
    );
    assert_eq!(
        services
            .start_session(user, StudyScope::All, NOW)
            .unwrap()
            .cards_due,
        2
    );
    // Learning cards are never capped.
    store
        .raw_execute_for_tests(
            "UPDATE card_state SET phase = 1 WHERE card_id IN (SELECT id FROM cards ORDER BY id LIMIT 3)",
        )
        .unwrap();
    assert_eq!(
        services
            .queue_counts(user, &StudyScope::All, NOW)
            .unwrap()
            .due,
        5
    );
}

#[test]
fn deck_scoping_and_deck_autocreation() {
    let (services, user) = setup();
    services
        .create_cards(user, "Pharm", &[("a".into(), "b".into(), vec![])], NOW)
        .unwrap();
    services
        .create_cards(user, "Anatomy", &[("c".into(), "d".into(), vec![])], NOW)
        .unwrap();

    let decks = services.list_decks(user, NOW).unwrap();
    assert_eq!(decks.len(), 2, "decks auto-created by name");

    let pharm = decks.iter().find(|d| d.name == "Pharm").unwrap();
    let session = services
        .start_session(user, StudyScope::Deck(pharm.id), NOW)
        .unwrap();
    assert_eq!(session.cards_due, 1);
    assert_eq!(session.first_card.unwrap().front, "a");
}

#[test]
fn duplicate_submission_within_retry_window_is_idempotent() {
    let (services, user) = setup();
    services
        .create_cards(user, "D", &[("f".into(), "b".into(), vec![])], NOW)
        .unwrap();
    let session = services.start_session(user, StudyScope::All, NOW).unwrap();
    let card = session.first_card.unwrap().card_id;

    services
        .submit_review(
            user,
            session.session_id,
            card,
            Rating::Good,
            ReviewOrigin::new("mcp", Some("Claude")),
            NOW,
        )
        .unwrap();
    // A transport retry lands 2 seconds later: must not double-record.
    services
        .submit_review(
            user,
            session.session_id,
            card,
            Rating::Good,
            ReviewOrigin::new("mcp", Some("Claude")),
            NOW + 2_000,
        )
        .unwrap();
    let stats = services
        .store()
        .session_stats(user, session.session_id)
        .unwrap();
    assert_eq!(stats.reviewed, 1, "retry must not create a second review");

    // A genuine re-review outside the window still records.
    let later = NOW + 11 * 60_000;
    services
        .submit_review(
            user,
            session.session_id,
            card,
            Rating::Good,
            ReviewOrigin::new("mcp", Some("Claude")),
            later,
        )
        .unwrap();
    assert_eq!(
        services
            .store()
            .session_stats(user, session.session_id)
            .unwrap()
            .reviewed,
        2
    );
}

#[test]
fn empty_and_invalid_input_handling() {
    let (services, user) = setup();
    // Empty queue -> clean empty session, not an error.
    let session = services.start_session(user, StudyScope::All, NOW).unwrap();
    assert_eq!(session.cards_due, 0);
    assert!(session.first_card.is_none());

    // Invalid card text is rejected before anything is written — including
    // the would-be auto-created deck.
    let err = services
        .create_cards(user, "D", &[("  ".into(), "b".into(), vec![])], NOW)
        .unwrap_err();
    assert!(err.to_string().contains("front is empty"));
    assert!(services.list_decks(user, NOW).unwrap().is_empty());
}

const DAY: i64 = 24 * 60 * 60 * 1000;

/// Setup that also hands back the store for direct review seeding.
fn setup_with_store() -> (Services, Arc<Store>, flash_core::UserId) {
    let store = Arc::new(Store::open_in_memory().unwrap());
    let user = store.create_user("Test", None, "member", NOW).unwrap();
    (services(store.clone()), store, user)
}

fn seed_review(store: &Store, user: flash_core::UserId, card: flash_core::CardId, ts: i64) {
    let scheduler = flash_core::Scheduler::new(None, 0.9).unwrap();
    let state = store.get_card_state(user, card).unwrap();
    let outcome = scheduler.review(&state, Rating::Good, ts).unwrap();
    store
        .record_review(user, card, &outcome, ts, "web", None, None)
        .unwrap();
}

#[test]
fn stats_day_cutoff_merges_late_night_reviews() {
    let (services, store, user) = setup_with_store();
    let ids = store
        .create_cards(
            user,
            store.create_deck(user, "D", "", NOW).unwrap(),
            &[(flash_core::validate_card_text("f", "b").unwrap(), vec![])],
            None,
            NOW,
        )
        .unwrap();
    // The cutoff is evaluated in the user's own zone, so pin one: in
    // America/New_York NOW is ~17:13, and nine hours later is ~2:13am the
    // next civil day, still before the 4am cutoff — both land on the same
    // study day.
    store
        .set_study_settings(user, Some("America/New_York"), None, None)
        .unwrap();
    seed_review(&store, user, ids[0], NOW);
    seed_review(&store, user, ids[0], NOW + 9 * 60 * 60 * 1000);

    let stats = services
        .stats(&heavy_permit(user), NOW + 9 * 60 * 60 * 1000)
        .unwrap();
    assert_eq!(stats.streak, 1, "2am review must not start a second day");
    assert_eq!(stats.days_learned_pct, 100);
}

#[test]
fn stats_streaks_and_heatmap_shape() {
    let (services, store, user) = setup_with_store();
    let deck = store.create_deck(user, "D", "", NOW).unwrap();
    let ids = store
        .create_cards(
            user,
            deck,
            &[(flash_core::validate_card_text("f", "b").unwrap(), vec![])],
            None,
            NOW,
        )
        .unwrap();
    // Current run: today, -1d, -2d. Older run: -5d..-8d (longest = 4).
    for offset in [0i64, 1, 2, 5, 6, 7, 8] {
        seed_review(&store, user, ids[0], NOW - offset * DAY);
    }

    let stats = services.stats(&heavy_permit(user), NOW).unwrap();
    assert_eq!(stats.streak, 3);
    assert_eq!(stats.longest_streak, 4);
    assert_eq!(stats.total_reviews, 7);
    // Denominator: 9 days since the first review.
    assert_eq!(stats.days_learned_pct, 7 * 100 / 9);

    assert_eq!(stats.heat_weeks.len(), 53);
    assert!(stats.heat_weeks.iter().all(|w| w.len() == 7));
    let cells: Vec<_> = stats.heat_weeks.iter().flatten().collect();
    let today_pos = cells.iter().position(|c| c.today).expect("today cell");
    assert_eq!(cells.iter().filter(|c| c.today).count(), 1);
    assert!(cells[..today_pos].iter().all(|c| !c.future));
    assert!(cells[today_pos + 1..].iter().all(|c| c.future));
    // Seven studied days show up as leveled past cells.
    assert_eq!(cells.iter().filter(|c| !c.future && c.level > 0).count(), 7);
    let month_cols: u32 = stats.heat_months.iter().map(|m| m.weeks).sum();
    assert_eq!(month_cols, 53);
}

#[test]
fn stats_future_due_lands_in_heatmap() {
    let (services, store, user) = setup_with_store();
    let deck = store.create_deck(user, "D", "", NOW).unwrap();
    let ids = store
        .create_cards(
            user,
            deck,
            &[(flash_core::validate_card_text("f", "b").unwrap(), vec![])],
            None,
            NOW,
        )
        .unwrap();
    // One Good review schedules the card a few days out.
    seed_review(&store, user, ids[0], NOW);

    let stats = services.stats(&heavy_permit(user), NOW).unwrap();
    let future_due: u32 = stats
        .heat_weeks
        .iter()
        .flatten()
        .filter(|c| c.future)
        .map(|c| c.count)
        .sum();
    assert_eq!(future_due, 1, "the scheduled card appears as future load");
    assert_eq!(stats.active_cards, 1);
    assert_eq!(stats.mature_cards, 0, "stability is far below 21 days");
}

use flash_server::service::ServiceError;
use flash_store::import::ImportRow;

/// Seeds `n` cards directly at the store level into one deck.
fn seed_cards(store: &Store, user: flash_core::UserId, n: usize) -> Vec<flash_core::CardId> {
    let deck = store.create_deck(user, "Seed", "", NOW).unwrap();
    let cards: Vec<_> = (0..n)
        .map(|i| {
            (
                flash_core::validate_card_text(&format!("f{i}"), "b").unwrap(),
                Vec::new(),
            )
        })
        .collect();
    store.create_cards(user, deck, &cards, None, NOW).unwrap()
}

// ---- progress import (revlog replay) ----

#[test]
fn progress_import_replays_history_into_card_state() {
    let (services, store, user) = setup_with_store();
    let day = 24 * 60 * 60 * 1000;
    let rows_with_history = vec![
        ImportRow {
            front: "seasoned".into(),
            back: "b".into(),
            tags: vec![],
            deck: None,
            media: vec![],
            front_html: None,
            back_html: None,
            suspended: false,
            cloze_text: None,
            cloze_index: None,
            type_answer: None,
            reviews: vec![
                (NOW - 30 * day, 3),
                (NOW - 20 * day, 3),
                (NOW - 10 * day, 2),
            ],
        },
        ImportRow {
            front: "fresh".into(),
            back: "b".into(),
            tags: vec![],
            deck: None,
            media: vec![],
            front_html: None,
            back_html: None,
            suspended: false,
            cloze_text: None,
            cloze_index: None,
            type_answer: None,
            reviews: vec![],
        },
    ];

    services
        .import_cards(user, "Anki", rows_with_history.clone(), true, None, NOW)
        .unwrap();
    let cards = store.list_cards(user, None, Some("seasoned"), 10).unwrap();
    let state = store.get_card_state(user, cards[0].id).unwrap();
    assert_eq!(state.phase, flash_core::Phase::Review);
    assert_eq!(state.reps, 3);
    assert!(state.stability.is_some());
    assert_eq!(state.last_review_ms, Some(NOW - 10 * day));
    assert!(state.due_ms > NOW - 10 * day, "due after last review");

    // The no-history card in the same batch stays new.
    let fresh = store.list_cards(user, None, Some("fresh"), 10).unwrap();
    let fresh_state = store.get_card_state(user, fresh[0].id).unwrap();
    assert_eq!(fresh_state.phase, flash_core::Phase::New);

    // Progress-imported cards count as in-rotation (not New) — they feed
    // the post-grace clamp and skip the new-card budget.
    assert_eq!(store.cards_in_rotation(user).unwrap(), 1);
}

#[test]
fn progress_import_opt_out_leaves_cards_new() {
    let (services, store, user) = setup_with_store();
    let rows = vec![ImportRow {
        front: "seasoned".into(),
        back: "b".into(),
        tags: vec![],
        deck: None,
        media: vec![],
        front_html: None,
        back_html: None,
        suspended: false,
        cloze_text: None,
        cloze_index: None,
        type_answer: None,
        reviews: vec![(NOW - 1000, 3), (NOW - 500, 3)],
    }];
    services
        .import_cards(user, "Anki", rows, false, None, NOW)
        .unwrap();
    let cards = store.list_cards(user, None, None, 10).unwrap();
    let state = store.get_card_state(user, cards[0].id).unwrap();
    assert_eq!(state.phase, flash_core::Phase::New);
    assert_eq!(state.reps, 0);
}

#[test]
fn progress_import_survives_junk_history() {
    let (services, store, user) = setup_with_store();
    let rows = vec![ImportRow {
        front: "messy".into(),
        back: "b".into(),
        tags: vec![],
        deck: None,
        media: vec![],
        front_html: None,
        back_html: None,
        suspended: false,
        cloze_text: None,
        cloze_index: None,
        type_answer: None,
        // Out-of-order timestamp and invalid rating get skipped; the two
        // valid chronological reviews still replay.
        reviews: vec![
            (NOW - 3000, 3),
            (NOW - 5000, 3),
            (NOW - 1000, 7),
            (NOW - 500, 2),
        ],
    }];
    services
        .import_cards(user, "Anki", rows, true, None, NOW)
        .unwrap();
    let cards = store.list_cards(user, None, None, 10).unwrap();
    let state = store.get_card_state(user, cards[0].id).unwrap();
    assert_eq!(state.reps, 2);
    assert_eq!(state.last_review_ms, Some(NOW - 500));
}

// ---- full-fidelity import: suspension, rich extras, settings ----

fn import_row(front: &str, back: &str) -> ImportRow {
    ImportRow {
        reviews: vec![],
        media: vec![],
        front: front.into(),
        back: back.into(),
        front_html: None,
        back_html: None,
        tags: vec![],
        deck: None,
        suspended: false,
        cloze_text: None,
        cloze_index: None,
        type_answer: None,
    }
}

#[test]
fn suspended_import_stays_out_of_the_study_queue() {
    let (services, store, user) = setup_with_store();
    let mut paused = import_row("paused", "b");
    paused.suspended = true;
    paused.reviews = vec![(NOW - 1000, 3)];
    services
        .import_cards(
            user,
            "Anki",
            vec![import_row("active", "b"), paused],
            true,
            None,
            NOW,
        )
        .unwrap();

    let row = store
        .list_cards(user, None, Some("paused"), 10)
        .unwrap()
        .remove(0);
    assert!(row.suspended, "arrives suspended, as in Anki");
    // Progress replay and suspension coexist: state exists, queue skips it.
    let state = store.get_card_state(user, row.id).unwrap();
    assert_eq!(state.reps, 1);
    let session = services.start_session(user, StudyScope::All, NOW).unwrap();
    assert_eq!(session.cards_due, 1, "only the active card is served");
}

#[test]
fn mcp_edit_makes_card_plain_and_detached() {
    let (services, store, user) = setup_with_store();
    let mut row = import_row("question", "answer");
    row.front_html = Some(flash_store::richtext::sanitize_with_media(
        "<b>question</b>",
    ));
    row.cloze_text = Some("{{c1::question}}\u{1f}".into());
    row.cloze_index = Some(1);
    row.type_answer = Some("answer".into());
    services
        .import_cards(user, "Anki", vec![row], false, None, NOW)
        .unwrap();

    let card = store.list_cards(user, None, None, 10).unwrap().remove(0);
    assert_eq!(card.front_html.as_deref(), Some("<b>question</b>"));
    assert_eq!(card.type_answer.as_deref(), Some("answer"));
    let exported = store.export_cards(user).unwrap();
    assert_eq!(exported[0].cloze.as_ref().unwrap().1, 1);

    // A plain-text (MCP) edit makes the card plain again — no stale rich view.
    store
        .update_card(
            user,
            card.id,
            &flash_core::validate_card_text("q2", "a2").unwrap(),
            NOW,
        )
        .unwrap();
    let card = store.get_card(user, card.id).unwrap().unwrap();
    assert!(card.front_html.is_none() && card.type_answer.is_none());
    assert!(store.export_cards(user).unwrap()[0].cloze.is_none());
}

#[test]
fn imported_settings_apply_and_junk_params_are_dropped() {
    let (services, store, user) = setup_with_store();
    let good = flash_store::import::ImportedSettings {
        desired_retention: Some(0.85),
        fsrs_params: None,
        new_per_day: Some(15),
    };
    services.adopt_imported_settings(user, &good).unwrap();
    let settings = store.get_settings(user).unwrap();
    assert!((settings.desired_retention - 0.85).abs() < 1e-6);
    assert_eq!(settings.new_per_day, 15);

    // Params the scheduler rejects never reach the database.
    let bad = flash_store::import::ImportedSettings {
        desired_retention: None,
        fsrs_params: Some(vec![1.0, 2.0]),
        new_per_day: None,
    };
    services.adopt_imported_settings(user, &bad).unwrap();
    assert!(store.get_settings(user).unwrap().fsrs_params.is_none());
}

// ---- notes: the advanced editor ----

mod notes {
    use super::*;
    use flash_core::{CardId, MediaId};
    use flash_server::service::{EditorSeed, NoteInput, ServiceError};
    use flash_store::media::MediaKind;
    use flash_store::notes::NoteType;

    fn input(kind: NoteType, front: &str, back: &str, tags: &[&str]) -> NoteInput {
        NoteInput {
            note_type: kind,
            front_html: front.into(),
            back_html: back.into(),
            tags: tags.iter().map(|t| t.to_string()).collect(),
        }
    }

    fn deck(store: &Store, user: flash_core::UserId) -> flash_core::DeckId {
        store.create_deck(user, "Notes", "", NOW).unwrap()
    }

    #[test]
    fn create_note_per_type_yields_the_right_cards() {
        let (services, store, user) = setup_with_store();
        let deck = deck(&store, user);
        let cases = [
            (NoteType::Basic, "Q", "A", 1),
            (NoteType::BasicReversed, "Q", "A", 2),
            (NoteType::BasicTyped, "Q", "A", 1),
            (NoteType::Cloze, "{{c1::a}} {{c2::b}} {{c3::c}}", "", 3),
        ];
        for (kind, f, b, n) in cases {
            let (_, ids) = services
                .create_note(user, deck, &input(kind, f, b, &["T"]), NOW)
                .unwrap();
            assert_eq!(ids.len(), n, "{kind:?}");
            for id in ids {
                let card = store.get_card(user, id).unwrap().unwrap();
                assert_eq!(card.tags, vec!["t"]);
            }
        }
        assert_eq!(store.count_cards(user, Some(deck), None).unwrap(), 7);
    }

    #[test]
    fn editor_html_is_sanitized_before_anything_is_stored() {
        let (services, store, user) = setup_with_store();
        let deck = deck(&store, user);
        let hostile = concat!(
            "<b onclick=\"x()\">bold</b><script>alert(1)</script>",
            "<span style=\"color:red\" class=\"hl-red evil\">red</span>",
            "<img src=\"https://evil.example/x.png\"><a href=\"javascript:1\">link</a>"
        );
        let (note, ids) = services
            .create_note(user, deck, &input(NoteType::Basic, hostile, "A", &[]), NOW)
            .unwrap();
        let card = store.get_card(user, ids[0]).unwrap().unwrap();
        let html = card.front_html.unwrap();
        assert!(
            !html.contains("script") && !html.contains("onclick"),
            "{html}"
        );
        assert!(!html.contains("style=") && !html.contains("evil"), "{html}");
        assert!(!html.contains("https://") && !html.contains("<a"), "{html}");
        assert!(
            html.contains("<b>bold</b>") && html.contains("class=\"hl-red\""),
            "{html}"
        );
        assert_eq!(card.front, "boldredlink", "plain text has no script body");
        let row = store.get_note(user, note).unwrap().unwrap();
        assert_eq!(row.front_html, html, "the note keeps the sanitized source");
    }

    #[test]
    fn media_must_belong_to_the_user() {
        let (services, store, user) = setup_with_store();
        let deck = deck(&store, user);
        let other = store.create_user("O", None, "member", NOW).unwrap();
        let theirs = store
            .create_media(
                other,
                &"d".repeat(64),
                "d.png",
                "image/png",
                MediaKind::Image,
                5,
                NOW,
            )
            .unwrap();
        let foreign = format!("<img src=\"/media/{}\">", theirs.0);
        let err = services
            .create_note(user, deck, &input(NoteType::Basic, &foreign, "A", &[]), NOW)
            .unwrap_err();
        assert!(
            matches!(err, ServiceError::Invalid(ref m) if m.contains("upload")),
            "{err}"
        );
        let err = services
            .create_note(
                user,
                deck,
                &input(NoteType::Basic, "<img src=\"/media/424242\">", "A", &[]),
                NOW,
            )
            .unwrap_err();
        assert!(matches!(err, ServiceError::Invalid(_)));
        assert!(store
            .list_cards(user, Some(deck), None, 10)
            .unwrap()
            .is_empty());

        let mine = store
            .create_media(
                user,
                &"e".repeat(64),
                "e.mp3",
                "audio/mpeg",
                MediaKind::Audio,
                5,
                NOW,
            )
            .unwrap();
        let audio = format!("<audio controls src=\"/media/{}\"></audio>", mine.0);
        let (_, ids) = services
            .create_note(user, deck, &input(NoteType::Basic, "Q", &audio, &[]), NOW)
            .unwrap();
        let card = store.get_card(user, ids[0]).unwrap().unwrap();
        assert_eq!(card.back, "[audio]");
        assert_eq!(store.media_for_export(user).unwrap()[0].id, MediaId(mine.0));
    }

    #[test]
    fn invalid_notes_are_refused_with_a_message() {
        let (services, store, user) = setup_with_store();
        let deck = deck(&store, user);
        for (kind, f, b) in [
            (NoteType::Basic, "", "A"),
            (NoteType::Basic, "Q", "<br>"),
            (NoteType::Cloze, "no cloze", ""),
            (NoteType::Cloze, "{{c1::}}", ""),
        ] {
            assert!(
                matches!(
                    services.create_note(user, deck, &input(kind, f, b, &[]), NOW),
                    Err(ServiceError::Invalid(_))
                ),
                "{kind:?} {f:?} {b:?}"
            );
        }
        let huge = "x".repeat(40_001);
        assert!(matches!(
            services.create_note(user, deck, &input(NoteType::Basic, &huge, "A", &[]), NOW),
            Err(ServiceError::Invalid(_))
        ));
    }

    #[test]
    fn editing_an_imported_cloze_card_reuses_its_siblings() {
        let (services, store, user) = setup_with_store();
        let mut rows = Vec::new();
        for (i, front) in ["[...] b c", "a [...] c", "a b [...]"].iter().enumerate() {
            let mut r = import_row(front, "x");
            r.cloze_text = Some("{{c1::a}} {{c2::b}} {{c3::c}}\u{1f}Extra".into());
            r.cloze_index = Some(i as u32 + 1);
            r.front_html = Some(flash_store::richtext::sanitize_with_media(&format!(
                "<span class=\"cloze-blank\">[...]</span> {i}"
            )));
            rows.push(r);
        }
        services
            .import_cards(user, "Anki", rows, false, None, NOW)
            .unwrap();
        let cards = store.list_cards(user, None, None, 10).unwrap();
        assert_eq!(cards.len(), 3);
        let middle = cards.iter().find(|c| c.cloze_index == Some(2)).unwrap();

        let seed = services.editor_seed(user, middle.id).unwrap();
        assert_eq!(
            seed,
            EditorSeed {
                card_id: middle.id,
                deck_id: middle.deck_id,
                note_id: None,
                note_type: NoteType::Cloze,
                front_html: "{{c1::a}} {{c2::b}} {{c3::c}}".into(),
                back_html: "Extra".into(),
                tags: vec![],
                sibling_count: 1,
            }
        );

        let (note, ids) = services
            .save_card_editor(
                user,
                middle.id,
                &input(
                    NoteType::Cloze,
                    "{{c1::a}} {{c2::B!}} {{c3::c}}",
                    "<b>More</b>",
                    &["z"],
                ),
                NOW,
            )
            .unwrap();
        assert_eq!(ids.len(), 3);
        assert_eq!(
            store.list_cards(user, None, None, 10).unwrap().len(),
            3,
            "no duplicates"
        );
        let seed = services.editor_seed(user, middle.id).unwrap();
        assert_eq!(seed.note_id, Some(note));
        assert_eq!(seed.sibling_count, 3);
        assert_eq!(seed.back_html, "<b>More</b>");
        assert_eq!(seed.tags, vec!["z"]);
        let c2 = store.get_card(user, middle.id).unwrap().unwrap();
        assert_eq!(c2.back, "B!\nMore");
        assert!(c2.back_html.unwrap().contains("<b>More</b>"));
    }

    #[test]
    fn editor_seed_for_plain_and_typed_cards() {
        let (services, store, user) = setup_with_store();
        let ids = services
            .create_cards(
                user,
                "Notes",
                &[("a <b> & c\nd".into(), "e".into(), vec!["t".into()])],
                NOW,
            )
            .unwrap();
        let seed = services.editor_seed(user, ids[0]).unwrap();
        assert_eq!(seed.note_type, NoteType::Basic);
        assert_eq!(
            seed.front_html, "a &lt;b&gt; &amp; c<br>d",
            "escaped, not parsed"
        );
        assert_eq!(seed.tags, vec!["t"]);

        let mut r = import_row("type me", "answer");
        r.type_answer = Some("answer".into());
        services
            .import_cards(user, "Anki", vec![r], false, None, NOW)
            .unwrap();
        let typed = store
            .list_cards(user, None, Some("type me"), 10)
            .unwrap()
            .remove(0);
        assert_eq!(
            services.editor_seed(user, typed.id).unwrap().note_type,
            NoteType::BasicTyped
        );

        assert!(matches!(
            services.editor_seed(user, CardId(9999)),
            Err(ServiceError::Store(flash_store::StoreError::NotFound(
                "card"
            )))
        ));
    }

    #[test]
    fn paged_listing_clamps_and_counts() {
        let (services, store, user) = setup_with_store();
        let deck = deck(&store, user);
        let batch: Vec<_> = (0..30)
            .map(|i| (format!("q{i}"), "a".into(), vec![]))
            .collect();
        services.create_cards(user, "Notes", &batch, NOW).unwrap();
        let (rows, total) = services.list_cards_page(user, deck, None, 1, 25).unwrap();
        assert_eq!((rows.len(), total), (25, 30));
        let (rows, _) = services.list_cards_page(user, deck, None, 2, 25).unwrap();
        assert_eq!(rows.len(), 5);
        let (rows, _) = services.list_cards_page(user, deck, None, 99, 25).unwrap();
        assert_eq!(rows.len(), 5, "past the end clamps to the last page");
        let (rows, _) = services.list_cards_page(user, deck, None, 0, 25).unwrap();
        assert_eq!(rows.len(), 25, "page 0 clamps to 1");
        let (rows, total) = services
            .list_cards_page(user, deck, Some("q2"), 1, 25)
            .unwrap();
        assert_eq!((rows.len(), total), (11, 11));
    }
}
