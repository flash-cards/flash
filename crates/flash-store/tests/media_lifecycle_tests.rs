//! Media rows follow the cards that reference them: deleting a card or
//! editing it plain drops its links, and the daily sweep removes rows no
//! card links any more once they are an hour old, reporting the hashes
//! nobody else holds so the caller can remove the blobs. Deleting a deck
//! leaves an upload made minutes ago in another tab alone.

use flash_core::{validate_card_text, UserId};
use flash_store::media::MediaKind;
use flash_store::Store;

const HOUR_MS: i64 = 3_600_000;

fn store_with_user() -> (Store, UserId) {
    let store = Store::open_in_memory().unwrap();
    let user = store
        .create_user("Ada", Some("ada@example.com"), "member", 1)
        .unwrap();
    (store, user)
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

fn media(store: &Store, user: UserId, sha: &str, created_at: i64) -> flash_core::MediaId {
    store
        .create_media(
            user,
            sha,
            "x.png",
            "image/png",
            MediaKind::Image,
            12,
            created_at,
        )
        .unwrap()
}

#[test]
fn deleting_or_plain_editing_a_card_drops_its_media_links_and_the_sweep_reaps_them() {
    let (store, user) = store_with_user();
    let now = now();
    let deck = store.create_deck(user, "D", "", now).unwrap();
    let text = validate_card_text("q", "a").unwrap();
    let ids = store
        .create_cards(
            user,
            deck,
            &[(text.clone(), vec![]), (text, vec![])],
            None,
            now,
        )
        .unwrap();
    let old = media(&store, user, &"a".repeat(64), now - 2 * HOUR_MS);
    let shared = media(&store, user, &"b".repeat(64), now - 2 * HOUR_MS);
    store.link_card_media(user, ids[0], old).unwrap();
    store.link_card_media(user, ids[0], shared).unwrap();
    store.link_card_media(user, ids[1], shared).unwrap();

    // Nothing is unlinked yet: the sweep finds nothing.
    assert!(store
        .sweep_unlinked_media(now - HOUR_MS)
        .unwrap()
        .is_empty());

    store.delete_card(user, ids[0], now).unwrap();
    // `old` lost its only link; `shared` is still held by the other card.
    let orphans = store.sweep_unlinked_media(now - HOUR_MS).unwrap();
    assert_eq!(orphans, vec!["a".repeat(64)]);
    assert!(store.get_media(user, old).unwrap().is_none());
    assert!(store.get_media(user, shared).unwrap().is_some());

    let plain = validate_card_text("plain", "edit").unwrap();
    store.update_card(user, ids[1], &plain, now).unwrap();
    let orphans = store.sweep_unlinked_media(now - HOUR_MS).unwrap();
    assert_eq!(orphans, vec!["b".repeat(64)]);
    assert!(store.get_media(user, shared).unwrap().is_none());
}

#[test]
fn a_fresh_upload_survives_the_sweep_and_a_deck_deletion() {
    let (store, user) = store_with_user();
    let now = now();
    let fresh = media(&store, user, &"c".repeat(64), now - 60_000);
    let other = store.create_deck(user, "Other", "", now).unwrap();
    store.delete_deck(user, other).unwrap();
    assert!(store
        .sweep_unlinked_media(now - HOUR_MS)
        .unwrap()
        .is_empty());
    assert!(
        store.get_media(user, fresh).unwrap().is_some(),
        "an upload made a minute ago is not garbage"
    );
    // The same row, once it is old, is.
    let stale = media(&store, user, &"d".repeat(64), now - 2 * HOUR_MS);
    assert_eq!(
        store.sweep_unlinked_media(now - HOUR_MS).unwrap(),
        vec!["d".repeat(64)]
    );
    assert!(store.get_media(user, stale).unwrap().is_none());
}
