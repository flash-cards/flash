//! The seam a downstream crate extends the store through. The open
//! store owns accounts, decks, cards, reviews, media and tokens; an
//! extension owns its own tables on the same connection — created by its
//! own migrations — and is consulted where a core deletion would
//! otherwise leave its rows dangling.
//!
//! Hooks receive the core's own transaction (a `&Transaction` derefs to
//! `&Connection`), so whatever they do commits or rolls back with it.
//! They must use only the connection they are handed: the store's mutex
//! is held for the duration.

use rusqlite::Connection;

use flash_core::{DeckId, UserId};

use crate::Result;

pub use crate::exec_expect_row;
pub use crate::repo::cards::{attach_tag, card_tags, require_deck, write_card_extras};

/// One schema step: a unique name (recorded once applied) and the SQL
/// that runs, in its own transaction, the first time the extension opens
/// a database that lacks it.
#[derive(Debug, Clone, Copy)]
pub struct Migration {
    pub name: &'static str,
    pub sql: &'static str,
}

pub trait StoreExtension: Send + Sync + 'static {
    /// The key the extension's migrations are recorded under.
    fn name(&self) -> &'static str;

    /// The extension's schema history, in order, append-only.
    fn migrations(&self) -> &'static [Migration] {
        &[]
    }

    /// How many leading migrations a database from before the schema
    /// registry already contains: those are recorded as applied without
    /// running when such a database is first opened.
    fn legacy_baseline(&self) -> usize {
        0
    }

    /// Runs inside `delete_deck`, after the deck's cards and notes are
    /// gone and before the deck row is. `user` owns the deck (the core
    /// checked); the extension's own rows must be scoped by it too.
    fn before_delete_deck(&self, _conn: &Connection, _user: UserId, _deck: DeckId) -> Result<()> {
        Ok(())
    }

    /// Runs inside `delete_user_account`, before the core deletes the
    /// account's own rows.
    fn before_delete_user(&self, _conn: &Connection, _user: UserId) -> Result<()> {
        Ok(())
    }

    /// How many rows of the extension's own still name this blob hash.
    /// A blob is removed from storage only when the core's count and
    /// every extension's count are all zero.
    fn blob_refs(&self, _conn: &Connection, _sha256: &str) -> Result<i64> {
        Ok(0)
    }
}
