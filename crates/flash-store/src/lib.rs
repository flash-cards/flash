//! Persistence layer: SQLite via rusqlite. All SQL lives here; callers get
//! typed methods over domain types from flash-core.
//!
//! Concurrency: one connection behind a Mutex. A handful of users on a
//! small host; WAL keeps the rare concurrent read cheap. The async server calls
//! these sync methods inside spawn_blocking.

pub mod export;
pub mod ext;
pub mod import;
pub mod media;
mod migrations;
pub mod notes;
mod pragmas;
mod repo;
pub mod richtext;
mod tmp;

use std::path::Path;
use std::sync::Arc;

use parking_lot::{Mutex, MutexGuard};

use rusqlite::{Connection, Transaction};

use ext::StoreExtension;

pub use migrations::MONOLITH_MARKER;
pub use repo::auth::{InviteInfo, StoredPasskey};
pub use repo::cards::{
    check_tags, CardExtras, CardRow, DeckLimits, DeckSummary, DeletedDeck, MAX_DECKS_PER_USER,
    MAX_TAGS_PER_CARD, MAX_TAG_LEN,
};
pub use repo::media::MediaRow;
pub use repo::notes::{NoteRow, NoteSave};
pub use repo::oauth::{
    ApiTokenInfo, CodeConsume, CodeGrant, GrantRow, OAuthClient, RefreshGrant, TokenRow,
};
pub use repo::reviews::SessionStats;
pub use repo::users::{
    AccountRow, AdminUserRow, DeletedAccount, PasswordLogin, PW_LOCKOUT_THRESHOLD,
    PW_LOCKOUT_WINDOW_MS,
};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("{0}")]
    Invalid(String),
    #[error("not found: {0}")]
    NotFound(&'static str),
    #[error("card limit reached: {current} of {cap} cards used")]
    CapExceeded { current: u32, cap: u32 },
}

impl StoreError {
    /// A UNIQUE/CHECK/FK violation from SQLite — "already exists" or "not
    /// allowed", as opposed to the database being broken. Callers turn
    /// it into a conflict answer instead of an internal error.
    pub fn is_constraint(&self) -> bool {
        matches!(
            self,
            StoreError::Db(rusqlite::Error::SqliteFailure(e, _))
                if e.code == rusqlite::ErrorCode::ConstraintViolation
        )
    }
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// Runs a mutation that must touch a row: a zero row count is
/// `NotFound(what)`. Ownership-scoped UPDATEs rely on this — the WHERE
/// carries the user id, so "not yours" and "doesn't exist" both land
/// here. Takes any connection so transactions use it too.
pub fn exec_expect_row<P: rusqlite::Params>(
    conn: &Connection,
    what: &'static str,
    sql: &str,
    params: P,
) -> Result<()> {
    if conn.execute(sql, params)? == 0 {
        return Err(StoreError::NotFound(what));
    }
    Ok(())
}

pub struct Store {
    conn: Mutex<Connection>,
    extensions: Vec<Arc<dyn StoreExtension>>,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with(path, Vec::new())
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::open_in_memory_with(Vec::new())
    }

    /// Opens with extensions, which are consulted by the core's
    /// deletions from then on.
    pub fn open_with(path: &Path, extensions: Vec<Arc<dyn StoreExtension>>) -> Result<Self> {
        Self::init(Connection::open(path)?, extensions)
    }

    pub fn open_in_memory_with(extensions: Vec<Arc<dyn StoreExtension>>) -> Result<Self> {
        Self::init(Connection::open_in_memory()?, extensions)
    }

    fn init(conn: Connection, extensions: Vec<Arc<dyn StoreExtension>>) -> Result<Self> {
        pragmas::apply(&conn)?;
        migrations::run(&conn, &extensions)?;
        Ok(Self {
            conn: Mutex::new(conn),
            extensions,
        })
    }

    /// The core's schema cursor (PRAGMA user_version).
    pub fn schema_version(&self) -> Result<i64> {
        Ok(self
            .conn()
            .pragma_query_value(None, "user_version", |r| r.get(0))?)
    }

    /// Every recorded migration as `(extension, name, applied_at)`, in
    /// the order they were recorded.
    pub fn applied_migrations(&self) -> Result<Vec<(String, String, i64)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT extension, name, applied_at FROM schema_migrations
             ORDER BY applied_at, rowid",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The one connection, locked. A panic while the guard is held cannot
    /// poison it (parking_lot has no such state); an open transaction is
    /// rolled back as its guard unwinds, so the next caller sees a clean
    /// connection rather than a permanently panicking lock.
    pub(crate) fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock()
    }

    /// Runs `f` with the connection locked, for an extension's own
    /// queries. `f` must not call any other `Store` method: the mutex is
    /// not reentrant.
    pub fn with_conn<R>(&self, f: impl FnOnce(&Connection) -> Result<R>) -> Result<R> {
        let conn = self.conn();
        f(&conn)
    }

    /// Same, inside one transaction: commits on Ok, rolls back on Err.
    pub fn with_tx<R>(&self, f: impl FnOnce(&Transaction<'_>) -> Result<R>) -> Result<R> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let out = f(&tx)?;
        tx.commit()?;
        Ok(out)
    }

    /// A real database round-trip, for health checks.
    pub fn health_check(&self) -> Result<()> {
        let one: i64 = self.conn().query_row("SELECT 1", [], |r| r.get(0))?;
        if one != 1 {
            return Err(StoreError::Invalid("SELECT 1 != 1".into()));
        }
        Ok(())
    }

    /// Test hook: raw SQL, used to prove triggers hold even outside the
    /// typed API. Not for production paths.
    #[doc(hidden)]
    pub fn raw_execute_for_tests(&self, sql: &str) -> Result<usize> {
        Ok(self.conn().execute(sql, [])?)
    }
}
