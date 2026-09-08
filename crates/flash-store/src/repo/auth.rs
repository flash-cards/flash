//! Web sessions, enrollment invites, and passkey credentials.
//! Secrets never touch the database in the clear: callers pass SHA-256
//! hex of the cookie/invite token, produced in the server crate.

use flash_core::UserId;
use rusqlite::{params, OptionalExtension};

use crate::{exec_expect_row, Result, Store, StoreError};

#[derive(Debug, Clone)]
pub struct StoredPasskey {
    pub id: i64,
    pub user_id: UserId,
    /// serde-serialized webauthn_rs::prelude::Passkey (JSON).
    pub passkey_json: String,
    pub label: String,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct InviteInfo {
    pub display_name: String,
    pub role: String,
}

impl Store {
    // ---- Web sessions ----

    pub fn create_web_session(
        &self,
        id_hash: &str,
        user: UserId,
        now_ms: i64,
        expires_ms: i64,
    ) -> Result<()> {
        self.conn().execute(
            "INSERT INTO sessions (id_hash, user_id, created_at, expires_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![id_hash, user.raw(), now_ms, expires_ms],
        )?;
        Ok(())
    }

    /// Resolves a session cookie hash to a user and their role, if valid
    /// and unexpired.
    pub fn lookup_web_session(
        &self,
        id_hash: &str,
        now_ms: i64,
    ) -> Result<Option<(UserId, String)>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT s.user_id, u.role FROM sessions s
                 JOIN users u ON u.id = s.user_id
                 WHERE s.id_hash = ?1 AND s.expires_at > ?2",
                params![id_hash, now_ms],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()?
            .map(|(id, role)| (UserId::from_db(id), role)))
    }

    pub fn delete_web_session(&self, id_hash: &str) -> Result<()> {
        self.conn()
            .execute("DELETE FROM sessions WHERE id_hash = ?1", [id_hash])?;
        Ok(())
    }

    /// Signs the user out everywhere, optionally sparing one session
    /// (password change keeps the session that made it).
    pub fn delete_sessions_for_user(&self, user: UserId, keep_hash: Option<&str>) -> Result<usize> {
        Ok(self.conn().execute(
            "DELETE FROM sessions WHERE user_id = ?1 AND (?2 IS NULL OR id_hash != ?2)",
            params![user.raw(), keep_hash],
        )?)
    }

    // ---- Password resets ----
    // Same posture as invites: token stored SHA-256-only, TTL, single-use
    // burn via used_at. Resets target an existing user, so they live in
    // their own table rather than overloading invites.

    pub fn create_password_reset(
        &self,
        token_hash: &str,
        user: UserId,
        now_ms: i64,
        expires_ms: i64,
    ) -> Result<()> {
        self.conn().execute(
            "INSERT INTO password_resets (token_hash, user_id, created_at, expires_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![token_hash, user.raw(), now_ms, expires_ms],
        )?;
        Ok(())
    }

    /// Valid (unexpired, unused) reset for this token hash, if any.
    pub fn lookup_password_reset(&self, token_hash: &str, now_ms: i64) -> Result<Option<UserId>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT user_id FROM password_resets
                 WHERE token_hash = ?1 AND used_at IS NULL AND expires_at > ?2",
                params![token_hash, now_ms],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
            .map(UserId::from_db))
    }

    /// Burns the reset; errors if already used or expired (single-use).
    pub fn consume_password_reset(&self, token_hash: &str, now_ms: i64) -> Result<UserId> {
        let user = self
            .lookup_password_reset(token_hash, now_ms)?
            .ok_or(StoreError::NotFound("password reset"))?;
        exec_expect_row(
            &self.conn(),
            "password reset",
            "UPDATE password_resets SET used_at = ?1
             WHERE token_hash = ?2 AND used_at IS NULL AND expires_at > ?1",
            params![now_ms, token_hash],
        )?;
        Ok(user)
    }

    /// Reset emails sent to this user since `since_ms` (send cooldown).
    pub fn recent_password_resets(&self, user: UserId, since_ms: i64) -> Result<u32> {
        let n: i64 = self.conn().query_row(
            "SELECT COUNT(*) FROM password_resets WHERE user_id = ?1 AND created_at > ?2",
            params![user.raw(), since_ms],
            |r| r.get(0),
        )?;
        Ok(n as u32)
    }

    /// Every reset link the user has not used. A credential change calls
    /// this: a link mailed before the change must not still set the
    /// password after it.
    pub fn delete_unused_password_resets(&self, user: UserId) -> Result<usize> {
        Ok(self.conn().execute(
            "DELETE FROM password_resets WHERE user_id = ?1 AND used_at IS NULL",
            [user.raw()],
        )?)
    }

    /// Rollback for a failed send (mirrors delete_invite).
    pub fn delete_password_reset(&self, token_hash: &str) -> Result<()> {
        self.conn().execute(
            "DELETE FROM password_resets WHERE token_hash = ?1",
            [token_hash],
        )?;
        Ok(())
    }

    pub fn purge_expired_password_resets(&self, now_ms: i64) -> Result<usize> {
        Ok(self.conn().execute(
            "DELETE FROM password_resets WHERE expires_at <= ?1",
            [now_ms],
        )?)
    }

    // ---- Invites ----

    #[allow(clippy::too_many_arguments)]
    pub fn create_invite(
        &self,
        token_hash: &str,
        display_name: &str,
        role: &str,
        created_by: Option<UserId>,
        expires_ms: i64,
        now_ms: i64,
    ) -> Result<()> {
        self.conn().execute(
            "INSERT INTO invites (token_hash, display_name, role, created_by, expires_at, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                token_hash,
                display_name,
                role,
                created_by.map(UserId::raw),
                expires_ms,
                now_ms
            ],
        )?;
        Ok(())
    }

    /// Valid (unexpired, unused) invite for this token hash, if any.
    pub fn lookup_invite(&self, token_hash: &str, now_ms: i64) -> Result<Option<InviteInfo>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT display_name, role FROM invites
                 WHERE token_hash = ?1 AND used_at IS NULL AND expires_at > ?2",
                params![token_hash, now_ms],
                |r| {
                    Ok(InviteInfo {
                        display_name: r.get(0)?,
                        role: r.get(1)?,
                    })
                },
            )
            .optional()?)
    }

    /// Removes an invite outright (rollback when the mail carrying it
    /// could not be sent).
    pub fn delete_invite(&self, token_hash: &str) -> Result<()> {
        self.conn()
            .execute("DELETE FROM invites WHERE token_hash = ?1", [token_hash])?;
        Ok(())
    }

    /// Burns the invite; errors if it was already used (single-use).
    pub fn consume_invite(&self, token_hash: &str, now_ms: i64) -> Result<()> {
        exec_expect_row(
            &self.conn(),
            "invite",
            "UPDATE invites SET used_at = ?1
             WHERE token_hash = ?2 AND used_at IS NULL AND expires_at > ?1",
            params![now_ms, token_hash],
        )
    }

    // ---- Passkeys ----

    pub fn add_passkey(
        &self,
        user: UserId,
        cred_id: &[u8],
        passkey_json: &str,
        label: &str,
        now_ms: i64,
    ) -> Result<i64> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO webauthn_credentials (user_id, cred_id, passkey, label, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![user.raw(), cred_id, passkey_json, label, now_ms],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// All passkeys (for login ceremonies the whole table is the candidate
    /// set — single-digit users by design).
    pub fn all_passkeys(&self) -> Result<Vec<StoredPasskey>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, user_id, passkey, label, created_at, last_used_at
             FROM webauthn_credentials",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(StoredPasskey {
                id: r.get(0)?,
                user_id: UserId::from_db(r.get(1)?),
                passkey_json: r.get(2)?,
                label: r.get(3)?,
                created_at: r.get(4)?,
                last_used_at: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn passkeys_for_user(&self, user: UserId) -> Result<Vec<StoredPasskey>> {
        Ok(self
            .all_passkeys()?
            .into_iter()
            .filter(|p| p.user_id == user)
            .collect())
    }

    /// Persists an updated serialized passkey (e.g. new sign counter) for
    /// one of `user`'s credentials.
    pub fn update_passkey(
        &self,
        user: UserId,
        id: i64,
        passkey_json: &str,
        now_ms: i64,
    ) -> Result<()> {
        self.conn().execute(
            "UPDATE webauthn_credentials SET passkey = ?1, last_used_at = ?2
             WHERE id = ?3 AND user_id = ?4",
            params![passkey_json, now_ms, id, user.raw()],
        )?;
        Ok(())
    }

    /// Records a successful login with this passkey. Separate from
    /// update_passkey because most authenticators never change their
    /// serialized state (sign counter stuck at 0), yet the login still
    /// happened.
    pub fn touch_passkey(&self, user: UserId, id: i64, now_ms: i64) -> Result<()> {
        self.conn().execute(
            "UPDATE webauthn_credentials SET last_used_at = ?1 WHERE id = ?2 AND user_id = ?3",
            params![now_ms, id, user.raw()],
        )?;
        Ok(())
    }

    /// Removes one of the user's passkeys; false when it wasn't theirs.
    pub fn delete_passkey(&self, user: UserId, id: i64) -> Result<bool> {
        let n = self.conn().execute(
            "DELETE FROM webauthn_credentials WHERE id = ?1 AND user_id = ?2",
            params![id, user.raw()],
        )?;
        Ok(n > 0)
    }

    pub fn user_count(&self) -> Result<u32> {
        let n: i64 = self
            .conn()
            .query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))?;
        Ok(n as u32)
    }
}
