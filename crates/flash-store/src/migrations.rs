//! Schema history. The core's steps are keyed on PRAGMA user_version and
//! run once, in order, each in a transaction; an extension's steps are
//! keyed by name in `schema_migrations`, which every open creates. Never
//! edit a shipped step — append.
//!
//! A database from before the registry existed (the single-binary era,
//! schema 14) is recognised by its version and an empty registry: its
//! rows are recorded as applied without running anything, under the
//! marker `('core', "monolith_v14")`, and each extension then adopts its
//! own leading `legacy_baseline()` steps the same way.

use std::collections::HashSet;
use std::sync::Arc;

use rusqlite::{params, Connection};

use crate::ext::{Migration, StoreExtension};
use crate::{Result, StoreError};

/// The registry row a pre-registry database receives when first opened.
pub const MONOLITH_MARKER: &str = "monolith_v14";
/// The only pre-registry schema a database may arrive at.
const LAST_MONOLITH_VERSION: i64 = 14;

const CORE: &[Migration] = &[
    Migration {
        name: "v001_initial",
        sql: V001_INITIAL,
    },
    Migration {
        name: "v002_password_auth",
        sql: V002_PASSWORD_AUTH,
    },
    Migration {
        name: "v003_reserved",
        sql: V003_RESERVED,
    },
    Migration {
        name: "v004_media",
        sql: V004_MEDIA,
    },
    Migration {
        name: "v005_rich_cards",
        sql: V005_RICH_CARDS,
    },
    Migration {
        name: "v006_limits",
        sql: V006_LIMITS,
    },
    Migration {
        name: "v007_connect_cta",
        sql: V007_CONNECT_CTA,
    },
    Migration {
        name: "v008_account_deletion",
        sql: V008_ACCOUNT_DELETION,
    },
    Migration {
        name: "v009_notes",
        sql: V009_NOTES,
    },
    Migration {
        name: "v010_reserved",
        sql: V010_RESERVED,
    },
    Migration {
        name: "v011_reserved",
        sql: V011_RESERVED,
    },
    Migration {
        name: "v012_mobile",
        sql: V012_MOBILE,
    },
    Migration {
        name: "v013_reserved",
        sql: V013_RESERVED,
    },
    Migration {
        name: "v014_reserved",
        sql: V014_RESERVED,
    },
    Migration {
        name: "v015_review_client",
        sql: V015_REVIEW_CLIENT,
    },
];

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn applied(conn: &Connection, extension: &str) -> Result<HashSet<String>> {
    let mut stmt = conn.prepare("SELECT name FROM schema_migrations WHERE extension = ?1")?;
    let rows = stmt.query_map([extension], |r| r.get::<_, String>(0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn record(conn: &Connection, extension: &str, name: &str, now: i64) -> Result<()> {
    conn.execute(
        "INSERT INTO schema_migrations (extension, name, applied_at) VALUES (?1, ?2, ?3)",
        params![extension, name, now],
    )?;
    Ok(())
}

/// Runs one step and records it, atomically.
fn apply(conn: &Connection, extension: &str, m: &Migration, version: Option<i64>) -> Result<()> {
    let bump = version
        .map(|v| format!("PRAGMA user_version = {v};\n"))
        .unwrap_or_default();
    conn.execute_batch(&format!(
        "BEGIN;\n{}\n{bump}INSERT INTO schema_migrations (extension, name, applied_at) \
         VALUES ('{extension}', '{}', {});\nCOMMIT;",
        m.sql,
        m.name,
        now_ms()
    ))?;
    Ok(())
}

pub fn run(conn: &Connection, extensions: &[Arc<dyn StoreExtension>]) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
           extension TEXT NOT NULL,
           name TEXT NOT NULL,
           applied_at INTEGER NOT NULL,
           PRIMARY KEY (extension, name)
         ) STRICT;",
    )?;
    let current: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    let registered: i64 =
        conn.query_row("SELECT COUNT(*) FROM schema_migrations", [], |r| r.get(0))?;

    // A pre-registry database: adopt it whole, or refuse it.
    if current > 0 && registered == 0 {
        if current != LAST_MONOLITH_VERSION {
            return Err(StoreError::Invalid(format!(
                "database is at schema version {current}; upgrade it with the last \
                 single-binary release (schema {LAST_MONOLITH_VERSION}) first"
            )));
        }
        let tx = conn.unchecked_transaction()?;
        let now = now_ms();
        record(&tx, "core", MONOLITH_MARKER, now)?;
        // Only the steps the monolith already contains are adopted; anything
        // newer runs below like on any other database.
        for m in CORE.iter().take(LAST_MONOLITH_VERSION as usize) {
            record(&tx, "core", m.name, now)?;
        }
        tx.commit()?;
    }

    for (i, m) in CORE.iter().enumerate() {
        let version = (i + 1) as i64;
        if version > current {
            apply(conn, "core", m, Some(version))?;
        }
    }

    let adopted_monolith: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM schema_migrations WHERE extension = 'core' AND name = ?1)",
        [MONOLITH_MARKER],
        |r| r.get(0),
    )?;
    for ext in extensions {
        let name = ext.name();
        let steps = ext.migrations();
        if adopted_monolith && applied(conn, name)?.is_empty() {
            let tx = conn.unchecked_transaction()?;
            let now = now_ms();
            for m in steps.iter().take(ext.legacy_baseline()) {
                record(&tx, name, m.name, now)?;
            }
            tx.commit()?;
        }
        let done = applied(conn, name)?;
        for m in steps {
            if !done.contains(m.name) {
                apply(conn, name, m, None)?;
            }
        }
    }
    Ok(())
}

/// Timestamps are epoch milliseconds (FSRS cares about sub-day elapsed time).
/// Secrets (session ids, invite/oauth tokens and codes) are stored only as
/// SHA-256 hex. review_log is append-only, enforced by triggers (the delete
/// trigger is relaxed in V008 for rows whose owner no longer exists, so an
/// account can be purged).
const V001_INITIAL: &str = r#"
CREATE TABLE users (
  id INTEGER PRIMARY KEY,
  display_name TEXT NOT NULL,
  email TEXT UNIQUE,
  role TEXT NOT NULL DEFAULT 'member' CHECK (role IN ('admin','member')),
  created_at INTEGER NOT NULL
) STRICT;

CREATE TABLE user_settings (
  user_id INTEGER PRIMARY KEY REFERENCES users(id),
  grading_mode TEXT NOT NULL DEFAULT 'silent'
    CHECK (grading_mode IN ('silent','announce','self')),
  desired_retention REAL NOT NULL DEFAULT 0.9,
  new_per_day INTEGER NOT NULL DEFAULT 20,
  day_cutoff_hour INTEGER NOT NULL DEFAULT 4,
  timezone TEXT NOT NULL DEFAULT 'America/New_York',
  fsrs_params TEXT
) STRICT;

CREATE TABLE webauthn_credentials (
  id INTEGER PRIMARY KEY,
  user_id INTEGER NOT NULL REFERENCES users(id),
  cred_id BLOB NOT NULL UNIQUE,
  passkey TEXT NOT NULL,
  label TEXT NOT NULL DEFAULT '',
  created_at INTEGER NOT NULL,
  last_used_at INTEGER
) STRICT;

CREATE TABLE invites (
  id INTEGER PRIMARY KEY,
  token_hash TEXT NOT NULL UNIQUE,
  role TEXT NOT NULL DEFAULT 'member',
  display_name TEXT NOT NULL,
  created_by INTEGER REFERENCES users(id),
  expires_at INTEGER NOT NULL,
  used_at INTEGER
) STRICT;

CREATE TABLE sessions (
  id_hash TEXT PRIMARY KEY,
  user_id INTEGER NOT NULL REFERENCES users(id),
  created_at INTEGER NOT NULL,
  expires_at INTEGER NOT NULL
) STRICT;

CREATE TABLE oauth_clients (
  id INTEGER PRIMARY KEY,
  client_id TEXT NOT NULL UNIQUE,
  client_name TEXT NOT NULL,
  redirect_uris TEXT NOT NULL,
  metadata TEXT NOT NULL,
  created_at INTEGER NOT NULL
) STRICT;

CREATE TABLE oauth_codes (
  code_hash TEXT PRIMARY KEY,
  client_id TEXT NOT NULL,
  user_id INTEGER NOT NULL REFERENCES users(id),
  redirect_uri TEXT NOT NULL,
  code_challenge TEXT NOT NULL,
  scope TEXT NOT NULL,
  resource TEXT,
  expires_at INTEGER NOT NULL,
  used_at INTEGER
) STRICT;

CREATE TABLE oauth_tokens (
  id INTEGER PRIMARY KEY,
  access_hash TEXT NOT NULL UNIQUE,
  refresh_hash TEXT UNIQUE,
  client_id TEXT NOT NULL,
  user_id INTEGER NOT NULL REFERENCES users(id),
  scope TEXT NOT NULL,
  access_expires_at INTEGER NOT NULL,
  refresh_expires_at INTEGER,
  created_at INTEGER NOT NULL,
  revoked_at INTEGER
) STRICT;
CREATE INDEX idx_tokens_user ON oauth_tokens(user_id);

CREATE TABLE decks (
  id INTEGER PRIMARY KEY,
  user_id INTEGER NOT NULL REFERENCES users(id),
  name TEXT NOT NULL,
  description TEXT NOT NULL DEFAULT '',
  created_at INTEGER NOT NULL,
  UNIQUE (user_id, name)
) STRICT;

CREATE TABLE cards (
  id INTEGER PRIMARY KEY,
  user_id INTEGER NOT NULL REFERENCES users(id),
  deck_id INTEGER NOT NULL REFERENCES decks(id),
  front TEXT NOT NULL,
  back TEXT NOT NULL,
  suspended INTEGER NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  deleted_at INTEGER
) STRICT;
CREATE INDEX idx_cards_deck ON cards(deck_id) WHERE deleted_at IS NULL;

CREATE TABLE tags (
  id INTEGER PRIMARY KEY,
  user_id INTEGER NOT NULL REFERENCES users(id),
  name TEXT NOT NULL,
  UNIQUE (user_id, name)
) STRICT;

CREATE TABLE card_tags (
  card_id INTEGER NOT NULL REFERENCES cards(id),
  tag_id INTEGER NOT NULL REFERENCES tags(id),
  PRIMARY KEY (card_id, tag_id)
) WITHOUT ROWID;

CREATE TABLE card_state (
  card_id INTEGER PRIMARY KEY REFERENCES cards(id),
  phase INTEGER NOT NULL DEFAULT 0,
  stability REAL,
  difficulty REAL,
  due INTEGER NOT NULL,
  last_review INTEGER,
  reps INTEGER NOT NULL DEFAULT 0,
  lapses INTEGER NOT NULL DEFAULT 0
) STRICT;
CREATE INDEX idx_state_due ON card_state(due);

CREATE TABLE review_log (
  id INTEGER PRIMARY KEY,
  card_id INTEGER NOT NULL,
  user_id INTEGER NOT NULL,
  reviewed_at INTEGER NOT NULL,
  rating INTEGER NOT NULL CHECK (rating BETWEEN 1 AND 4),
  phase_before INTEGER NOT NULL,
  elapsed_ms INTEGER NOT NULL,
  stability_after REAL NOT NULL,
  difficulty_after REAL NOT NULL,
  due_after INTEGER NOT NULL,
  source TEXT NOT NULL CHECK (source IN ('mcp','web','import')),
  session_id INTEGER
) STRICT;
CREATE INDEX idx_review_card ON review_log(card_id, reviewed_at);
CREATE INDEX idx_review_user_time ON review_log(user_id, reviewed_at);

CREATE TRIGGER review_log_no_update BEFORE UPDATE ON review_log
BEGIN SELECT RAISE(ABORT, 'review_log is immutable'); END;
CREATE TRIGGER review_log_no_delete BEFORE DELETE ON review_log
BEGIN SELECT RAISE(ABORT, 'review_log is immutable'); END;

CREATE TABLE study_sessions (
  id INTEGER PRIMARY KEY,
  user_id INTEGER NOT NULL REFERENCES users(id),
  started_at INTEGER NOT NULL,
  ended_at INTEGER,
  scope TEXT NOT NULL
) STRICT;
"#;

/// Password auth (optional alongside passkeys). password_hash is a PHC
/// string (Argon2id) — a verifier, not a recoverable secret. The pw_failed
/// pair implements a small DB-backed lockout window. Invites learn when
/// they were minted. (Columns a downstream extension added in this slot
/// live in that extension's own baseline migration.)
const V002_PASSWORD_AUTH: &str = r#"
ALTER TABLE invites ADD COLUMN created_at INTEGER;

ALTER TABLE users ADD COLUMN password_hash TEXT;
ALTER TABLE users ADD COLUMN pw_failed_count INTEGER NOT NULL DEFAULT 0;
ALTER TABLE users ADD COLUMN pw_failed_at INTEGER;

CREATE TABLE password_resets (
  token_hash TEXT PRIMARY KEY,
  user_id INTEGER NOT NULL REFERENCES users(id),
  created_at INTEGER NOT NULL,
  expires_at INTEGER NOT NULL,
  used_at INTEGER
) STRICT;
CREATE INDEX idx_password_resets_user ON password_resets(user_id);

CREATE INDEX idx_cards_user ON cards(user_id) WHERE deleted_at IS NULL;
"#;

/// Slot kept so the numbering stays stable: what shipped here in the
/// single-binary history belongs to a downstream extension.
const V003_RESERVED: &str = "-- reserved for a downstream extension";

/// Media metadata. Blobs live on disk under data_dir/media, named by
/// sha256 (content-addressed: hostile filenames never touch the
/// filesystem, identical files dedupe). Rows are per-user references to a
/// blob; a blob is deletable only when no row across any user names its
/// hash. `filename` is sanitized display metadata, never a path.
const V004_MEDIA: &str = r#"
CREATE TABLE media (
  id INTEGER PRIMARY KEY,
  user_id INTEGER NOT NULL REFERENCES users(id),
  sha256 TEXT NOT NULL,
  filename TEXT NOT NULL,
  mime TEXT NOT NULL,
  kind TEXT NOT NULL CHECK (kind IN ('image','audio','video')),
  size INTEGER NOT NULL,
  created_at INTEGER NOT NULL,
  UNIQUE (user_id, sha256, filename)
) STRICT;
CREATE INDEX idx_media_user ON media(user_id);
CREATE INDEX idx_media_sha ON media(sha256);

CREATE TABLE card_media (
  card_id INTEGER NOT NULL REFERENCES cards(id),
  media_id INTEGER NOT NULL REFERENCES media(id),
  PRIMARY KEY (card_id, media_id)
) WITHOUT ROWID;
"#;

/// Dual card representation and Anki fidelity. front/back stay the plain
/// text every surface (MCP, CSV, search) uses; *_html is the sanitized
/// semantic-HTML view for the web UI (NULL = plain card). cloze_text +
/// cloze_index preserve the original cloze source for round-trip export;
/// type_answer drives the interactive typing UI. Editing a card clears
/// all five (a hand-edited card is plain by definition).
const V005_RICH_CARDS: &str = r#"
ALTER TABLE cards ADD COLUMN front_html TEXT;
ALTER TABLE cards ADD COLUMN back_html TEXT;
ALTER TABLE cards ADD COLUMN cloze_text TEXT;
ALTER TABLE cards ADD COLUMN cloze_index INTEGER;
ALTER TABLE cards ADD COLUMN type_answer TEXT;
"#;

/// Daily limits, Anki-style: the account carries defaults (new/day,
/// reviews/day), each deck may override either (NULL = inherit), and both
/// levels carry a "more new cards today" boost keyed to the study-day
/// start it was granted in (stale boost_day = no boost).
const V006_LIMITS: &str = r#"
ALTER TABLE user_settings ADD COLUMN reviews_per_day INTEGER NOT NULL DEFAULT 200;
ALTER TABLE user_settings ADD COLUMN boost_new INTEGER NOT NULL DEFAULT 0;
ALTER TABLE user_settings ADD COLUMN boost_day INTEGER NOT NULL DEFAULT 0;
ALTER TABLE decks ADD COLUMN new_per_day INTEGER;
ALTER TABLE decks ADD COLUMN reviews_per_day INTEGER;
ALTER TABLE decks ADD COLUMN boost_new INTEGER NOT NULL DEFAULT 0;
ALTER TABLE decks ADD COLUMN boost_day INTEGER NOT NULL DEFAULT 0;
"#;

/// The Today page's "Connect your AI" card can be dismissed for good.
const V007_CONNECT_CTA: &str = r#"
ALTER TABLE user_settings ADD COLUMN hide_connect_cta INTEGER NOT NULL DEFAULT 0;
"#;

/// Self-serve account deletion. review_log stays immutable for every live
/// account; only rows whose user row is already gone may be deleted, which
/// is exactly the last step of Store::delete_user_account.
const V008_ACCOUNT_DELETION: &str = r#"
DROP TRIGGER review_log_no_delete;
CREATE TRIGGER review_log_no_delete BEFORE DELETE ON review_log
WHEN EXISTS (SELECT 1 FROM users WHERE id = OLD.user_id)
BEGIN SELECT RAISE(ABORT, 'review_log is immutable'); END;
"#;

/// Notes: the editable unit behind sibling cards (Anki's model). A note
/// holds the editor source — Front/Back, or cloze Text/Extra with the raw
/// `{{cN::...}}` markup — and fans out into cards by `ord` (reversed: 0
/// and 1; cloze: index - 1). Editing a note regenerates its cards in
/// place, so review state survives. Cards created before this migration
/// (and by MCP or the quick-add form) have no note; they behave as
/// single-card Basic notes and are adopted into a real note on their
/// first rich edit.
const V009_NOTES: &str = r#"
CREATE TABLE notes (
  id INTEGER PRIMARY KEY,
  user_id INTEGER NOT NULL REFERENCES users(id),
  deck_id INTEGER NOT NULL REFERENCES decks(id),
  note_type TEXT NOT NULL CHECK (note_type IN ('basic','basic_reversed','basic_typed','cloze')),
  front_html TEXT NOT NULL,
  back_html TEXT NOT NULL DEFAULT '',
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  deleted_at INTEGER
) STRICT;
CREATE INDEX idx_notes_deck ON notes(deck_id) WHERE deleted_at IS NULL;
ALTER TABLE cards ADD COLUMN note_id INTEGER REFERENCES notes(id);
ALTER TABLE cards ADD COLUMN ord INTEGER NOT NULL DEFAULT 0;
CREATE UNIQUE INDEX idx_cards_note_ord ON cards(note_id, ord) WHERE note_id IS NOT NULL AND deleted_at IS NULL;
"#;

/// Slot kept so the numbering stays stable (see V003).
const V010_RESERVED: &str = "-- reserved for a downstream extension";

/// Slot kept so the numbering stays stable (see V003).
const V011_RESERVED: &str = "-- reserved for a downstream extension";

/// The mobile app. Bearer tokens minted for the first-party client carry
/// a device label and a last-use time (Settings → "sign out other
/// devices"); the theme moves from a browser cookie into settings so the
/// app and the web agree. Finally `review_log.source` gains 'mobile'.
/// SQLite cannot alter a CHECK, so the table is rebuilt: same columns,
/// both indexes and both triggers recreated (the delete trigger in its
/// V008 form). DROP TABLE fires no row triggers, and nothing references
/// review_log by foreign key. (Tables a downstream extension added in
/// this slot live in that extension's own baseline migration.)
const V012_MOBILE: &str = r#"
ALTER TABLE oauth_tokens ADD COLUMN label TEXT NOT NULL DEFAULT '';
ALTER TABLE oauth_tokens ADD COLUMN last_used_at INTEGER;
ALTER TABLE user_settings ADD COLUMN theme TEXT NOT NULL DEFAULT 'dark';

CREATE TABLE review_log_v12 (
  id INTEGER PRIMARY KEY,
  card_id INTEGER NOT NULL,
  user_id INTEGER NOT NULL,
  reviewed_at INTEGER NOT NULL,
  rating INTEGER NOT NULL CHECK (rating BETWEEN 1 AND 4),
  phase_before INTEGER NOT NULL,
  elapsed_ms INTEGER NOT NULL,
  stability_after REAL NOT NULL,
  difficulty_after REAL NOT NULL,
  due_after INTEGER NOT NULL,
  source TEXT NOT NULL CHECK (source IN ('mcp','web','import','mobile')),
  session_id INTEGER
) STRICT;
INSERT INTO review_log_v12
  SELECT id, card_id, user_id, reviewed_at, rating, phase_before, elapsed_ms,
         stability_after, difficulty_after, due_after, source, session_id
  FROM review_log;
DROP TRIGGER review_log_no_update;
DROP TRIGGER review_log_no_delete;
DROP TABLE review_log;
ALTER TABLE review_log_v12 RENAME TO review_log;
CREATE INDEX idx_review_card ON review_log(card_id, reviewed_at);
CREATE INDEX idx_review_user_time ON review_log(user_id, reviewed_at);
CREATE TRIGGER review_log_no_update BEFORE UPDATE ON review_log
BEGIN SELECT RAISE(ABORT, 'review_log is immutable'); END;
CREATE TRIGGER review_log_no_delete BEFORE DELETE ON review_log
WHEN EXISTS (SELECT 1 FROM users WHERE id = OLD.user_id)
BEGIN SELECT RAISE(ABORT, 'review_log is immutable'); END;
"#;

/// Slot kept so the numbering stays stable (see V003).
const V013_RESERVED: &str = "-- reserved for a downstream extension";

/// Slot kept so the numbering stays stable (see V003).
const V014_RESERVED: &str = "-- reserved for a downstream extension";

/// Which client a review came through when the surface has several: the
/// OAuth client's name for a review over MCP, the platform for the app.
/// NULL for the web. Adding a column does not fire review_log's
/// immutability triggers.
const V015_REVIEW_CLIENT: &str = "ALTER TABLE review_log ADD COLUMN client TEXT;";
