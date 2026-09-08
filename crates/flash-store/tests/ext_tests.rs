//! The extension seam: hooks run inside the core's own deletion
//! transactions, and an extension's blob references keep a hash out of
//! the orphan list.

use std::sync::Arc;

use parking_lot::Mutex;

use flash_core::{CardText, DeckId, UserId};
use flash_store::ext::StoreExtension;
use flash_store::media::MediaKind;
use flash_store::Store;
use rusqlite::Connection;

const NOW: i64 = 1_700_000_000_000;

fn sha() -> String {
    "ab".repeat(32)
}

/// Records each hook with whether the row being deleted was still there,
/// and pins the seeded blob on request.
#[derive(Default)]
struct Probe {
    calls: Mutex<Vec<String>>,
    pins: Mutex<bool>,
}

impl StoreExtension for Probe {
    fn name(&self) -> &'static str {
        "probe"
    }

    fn before_delete_deck(
        &self,
        conn: &Connection,
        _user: UserId,
        deck: DeckId,
    ) -> flash_store::Result<()> {
        let present: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM decks WHERE id = ?1)",
            [deck.0],
            |r| r.get(0),
        )?;
        self.calls.lock().push(format!("deck:{}:{present}", deck.0));
        Ok(())
    }

    fn before_delete_user(&self, conn: &Connection, user: UserId) -> flash_store::Result<()> {
        let present: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM users WHERE id = ?1)",
            [user.raw()],
            |r| r.get(0),
        )?;
        self.calls
            .lock()
            .push(format!("user:{}:{present}", user.raw()));
        Ok(())
    }

    fn blob_refs(&self, _conn: &Connection, sha256: &str) -> flash_store::Result<i64> {
        Ok((*self.pins.lock() && sha256 == sha()) as i64)
    }
}

/// A deck with one image card whose media row names `sha()`.
fn deck_with_media(store: &Store, owner: UserId, name: &str) -> DeckId {
    let deck = store.create_deck(owner, name, "", NOW).unwrap();
    let ids = store
        .create_cards(
            owner,
            deck,
            &[(
                CardText {
                    front: "pic".into(),
                    back: "answer".into(),
                },
                vec![],
            )],
            None,
            NOW,
        )
        .unwrap();
    let media = store
        .create_media(
            owner,
            &sha(),
            "heart.png",
            "image/png",
            MediaKind::Image,
            1234,
            NOW,
        )
        .unwrap();
    store.link_card_media(owner, ids[0], media).unwrap();
    deck
}

#[test]
fn hooks_run_inside_deletions_and_pin_blobs() {
    let probe = Arc::new(Probe::default());
    let store = Store::open_in_memory_with(vec![probe.clone()]).unwrap();
    let ada = store
        .create_user("Ada", Some("ada@example.com"), "member", NOW)
        .unwrap();

    // Pinned by the extension: the blob is not an orphan.
    let deck = deck_with_media(&store, ada, "Pharm");
    *probe.pins.lock() = true;
    let deleted = store.delete_deck(ada, deck).unwrap();
    assert!(deleted.orphan_blobs.is_empty());
    assert_eq!(
        probe.calls.lock().as_slice(),
        [format!("deck:{}:true", deck.0)],
        "the hook ran while the deck row still existed"
    );

    // Unpinned: the same blob is reported for removal.
    let deck = deck_with_media(&store, ada, "Anatomy");
    *probe.pins.lock() = false;
    let deleted = store.delete_deck(ada, deck).unwrap();
    assert_eq!(deleted.orphan_blobs, vec![sha()]);

    // Account deletion consults the extension before the user's rows go.
    deck_with_media(&store, ada, "Physio");
    *probe.pins.lock() = true;
    let deleted = store.delete_user_account(ada).unwrap();
    assert!(deleted.orphan_blobs.is_empty());
    assert!(probe
        .calls
        .lock()
        .contains(&format!("user:{}:true", ada.raw())));
}

#[test]
fn with_tx_rolls_back_on_error() {
    let store = Store::open_in_memory().unwrap();
    let result: flash_store::Result<()> = store.with_tx(|tx| {
        tx.execute_batch("CREATE TABLE probe (n INTEGER)")?;
        tx.execute("INSERT INTO probe (n) VALUES (1)", [])?;
        Err(flash_store::StoreError::Invalid("stop".into()))
    });
    assert!(result.is_err());
    let exists: bool = store
        .with_conn(|conn| {
            Ok(conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name = 'probe')",
                [],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    assert!(!exists, "the transaction rolled back");
}
