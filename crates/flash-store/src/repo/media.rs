//! Media metadata. The blob bytes live on disk (flash-server owns that
//! IO); this repo owns the per-user rows that make a blob reachable and
//! the quota arithmetic. Every read is ownership-scoped in SQL — user A
//! can never resolve user B's media id.

use flash_core::{CardId, MediaId, UserId};
use rusqlite::Connection;

use crate::media::MediaKind;
use crate::{Result, Store, StoreError};

#[derive(Debug, Clone)]
pub struct MediaRow {
    pub id: MediaId,
    pub sha256: String,
    pub filename: String,
    pub mime: String,
    pub kind: MediaKind,
    pub size: u64,
    pub created_at: i64,
}

/// The columns `media_row_from` reads, in order.
const MEDIA_COLS: &str = "id, sha256, filename, mime, kind, size, created_at";

fn media_row_from(r: &rusqlite::Row<'_>) -> rusqlite::Result<MediaRow> {
    Ok(MediaRow {
        id: MediaId(r.get(0)?),
        sha256: r.get(1)?,
        filename: r.get(2)?,
        mime: r.get(3)?,
        kind: MediaKind::from_db(&r.get::<_, String>(4)?),
        size: r.get::<_, i64>(5)? as u64,
        created_at: r.get(6)?,
    })
}

impl Store {
    /// How many rows anywhere still name this blob hash: the core's media
    /// rows plus whatever each extension pins. Zero means the blob may
    /// leave storage. Takes the caller's connection so deletions count
    /// inside their own transaction.
    pub(crate) fn blob_refs(&self, conn: &Connection, sha256: &str) -> Result<i64> {
        let mut refs: i64 = conn.query_row(
            "SELECT COUNT(*) FROM media WHERE sha256 = ?1",
            [sha256],
            |r| r.get(0),
        )?;
        for ext in &self.extensions {
            refs += ext.blob_refs(conn, sha256)?;
        }
        Ok(refs)
    }

    /// Records a media file for a user; idempotent on (sha256, filename).
    /// Caller has already validated content and stored the blob.
    #[allow(clippy::too_many_arguments)]
    pub fn create_media(
        &self,
        user: UserId,
        sha256: &str,
        filename: &str,
        mime: &str,
        kind: MediaKind,
        size: u64,
        now_ms: i64,
    ) -> Result<MediaId> {
        if kind == MediaKind::Unsupported {
            return Err(StoreError::Invalid("unsupported media kind".into()));
        }
        let conn = self.conn();
        conn.execute(
            "INSERT INTO media (user_id, sha256, filename, mime, kind, size, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT (user_id, sha256, filename) DO NOTHING",
            rusqlite::params![
                user.raw(),
                sha256,
                filename,
                mime,
                kind.as_str(),
                size as i64,
                now_ms
            ],
        )?;
        let id: i64 = conn.query_row(
            "SELECT id FROM media WHERE user_id = ?1 AND sha256 = ?2 AND filename = ?3",
            rusqlite::params![user.raw(), sha256, filename],
            |r| r.get(0),
        )?;
        Ok(MediaId(id))
    }

    /// Ownership-scoped lookup: None when the id doesn't exist *or* belongs
    /// to someone else (indistinguishable to the caller, by design).
    pub fn get_media(&self, user: UserId, id: MediaId) -> Result<Option<MediaRow>> {
        let conn = self.conn();
        let row = conn
            .query_row(
                &format!("SELECT {MEDIA_COLS} FROM media WHERE id = ?1 AND user_id = ?2"),
                rusqlite::params![id.0, user.raw()],
                media_row_from,
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })?;
        Ok(row)
    }

    /// Whether any user already holds a blob with this hash — the upload
    /// can be skipped, the store is content-addressed.
    pub fn blob_known(&self, sha256: &str) -> Result<bool> {
        Ok(self.conn().query_row(
            "SELECT EXISTS(SELECT 1 FROM media WHERE sha256 = ?1)",
            [sha256],
            |r| r.get(0),
        )?)
    }

    /// How many media rows a user has, for the per-account object cap:
    /// bytes alone let a flood of tiny files exhaust a volume's inodes.
    pub fn media_count(&self, user: UserId) -> Result<u64> {
        let n: i64 = self.conn().query_row(
            "SELECT COUNT(*) FROM media WHERE user_id = ?1",
            [user.raw()],
            |r| r.get(0),
        )?;
        Ok(n.max(0) as u64)
    }

    /// Total media bytes a user has stored (for quota checks).
    pub fn media_bytes_used(&self, user: UserId) -> Result<u64> {
        let sum: i64 = self.conn().query_row(
            "SELECT COALESCE(SUM(size), 0) FROM media WHERE user_id = ?1",
            [user.raw()],
            |r| r.get(0),
        )?;
        Ok(sum as u64)
    }

    /// Links a card to a media row it references (both ownership-checked).
    pub fn link_card_media(&self, user: UserId, card: CardId, media: MediaId) -> Result<()> {
        let n = self.conn().execute(
            "INSERT OR IGNORE INTO card_media (card_id, media_id)
             SELECT c.id, m.id FROM cards c, media m
             WHERE c.id = ?1 AND c.user_id = ?3 AND m.id = ?2 AND m.user_id = ?3",
            rusqlite::params![card.0, media.0, user.raw()],
        )?;
        // 0 rows = already linked OR not owned; verify ownership explicitly.
        if n == 0 && self.get_media(user, media)?.is_none() {
            return Err(StoreError::NotFound("media"));
        }
        Ok(())
    }

    /// Deletes a user's media row (and its card links), returning how many
    /// rows across ALL users still reference the same blob hash — 0 means
    /// the caller may remove the blob from disk.
    pub fn delete_media(&self, user: UserId, id: MediaId) -> Result<u64> {
        let conn = self.conn();
        let sha: String = conn
            .query_row(
                "SELECT sha256 FROM media WHERE id = ?1 AND user_id = ?2",
                rusqlite::params![id.0, user.raw()],
                |r| r.get(0),
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => StoreError::NotFound("media"),
                other => StoreError::Db(other),
            })?;
        conn.execute(
            "DELETE FROM card_media WHERE media_id = ?1
               AND EXISTS (SELECT 1 FROM media WHERE id = ?1 AND user_id = ?2)",
            rusqlite::params![id.0, user.raw()],
        )?;
        conn.execute(
            "DELETE FROM media WHERE id = ?1 AND user_id = ?2",
            rusqlite::params![id.0, user.raw()],
        )?;
        Ok(self.blob_refs(&conn, &sha)? as u64)
    }

    /// Media rows no card links any more, older than `older_than_ms`, for
    /// every account: the rows go, and the hashes nobody holds any more
    /// come back for the caller to remove from storage. Housekeeping
    /// runs this daily; the age floor keeps an editor upload that is
    /// about to be linked (a note saved in the next minute) out of it.
    pub fn sweep_unlinked_media(&self, older_than_ms: i64) -> Result<Vec<String>> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let unlinked: Vec<(i64, i64, String)> = {
            let mut stmt = tx.prepare(
                "SELECT m.id, m.user_id, m.sha256 FROM media m
                 WHERE m.created_at < ?1
                   AND NOT EXISTS (SELECT 1 FROM card_media cm WHERE cm.media_id = m.id)",
            )?;
            let rows =
                stmt.query_map([older_than_ms], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        for (id, user_id, _) in &unlinked {
            tx.execute(
                "DELETE FROM media WHERE id = ?1 AND user_id = ?2",
                rusqlite::params![id, user_id],
            )?;
        }
        let mut orphans = Vec::new();
        for (_, _, sha) in unlinked {
            if self.blob_refs(&tx, &sha)? == 0 && !orphans.contains(&sha) {
                orphans.push(sha);
            }
        }
        tx.commit()?;
        Ok(orphans)
    }

    /// Media rows referenced by the user's live (non-deleted) cards — what
    /// an export has to bundle. Ordered by id so exported filenames are
    /// stable across runs.
    pub fn media_for_export(&self, user: UserId) -> Result<Vec<MediaRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT DISTINCT m.id, m.sha256, m.filename, m.mime, m.kind, m.size, m.created_at
             FROM media m
             JOIN card_media cm ON cm.media_id = m.id
             JOIN cards c ON c.id = cm.card_id
             WHERE m.user_id = ?1 AND c.user_id = ?1 AND c.deleted_at IS NULL
             ORDER BY m.id",
        )?;
        let rows = stmt.query_map([user.raw()], media_row_from)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }
}
