//! Integration tests over an in-memory database: full migration run plus
//! CRUD, review recording, immutability triggers, and queue scoping.

use flash_core::queue::StudyScope;
use flash_core::{validate_card_text, CardState, Phase, Rating, Scheduler};
use flash_store::{Store, StoreError};

const NOW: i64 = 1_700_000_000_000;
const DAY: i64 = 24 * 60 * 60 * 1000;

fn store_with_user() -> (Store, flash_core::UserId) {
    let store = Store::open_in_memory().expect("open + migrate");
    let user = store
        .create_user("Test User", Some("t@example.com"), "member", NOW)
        .unwrap();
    (store, user)
}

fn card(front: &str, back: &str, tags: &[&str]) -> (flash_core::CardText, Vec<String>) {
    (
        validate_card_text(front, back).unwrap(),
        tags.iter().map(|t| t.to_string()).collect(),
    )
}

#[test]
fn migrations_and_default_settings() {
    let (store, user) = store_with_user();
    let settings = store.get_settings(user).unwrap();
    assert_eq!(settings.grading_mode, flash_core::GradingMode::Silent);
    assert_eq!(settings.new_per_day, 20);
    assert_eq!(settings.reviews_per_day, 200);
    assert_eq!((settings.boost_new, settings.boost_day), (0, 0));
    assert!(settings.fsrs_params.is_none());
}

/// The per-account deck cap is the store's, so no caller can walk
/// around it: the third review found imports creating decks past the
/// cap by calling the store directly.
#[test]
fn the_deck_cap_is_enforced_where_the_row_is_created() {
    let (store, user) = store_with_user();
    for i in 0..flash_store::MAX_DECKS_PER_USER {
        store
            .create_deck(user, &format!("deck {i}"), "", NOW)
            .unwrap();
    }
    let err = store.create_deck(user, "one more", "", NOW).unwrap_err();
    assert!(
        matches!(&err, StoreError::Invalid(m) if m.contains("at most 500 decks")),
        "{err:?}"
    );
    assert_eq!(
        store.deck_count(user).unwrap(),
        flash_store::MAX_DECKS_PER_USER
    );
}

#[test]
fn daily_limits_round_trip() {
    let (store, user) = store_with_user();
    store.set_daily_limits(user, 35, 400).unwrap();
    store.set_account_boost(user, 10, 123).unwrap();
    let s = store.get_settings(user).unwrap();
    assert_eq!(
        (s.new_per_day, s.reviews_per_day, s.boost_new, s.boost_day),
        (35, 400, 10, 123)
    );

    let deck = store.create_deck(user, "Limited", "", NOW).unwrap();
    let limits = store.get_deck_limits(user, deck).unwrap();
    assert_eq!(
        limits,
        flash_store::DeckLimits::default(),
        "new decks inherit everything"
    );
    store.set_deck_limits(user, deck, Some(5), None).unwrap();
    store.set_deck_boost(user, deck, 3, 456).unwrap();
    let limits = store.get_deck_limits(user, deck).unwrap();
    assert_eq!(limits.new_per_day, Some(5));
    assert_eq!(limits.reviews_per_day, None);
    assert_eq!((limits.boost_new, limits.boost_day), (3, 456));
    assert_eq!(store.all_deck_limits(user).unwrap(), vec![(deck, limits)]);
    // Someone else's deck is not found, not silently updated.
    let other = store.create_user("O", None, "member", NOW).unwrap();
    assert!(store.set_deck_limits(other, deck, Some(1), None).is_err());
}

#[test]
fn deck_and_card_round_trip() {
    let (store, user) = store_with_user();
    let deck = store.create_deck(user, "Pharmacology", "", NOW).unwrap();
    assert_eq!(
        store.find_deck_by_name(user, "Pharmacology").unwrap(),
        Some(deck)
    );

    let ids = store
        .create_cards(
            user,
            deck,
            &[
                card(
                    "What is the antidote for warfarin?",
                    "Vitamin K",
                    &["exam-2"],
                ),
                card(
                    "Digoxin therapeutic range?",
                    "0.5-2.0 ng/mL",
                    &["cardiac", "exam-2"],
                ),
            ],
            None,
            NOW,
        )
        .unwrap();
    assert_eq!(ids.len(), 2);

    let row = store.get_card(user, ids[1]).unwrap().unwrap();
    assert_eq!(row.back, "0.5-2.0 ng/mL");
    assert_eq!(row.tags, vec!["cardiac", "exam-2"]);

    let state = store.get_card_state(user, ids[0]).unwrap();
    assert_eq!(state, CardState::new_card(NOW));

    let decks = store.list_decks(user, NOW).unwrap();
    assert_eq!(decks[0].new_count, 2);
    assert_eq!(decks[0].due_count, 0);

    let found = store
        .list_cards(user, Some(deck), Some("warfarin"), 10)
        .unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].id, ids[0]);

    // Tags must round-trip through list_cards, not just get_card.
    let all = store.list_cards(user, Some(deck), None, 10).unwrap();
    let digoxin = all.iter().find(|c| c.id == ids[1]).unwrap();
    assert_eq!(digoxin.tags, vec!["cardiac", "exam-2"]);
    assert_eq!(
        all.iter().find(|c| c.id == ids[0]).unwrap().tags,
        vec!["exam-2"]
    );
}

#[test]
fn review_recording_updates_state_and_log() {
    let (store, user) = store_with_user();
    let deck = store.create_deck(user, "D", "", NOW).unwrap();
    let ids = store
        .create_cards(user, deck, &[card("f", "b", &[])], None, NOW)
        .unwrap();
    let scheduler = Scheduler::new(None, 0.9).unwrap();

    let state = store.get_card_state(user, ids[0]).unwrap();
    let outcome = scheduler.review(&state, Rating::Good, NOW).unwrap();
    store
        .record_review(user, ids[0], &outcome, NOW, "web", None, None)
        .unwrap();

    let after = store.get_card_state(user, ids[0]).unwrap();
    assert_eq!(after.phase, Phase::Review);
    assert_eq!(after.reps, 1);
    assert!(after.due_ms > NOW);
    // f32 round-trips through a REAL column exactly.
    assert_eq!(after.stability, outcome.state.stability);

    assert_eq!(store.new_cards_introduced_since(user, NOW - 1).unwrap(), 1);
}

#[test]
fn review_log_is_immutable() {
    let (store, user) = store_with_user();
    let deck = store.create_deck(user, "D", "", NOW).unwrap();
    let ids = store
        .create_cards(user, deck, &[card("f", "b", &[])], None, NOW)
        .unwrap();
    let scheduler = Scheduler::new(None, 0.9).unwrap();
    let outcome = scheduler
        .review(
            &store.get_card_state(user, ids[0]).unwrap(),
            Rating::Good,
            NOW,
        )
        .unwrap();
    store
        .record_review(user, ids[0], &outcome, NOW, "web", None, None)
        .unwrap();
    // Every interface records its own provenance; the V012 rebuild kept
    // the CHECK and added the app.
    store
        .record_review(user, ids[0], &outcome, NOW + 1, "mobile", Some("ios"), None)
        .unwrap();
    assert!(store
        .record_review(user, ids[0], &outcome, NOW + 2, "bogus", None, None)
        .is_err());
    assert_eq!(store.review_totals(user).unwrap().0, 2);

    // Triggers must reject tampering even through raw SQL.
    let err = store.raw_execute_for_tests("UPDATE review_log SET rating = 4");
    assert!(err.unwrap_err().to_string().contains("immutable"));
    let err = store.raw_execute_for_tests("DELETE FROM review_log");
    assert!(err.unwrap_err().to_string().contains("immutable"));
}

#[test]
fn api_tokens_list_revoke_and_purge() {
    let (store, user) = store_with_user();
    store
        .create_oauth_client("mcp", "Claude", &[], "{}", NOW)
        .unwrap();
    for (i, (client, label)) in [
        ("flash-mobile", "iPhone"),
        ("flash-mobile", "iPad"),
        ("mcp", ""),
    ]
    .into_iter()
    .enumerate()
    {
        store
            .insert_oauth_token(
                &format!("a{i}"),
                &format!("r{i}"),
                client,
                user,
                "mobile",
                label,
                NOW + 3_600_000,
                NOW + 60 * DAY,
                NOW + i as i64,
            )
            .unwrap();
    }

    let info = store.lookup_api_token("a0", NOW + 1).unwrap().unwrap();
    assert_eq!(info.user, user);
    assert_eq!(
        (info.role.as_str(), info.client_id.as_str()),
        ("member", "flash-mobile")
    );
    assert!(store.lookup_api_token("nope", NOW).unwrap().is_none());

    // Newest first, one client only.
    let mine = store
        .list_tokens_for_user(user, "flash-mobile", NOW + 5)
        .unwrap();
    let labels: Vec<&str> = mine.iter().map(|t| t.label.as_str()).collect();
    assert_eq!(labels, ["iPad", "iPhone"]);
    assert!(mine.iter().all(|t| t.last_used_at.is_none()));

    // A refresh stamps last_used_at.
    store
        .rotate_refresh_token(
            "r1",
            "a1n",
            "r1n",
            NOW + 7_200_000,
            NOW + 61 * DAY,
            NOW + 20,
        )
        .unwrap()
        .unwrap();
    let mine = store
        .list_tokens_for_user(user, "flash-mobile", NOW + 25)
        .unwrap();
    assert_eq!(mine[0].last_used_at, Some(NOW + 20));

    // "Sign out other devices" keeps the caller's grant and never touches
    // MCP grants.
    let keep = mine[1].id;
    assert_eq!(
        store
            .revoke_tokens_for_user(user, "flash-mobile", Some(keep), NOW + 30)
            .unwrap(),
        1
    );
    assert!(store.lookup_api_token("a1n", NOW + 31).unwrap().is_none());
    assert!(store.lookup_api_token("a0", NOW + 31).unwrap().is_some());
    assert!(store.lookup_api_token("a2", NOW + 31).unwrap().is_some());

    // Signing out this device revokes exactly once.
    store.revoke_token(user, keep, NOW + 40).unwrap();
    assert!(matches!(
        store.revoke_token(user, keep, NOW + 41),
        Err(StoreError::NotFound(_))
    ));
    assert!(store
        .list_tokens_for_user(user, "flash-mobile", NOW + 42)
        .unwrap()
        .is_empty());

    // Revoked rows survive the grace window, then go; the live MCP grant
    // stays.
    assert_eq!(store.purge_expired_tokens(NOW + 50).unwrap(), 0);
    assert_eq!(store.purge_expired_tokens(NOW + 8 * DAY).unwrap(), 2);
    assert_eq!(
        store
            .list_tokens_for_user(user, "mcp", NOW + 8 * DAY)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn delete_user_account_purges_everything_and_only_theirs() {
    use flash_store::media::MediaKind;
    let (store, user) = store_with_user();
    let other = store
        .create_user("Other", Some("o@example.com"), "member", NOW)
        .unwrap();
    let scheduler = Scheduler::new(None, 0.9).unwrap();

    // The doomed account: deck, tagged cards, a review, media (one hash
    // shared with `other`, one unique), sessions, tokens, a passkey.
    let deck = store.create_deck(user, "Pharm", "", NOW).unwrap();
    let ids = store
        .create_cards(
            user,
            deck,
            &[card("f", "b", &["t1"]), card("g", "c", &[])],
            None,
            NOW,
        )
        .unwrap();
    let outcome = scheduler
        .review(
            &store.get_card_state(user, ids[0]).unwrap(),
            Rating::Good,
            NOW,
        )
        .unwrap();
    store
        .record_review(user, ids[0], &outcome, NOW, "web", None, None)
        .unwrap();
    let shared = "a".repeat(64);
    let unique = "b".repeat(64);
    let m1 = store
        .create_media(
            user,
            &shared,
            "x.png",
            "image/png",
            MediaKind::Image,
            10,
            NOW,
        )
        .unwrap();
    store
        .create_media(
            user,
            &unique,
            "y.png",
            "image/png",
            MediaKind::Image,
            10,
            NOW,
        )
        .unwrap();
    store.link_card_media(user, ids[0], m1).unwrap();
    store
        .create_web_session("sess-hash", user, NOW, NOW + 1000)
        .unwrap();
    store
        .create_password_reset("reset-hash", user, NOW, NOW + 1000)
        .unwrap();
    store
        .add_passkey(user, b"cred", "{}", "phone", NOW)
        .unwrap();
    store
        .create_oauth_client(
            "client-1",
            "Claude",
            &["https://x/cb".to_string()],
            "{}",
            NOW,
        )
        .unwrap();
    store
        .insert_oauth_token(
            "acc",
            "ref",
            "client-1",
            user,
            "study",
            "",
            NOW + 1000,
            NOW + 2000,
            NOW,
        )
        .unwrap();

    // The survivor references the shared blob and has a review of its own.
    let odeck = store.create_deck(other, "Keep", "", NOW).unwrap();
    let oids = store
        .create_cards(other, odeck, &[card("k", "v", &[])], None, NOW)
        .unwrap();
    store
        .create_media(
            other,
            &shared,
            "x.png",
            "image/png",
            MediaKind::Image,
            10,
            NOW,
        )
        .unwrap();
    let outcome = scheduler
        .review(
            &store.get_card_state(other, oids[0]).unwrap(),
            Rating::Good,
            NOW,
        )
        .unwrap();
    store
        .record_review(other, oids[0], &outcome, NOW, "web", None, None)
        .unwrap();

    let deleted = store.delete_user_account(user).unwrap();
    assert_eq!(
        deleted.orphan_blobs,
        vec![unique.clone()],
        "only the unshared hash is orphaned"
    );

    assert!(matches!(
        store.get_user_info(user),
        Err(StoreError::NotFound(_))
    ));
    assert!(store.get_settings(user).is_err());
    assert!(store.list_decks(user, NOW).unwrap().is_empty());
    assert!(store.passkeys_for_user(user).unwrap().is_empty());
    assert!(store
        .lookup_web_session("sess-hash", NOW)
        .unwrap()
        .is_none());
    assert!(
        !store.email_taken("t@example.com").unwrap(),
        "the address is free again"
    );
    let admin_rows = store.list_users_admin(NOW).unwrap();
    assert_eq!(admin_rows.len(), 1);
    assert_eq!(admin_rows[0].id, other);
    assert_eq!(
        admin_rows[0].review_count, 1,
        "the survivor's review log is intact"
    );
    assert_eq!(admin_rows[0].card_count, 1);

    // Deleting again is a clean not-found, and the survivor's log is still
    // protected by the trigger.
    assert!(matches!(
        store.delete_user_account(user),
        Err(StoreError::NotFound(_))
    ));
    let err = store.raw_execute_for_tests("DELETE FROM review_log");
    assert!(err.unwrap_err().to_string().contains("immutable"));

    assert_eq!(store.admin_count().unwrap(), 0);
    store.create_user("Boss", None, "admin", NOW).unwrap();
    assert_eq!(store.admin_count().unwrap(), 1);
}

#[test]
fn queue_scoping_by_deck_and_tag() {
    let (store, user) = store_with_user();
    let pharm = store.create_deck(user, "Pharm", "", NOW).unwrap();
    let anatomy = store.create_deck(user, "Anatomy", "", NOW).unwrap();
    store
        .create_cards(user, pharm, &[card("p1", "b", &["exam-2"])], None, NOW)
        .unwrap();
    store
        .create_cards(user, anatomy, &[card("a1", "b", &[])], None, NOW)
        .unwrap();

    assert_eq!(
        store
            .queue_entries(user, &StudyScope::All, NOW)
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        store
            .queue_entries(user, &StudyScope::Deck(anatomy), NOW)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        store
            .queue_entries(user, &StudyScope::Tag("EXAM-2".into()), NOW)
            .unwrap()
            .len(),
        1,
        "tag matching is case-insensitive"
    );
}

#[test]
fn sessions_and_stats() {
    let (store, user) = store_with_user();
    let deck = store.create_deck(user, "D", "", NOW).unwrap();
    let ids = store
        .create_cards(
            user,
            deck,
            &[card("f1", "b", &[]), card("f2", "b", &[])],
            None,
            NOW,
        )
        .unwrap();
    let session = store
        .start_session(user, &StudyScope::Deck(deck), NOW)
        .unwrap();
    assert_eq!(
        store.get_session_scope(user, session).unwrap(),
        StudyScope::Deck(deck)
    );

    let scheduler = Scheduler::new(None, 0.9).unwrap();
    for (i, &rating) in [Rating::Good, Rating::Again].iter().enumerate() {
        let state = store.get_card_state(user, ids[i]).unwrap();
        let outcome = scheduler.review(&state, rating, NOW).unwrap();
        store
            .record_review(user, ids[i], &outcome, NOW, "mcp", None, Some(session))
            .unwrap();
    }
    let stats = store.session_stats(user, session).unwrap();
    assert_eq!((stats.reviewed, stats.good, stats.again), (2, 1, 1));

    store.end_session(user, session, NOW + 60_000).unwrap();
    // Another user's session id must not resolve.
    let other = store.create_user("Other", None, "member", NOW).unwrap();
    assert!(matches!(
        store.get_session_scope(other, session),
        Err(StoreError::NotFound(_))
    ));
}

#[test]
fn cross_user_isolation() {
    let (store, alice) = store_with_user();
    let bob = store.create_user("Bob", None, "member", NOW).unwrap();
    let deck = store.create_deck(alice, "D", "", NOW).unwrap();
    let ids = store
        .create_cards(alice, deck, &[card("f", "b", &[])], None, NOW)
        .unwrap();

    assert!(store.get_card(bob, ids[0]).unwrap().is_none());
    assert!(store.get_card_state(bob, ids[0]).is_err());
    assert!(store
        .queue_entries(bob, &StudyScope::All, NOW)
        .unwrap()
        .is_empty());
    assert!(store.delete_card(bob, ids[0], NOW).is_err());

    // A review recorded under Bob's name against Alice's card touches
    // nothing: the state row is guarded by the card's owner.
    let before = store.get_card_state(alice, ids[0]).unwrap();
    let scheduler = Scheduler::new(None, 0.9).unwrap();
    let outcome = scheduler.review(&before, Rating::Good, NOW).unwrap();
    assert!(store
        .record_review(bob, ids[0], &outcome, NOW + 1, "web", None, None)
        .is_err());
    assert_eq!(store.get_card_state(alice, ids[0]).unwrap(), before);

    // Tags: Bob can neither attach to nor read Alice's card.
    store
        .with_conn(|conn| {
            flash_store::ext::attach_tag(conn, bob, ids[0], "stolen")?;
            assert!(flash_store::ext::card_tags(conn, bob, ids[0])?.is_empty());
            assert!(flash_store::ext::card_tags(conn, alice, ids[0])?.is_empty());
            Ok(())
        })
        .unwrap();
}

#[test]
fn web_session_lookup_carries_role() {
    let store = Store::open_in_memory().unwrap();
    let admin = store.create_user("Admin", None, "admin", NOW).unwrap();
    store
        .create_web_session("hash-a", admin, NOW, NOW + 1000)
        .unwrap();

    let (user, role) = store.lookup_web_session("hash-a", NOW).unwrap().unwrap();
    assert_eq!((user, role.as_str()), (admin, "admin"));
    // Expired session resolves to nothing.
    assert!(store
        .lookup_web_session("hash-a", NOW + 1000)
        .unwrap()
        .is_none());
}

#[test]
fn email_uniqueness_and_lookup() {
    let store = Store::open_in_memory().unwrap();
    store
        .create_user("A", Some("a@example.com"), "member", NOW)
        .unwrap();
    assert!(store.email_taken("a@example.com").unwrap());
    assert!(!store.email_taken("b@example.com").unwrap());
    // UNIQUE column rejects a duplicate outright.
    assert!(store
        .create_user("B", Some("a@example.com"), "member", NOW)
        .is_err());
    // Multiple email-less users are fine (NULLs are distinct).
    store.create_user("C", None, "member", NOW).unwrap();
    store.create_user("D", None, "member", NOW).unwrap();
}

#[test]
fn admin_user_listing_aggregates_activity() {
    let store = Store::open_in_memory().unwrap();
    let admin = store
        .create_user("Admin", Some("admin@example.com"), "admin", NOW)
        .unwrap();
    let flashtester = store
        .create_user("FlashTester", None, "member", NOW + 1)
        .unwrap();

    // Admin: a deck, two cards, one review, a used passkey, a live grant.
    let deck = store.create_deck(admin, "Pharm", "", NOW).unwrap();
    let ids = store
        .create_cards(
            admin,
            deck,
            &[card("f1", "b1", &[]), card("f2", "b2", &[])],
            None,
            NOW,
        )
        .unwrap();
    let scheduler = Scheduler::new(None, 0.9).unwrap();
    let state = store.get_card_state(admin, ids[0]).unwrap();
    let outcome = scheduler.review(&state, Rating::Good, NOW).unwrap();
    store
        .record_review(admin, ids[0], &outcome, NOW + 500, "mcp", None, None)
        .unwrap();
    store
        .add_passkey(admin, b"cred", "{}", "phone", NOW)
        .unwrap();
    store.touch_passkey(admin, 1, NOW + 900).unwrap();
    store
        .create_oauth_client("cid", "Claude", &[], "{}", NOW)
        .unwrap();
    store
        .create_oauth_client("cid2", "ChatGPT", &[], "{}", NOW)
        .unwrap();
    // Two live grants on different clients, plus a second Claude grant
    // that must not produce a duplicate name.
    store
        .insert_oauth_token(
            "ah",
            "rh",
            "cid",
            admin,
            "flash",
            "",
            NOW + 10_000,
            NOW + 20_000,
            NOW,
        )
        .unwrap();
    store
        .insert_oauth_token(
            "ah3",
            "rh3",
            "cid",
            admin,
            "flash",
            "",
            NOW + 10_000,
            NOW + 20_000,
            NOW,
        )
        .unwrap();
    store
        .insert_oauth_token(
            "ah4",
            "rh4",
            "cid2",
            admin,
            "flash",
            "",
            NOW + 10_000,
            NOW + 20_000,
            NOW,
        )
        .unwrap();
    // FlashTester: an expired grant only; must not show as connected. Her last
    // login comes from her session row (no passkey use recorded).
    store
        .insert_oauth_token(
            "ah2",
            "rh2",
            "cid",
            flashtester,
            "flash",
            "",
            NOW - 2000,
            NOW - 1000,
            NOW - 5000,
        )
        .unwrap();
    store
        .create_web_session("hash-l", flashtester, NOW + 700, NOW + 100_000)
        .unwrap();

    let rows = store.list_users_admin(NOW + 1000).unwrap();
    assert_eq!(rows.len(), 2);

    let a = &rows[0];
    assert_eq!(
        (a.display_name.as_str(), a.role.as_str()),
        ("Admin", "admin")
    );
    assert_eq!(a.email.as_deref(), Some("admin@example.com"));
    assert_eq!((a.deck_count, a.card_count, a.review_count), (1, 2, 1));
    assert_eq!(a.last_review_at, Some(NOW + 500));
    assert_eq!(a.last_review_source.as_deref(), Some("mcp"));
    assert_eq!(a.last_login_at, Some(NOW + 900));
    assert_eq!(a.mcp_clients, vec!["ChatGPT", "Claude"]);

    let l = &rows[1];
    assert_eq!(
        (l.display_name.as_str(), l.role.as_str()),
        ("FlashTester", "member")
    );
    assert_eq!(l.email, None);
    assert_eq!((l.deck_count, l.card_count, l.review_count), (0, 0, 0));
    assert_eq!(l.last_review_at, None);
    assert_eq!(l.last_login_at, Some(NOW + 700));
    assert!(l.mcp_clients.is_empty());
}

#[test]
fn review_and_card_totals() {
    let (store, user) = store_with_user();
    assert_eq!(store.review_totals(user).unwrap(), (0, None));
    assert_eq!(store.card_totals(user).unwrap(), (0, 0));

    let deck = store.create_deck(user, "D", "", NOW).unwrap();
    let ids = store
        .create_cards(
            user,
            deck,
            &[card("f1", "b1", &[]), card("f2", "b2", &[])],
            None,
            NOW,
        )
        .unwrap();
    let scheduler = Scheduler::new(None, 0.9).unwrap();
    let state = store.get_card_state(user, ids[0]).unwrap();
    let outcome = scheduler.review(&state, Rating::Good, NOW).unwrap();
    store
        .record_review(user, ids[0], &outcome, NOW, "web", None, None)
        .unwrap();
    let outcome = scheduler
        .review(
            &store.get_card_state(user, ids[0]).unwrap(),
            Rating::Good,
            NOW + 1000,
        )
        .unwrap();
    store
        .record_review(user, ids[0], &outcome, NOW + 1000, "web", None, None)
        .unwrap();

    assert_eq!(store.review_totals(user).unwrap(), (2, Some(NOW)));

    // Force one card mature: review phase with 21d+ stability.
    store
        .raw_execute_for_tests("UPDATE card_state SET phase = 2, stability = 25.0")
        .unwrap();
    assert_eq!(store.card_totals(user).unwrap(), (2, 2));
    store
        .raw_execute_for_tests("UPDATE card_state SET stability = 5.0")
        .unwrap();
    assert_eq!(store.card_totals(user).unwrap(), (2, 0));
}

// ---- the insert-time cap ----

#[test]
fn create_cards_cap_rejects_atomically() {
    let (store, user) = store_with_user();
    let deck = store.create_deck(user, "D", "", NOW).unwrap();
    store
        .create_cards(
            user,
            deck,
            &[card("f1", "b", &[]), card("f2", "b", &[])],
            Some(3),
            NOW,
        )
        .unwrap();
    let err = store
        .create_cards(
            user,
            deck,
            &[card("f3", "b", &["t"]), card("f4", "b", &[])],
            Some(3),
            NOW,
        )
        .unwrap_err();
    match err {
        StoreError::CapExceeded { current, cap } => {
            assert_eq!((current, cap), (2, 3));
        }
        other => panic!("expected CapExceeded, got {other:?}"),
    }
    // Nothing from the rejected batch landed.
    assert_eq!(store.count_cards(user, None, None).unwrap(), 2);
    // A batch that fits exactly still goes through.
    store
        .create_cards(user, deck, &[card("f3", "b", &[])], Some(3), NOW)
        .unwrap();
    assert_eq!(store.count_cards(user, None, None).unwrap(), 3);
}

#[test]
fn card_count_ignores_deleted_cards() {
    let (store, user) = store_with_user();
    let deck = store.create_deck(user, "D", "", NOW).unwrap();
    let ids = store
        .create_cards(
            user,
            deck,
            &[card("f1", "b", &[]), card("f2", "b", &[])],
            None,
            NOW,
        )
        .unwrap();
    store.delete_card(user, ids[0], NOW).unwrap();
    assert_eq!(store.count_cards(user, None, None).unwrap(), 1);
}

#[test]
fn cards_in_rotation_counts_seen_including_suspended() {
    let (store, user) = store_with_user();
    let deck = store.create_deck(user, "D", "", NOW).unwrap();
    let ids = store
        .create_cards(
            user,
            deck,
            &[
                card("f1", "b", &[]),
                card("f2", "b", &[]),
                card("f3", "b", &[]),
            ],
            None,
            NOW,
        )
        .unwrap();
    assert_eq!(store.cards_in_rotation(user).unwrap(), 0);

    // Review two cards into rotation, then suspend one of them.
    let scheduler = Scheduler::new(None, 0.9).unwrap();
    for id in &ids[..2] {
        let state = store.get_card_state(user, *id).unwrap();
        let outcome = scheduler.review(&state, Rating::Good, NOW).unwrap();
        store
            .record_review(user, *id, &outcome, NOW, "web", None, None)
            .unwrap();
    }
    store
        .raw_execute_for_tests(&format!(
            "UPDATE cards SET suspended = 1 WHERE id = {}",
            ids[0].0
        ))
        .unwrap();
    assert_eq!(store.cards_in_rotation(user).unwrap(), 2);
}

// ---- invites ----

#[test]
fn invite_lookup_and_delete() {
    let store = Store::open_in_memory().unwrap();
    store
        .create_invite("hash1", "Ada", "member", None, NOW + 1000, NOW)
        .unwrap();
    let invite = store.lookup_invite("hash1", NOW).unwrap().unwrap();
    assert_eq!(invite.display_name, "Ada");
    assert_eq!(invite.role, "member");
    // Expired invites don't resolve.
    assert!(store.lookup_invite("hash1", NOW + 1000).unwrap().is_none());

    store.delete_invite("hash1").unwrap();
    assert!(store.lookup_invite("hash1", NOW).unwrap().is_none());
}

// ---- password auth ----

#[test]
fn password_hash_crud_and_user_by_email() {
    let (store, user) = store_with_user();
    assert_eq!(store.get_password_hash(user).unwrap(), None);
    let by_email = store.user_by_email("t@example.com").unwrap().unwrap();
    assert_eq!(by_email.user, user);
    assert!(by_email.password_hash.is_none());

    store
        .set_password_hash(user, Some("$argon2id$fake"))
        .unwrap();
    assert_eq!(
        store.get_password_hash(user).unwrap().as_deref(),
        Some("$argon2id$fake")
    );
    store.set_password_hash(user, None).unwrap();
    assert_eq!(store.get_password_hash(user).unwrap(), None);
    assert!(store.user_by_email("nobody@example.com").unwrap().is_none());
}

#[test]
fn password_failures_window_and_clear() {
    let (store, user) = store_with_user();
    for _ in 0..3 {
        store.record_password_failure(user, NOW).unwrap();
    }
    let login = store.user_by_email("t@example.com").unwrap().unwrap();
    assert_eq!(login.pw_failed_count, 3);
    assert_eq!(login.pw_failed_at, Some(NOW));

    // A failure outside the window restarts the count.
    let later = NOW + flash_store::PW_LOCKOUT_WINDOW_MS + 1;
    store.record_password_failure(user, later).unwrap();
    let login = store.user_by_email("t@example.com").unwrap().unwrap();
    assert_eq!(login.pw_failed_count, 1);

    store.clear_password_failures(user).unwrap();
    let login = store.user_by_email("t@example.com").unwrap().unwrap();
    assert_eq!(login.pw_failed_count, 0);
    assert_eq!(login.pw_failed_at, None);
}

#[test]
fn password_reset_lifecycle() {
    let (store, user) = store_with_user();
    store
        .create_password_reset("rhash", user, NOW, NOW + 1000)
        .unwrap();
    assert_eq!(
        store.lookup_password_reset("rhash", NOW).unwrap(),
        Some(user)
    );
    assert_eq!(
        store.lookup_password_reset("rhash", NOW + 1001).unwrap(),
        None,
        "expired"
    );
    assert_eq!(store.recent_password_resets(user, NOW - 1).unwrap(), 1);
    assert_eq!(store.recent_password_resets(user, NOW).unwrap(), 0);

    assert_eq!(store.consume_password_reset("rhash", NOW).unwrap(), user);
    assert!(
        store.consume_password_reset("rhash", NOW).is_err(),
        "single use"
    );
    assert_eq!(store.lookup_password_reset("rhash", NOW).unwrap(), None);

    store
        .create_password_reset("rhash2", user, NOW, NOW + 1000)
        .unwrap();
    store.delete_password_reset("rhash2").unwrap();
    assert_eq!(store.lookup_password_reset("rhash2", NOW).unwrap(), None);

    store
        .create_password_reset("rhash3", user, NOW, NOW + 1)
        .unwrap();
    assert_eq!(store.purge_expired_password_resets(NOW + 2).unwrap(), 1);
}

#[test]
fn delete_sessions_for_user_honors_keep() {
    let (store, user) = store_with_user();
    let other = store.create_user("Other", None, "member", NOW).unwrap();
    store
        .create_web_session("s1", user, NOW, NOW + 1000)
        .unwrap();
    store
        .create_web_session("s2", user, NOW, NOW + 1000)
        .unwrap();
    store
        .create_web_session("s3", other, NOW, NOW + 1000)
        .unwrap();

    assert_eq!(store.delete_sessions_for_user(user, Some("s2")).unwrap(), 1);
    assert!(store.lookup_web_session("s1", NOW).unwrap().is_none());
    assert!(store.lookup_web_session("s2", NOW).unwrap().is_some());
    assert!(
        store.lookup_web_session("s3", NOW).unwrap().is_some(),
        "other user untouched"
    );

    assert_eq!(store.delete_sessions_for_user(user, None).unwrap(), 1);
    assert!(store.lookup_web_session("s2", NOW).unwrap().is_none());
}

#[test]
fn delete_deck_removes_cards_and_frees_the_name() {
    let (store, user) = store_with_user();
    let doomed = store.create_deck(user, "Doomed", "", NOW).unwrap();
    let keeper = store.create_deck(user, "Keeper", "", NOW).unwrap();
    let ids = store
        .create_cards(
            user,
            doomed,
            &[
                card("a", "1", &["t"]),
                card("b", "2", &[]),
                card("c", "3", &[]),
            ],
            None,
            NOW,
        )
        .unwrap();
    store
        .create_cards(user, keeper, &[card("k", "v", &[])], None, NOW)
        .unwrap();
    // One reviewed (review_log row must survive), one soft-deleted (must
    // still be purged so the FK and name free up).
    let scheduler = Scheduler::new(None, 0.9).unwrap();
    let outcome = scheduler
        .review(
            &store.get_card_state(user, ids[0]).unwrap(),
            Rating::Good,
            NOW,
        )
        .unwrap();
    store
        .record_review(user, ids[0], &outcome, NOW, "web", None, None)
        .unwrap();
    store.delete_card(user, ids[1], NOW).unwrap();

    // Someone else can't delete it.
    let other = store.create_user("O", None, "member", NOW).unwrap();
    assert!(matches!(
        store.delete_deck(other, doomed),
        Err(StoreError::NotFound(_))
    ));

    assert_eq!(
        store.delete_deck(user, doomed).unwrap().cards,
        3,
        "soft-deleted card counted"
    );
    let decks = store.list_decks(user, NOW).unwrap();
    assert_eq!(decks.len(), 1);
    assert_eq!(decks[0].id, keeper);
    assert_eq!(store.count_cards(user, None, None).unwrap(), 1);
    // Review history survives the purge.
    let admin_rows = store.list_users_admin(NOW).unwrap();
    assert_eq!(admin_rows[0].review_count, 1);
    // The name is free for reuse immediately.
    store.create_deck(user, "Doomed", "", NOW).unwrap();
    // Deleting again: clean not-found.
    assert!(matches!(
        store.delete_deck(user, doomed),
        Err(StoreError::NotFound(_))
    ));
}
