//! OAuth 2.1 storage: dynamically-registered clients, single-use codes,
//! and opaque tokens. Callers pass SHA-256 hex of codes/tokens only.

use flash_core::UserId;
use rusqlite::{params, OptionalExtension};

use crate::{Result, Store, StoreError};

#[derive(Debug, Clone)]
pub struct OAuthClient {
    pub client_id: String,
    pub client_name: String,
    pub redirect_uris: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct CodeGrant {
    pub client_id: String,
    pub user_id: UserId,
    pub redirect_uri: String,
    pub code_challenge: String,
    pub scope: String,
}

/// Result of trying to consume an authorization code.
pub enum CodeConsume {
    Fresh(CodeGrant),
    /// The code was already used — OAuth 2.1 requires revoking the grant.
    Replayed {
        client_id: String,
        user_id: UserId,
    },
    Invalid,
}

#[derive(Debug, Clone)]
pub struct RefreshGrant {
    pub client_id: String,
    pub user_id: UserId,
    pub scope: String,
}

/// What a valid access token says about its bearer: enough for an API
/// extractor to decide both "who" and "which client minted this".
#[derive(Debug, Clone)]
pub struct ApiTokenInfo {
    pub id: i64,
    pub user: UserId,
    pub role: String,
    pub client_id: String,
    /// The client's registered display name ("Claude", "ChatGPT", …).
    pub client_name: String,
    pub scope: String,
}

/// One live grant as listed in Settings ("this iPhone", "iPad", …).
#[derive(Debug, Clone)]
pub struct TokenRow {
    pub id: i64,
    pub label: String,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
}

/// One client the user has live grants with, as the settings page lists
/// it: an MCP connector registered dynamically (named by its
/// registration) or the first-party app (no registration row, one
/// token per device).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantRow {
    pub client_id: String,
    /// The registered `client_name`; empty for a client with no
    /// registration row.
    pub client_name: String,
    /// Live tokens under this client.
    pub tokens: u32,
    pub first_granted_at: i64,
    pub last_used_at: Option<i64>,
}

/// Grace after a token is revoked or its refresh half expires before the
/// row is purged: long enough to investigate a report, short enough that
/// the table stays bounded.
const TOKEN_PURGE_GRACE_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// How long a registered client that never completed an authorization is
/// kept. A real connector setup finishes in minutes; a day is generous.
const CLIENT_PURGE_AGE_MS: i64 = 24 * 60 * 60 * 1000;

impl Store {
    pub fn create_oauth_client(
        &self,
        client_id: &str,
        client_name: &str,
        redirect_uris: &[String],
        metadata_json: &str,
        now_ms: i64,
    ) -> Result<()> {
        // A list of strings always serializes; the sentence is fixed so
        // no library text can reach a client through it.
        let uris = serde_json::to_string(redirect_uris)
            .map_err(|_| StoreError::Invalid("redirect uris could not be stored".into()))?;
        self.conn().execute(
            "INSERT INTO oauth_clients (client_id, client_name, redirect_uris, metadata, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![client_id, client_name, uris, metadata_json, now_ms],
        )?;
        Ok(())
    }

    /// Revokes every live grant the user holds, for any client, except
    /// `keep`. What a credential change calls: a password change or reset
    /// must end every session that credential (or its absence) allowed,
    /// and connector grants are sessions too.
    pub fn revoke_all_tokens_for_user(
        &self,
        user: UserId,
        keep: Option<i64>,
        now_ms: i64,
    ) -> Result<usize> {
        Ok(self.conn().execute(
            "UPDATE oauth_tokens SET revoked_at = ?1
             WHERE user_id = ?2 AND revoked_at IS NULL
               AND (?3 IS NULL OR id != ?3)",
            params![now_ms, user.raw(), keep],
        )?)
    }

    /// Authorization codes past their expiry, spent or not: each one is
    /// a row a signed-in member could mint at the limiter's pace, and
    /// nothing reads an expired one.
    pub fn purge_expired_oauth_codes(&self, now_ms: i64) -> Result<usize> {
        Ok(self
            .conn()
            .execute("DELETE FROM oauth_codes WHERE expires_at < ?1", [now_ms])?)
    }

    /// Removes registered clients that never obtained a grant and are
    /// older than `CLIENT_PURGE_AGE_MS`. Registration is unauthenticated,
    /// so without this the table grows with every bot that finds it; a
    /// client mid-authorization is younger than the cutoff by definition.
    pub fn purge_unused_oauth_clients(&self, now_ms: i64) -> Result<usize> {
        let cutoff = now_ms - CLIENT_PURGE_AGE_MS;
        Ok(self.conn().execute(
            "DELETE FROM oauth_clients
             WHERE created_at < ?1
               AND NOT EXISTS (SELECT 1 FROM oauth_tokens t WHERE t.client_id = oauth_clients.client_id)
               AND NOT EXISTS (SELECT 1 FROM oauth_codes c WHERE c.client_id = oauth_clients.client_id)",
            [cutoff],
        )?)
    }

    pub fn get_oauth_client(&self, client_id: &str) -> Result<Option<OAuthClient>> {
        let row = self
            .conn()
            .query_row(
                "SELECT client_name, redirect_uris FROM oauth_clients WHERE client_id = ?1",
                [client_id],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()?;
        match row {
            None => Ok(None),
            Some((client_name, uris)) => Ok(Some(OAuthClient {
                client_id: client_id.to_string(),
                client_name,
                // The column was written by `create_oauth_client` above;
                // a row that does not parse is corruption, and the
                // sentence stays fixed either way.
                redirect_uris: serde_json::from_str(&uris)
                    .map_err(|_| StoreError::Invalid("client record is unreadable".into()))?,
            })),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_oauth_code(
        &self,
        code_hash: &str,
        client_id: &str,
        user: UserId,
        redirect_uri: &str,
        code_challenge: &str,
        scope: &str,
        resource: Option<&str>,
        expires_ms: i64,
    ) -> Result<()> {
        self.conn().execute(
            "INSERT INTO oauth_codes
               (code_hash, client_id, user_id, redirect_uri, code_challenge, scope, resource, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![code_hash, client_id, user.raw(), redirect_uri, code_challenge, scope, resource, expires_ms],
        )?;
        Ok(())
    }

    /// Single-use consumption with replay detection.
    pub fn consume_oauth_code(&self, code_hash: &str, now_ms: i64) -> Result<CodeConsume> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let row = tx
            .query_row(
                "SELECT client_id, user_id, redirect_uri, code_challenge, scope, used_at, expires_at
                 FROM oauth_codes WHERE code_hash = ?1",
                [code_hash],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        UserId::from_db(r.get::<_, i64>(1)?),
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                        r.get::<_, Option<i64>>(5)?,
                        r.get::<_, i64>(6)?,
                    ))
                },
            )
            .optional()?;
        let Some((client_id, user_id, redirect_uri, code_challenge, scope, used_at, expires_at)) =
            row
        else {
            return Ok(CodeConsume::Invalid);
        };
        if used_at.is_some() {
            tx.commit()?;
            return Ok(CodeConsume::Replayed { client_id, user_id });
        }
        if expires_at <= now_ms {
            return Ok(CodeConsume::Invalid);
        }
        tx.execute(
            "UPDATE oauth_codes SET used_at = ?1 WHERE code_hash = ?2",
            params![now_ms, code_hash],
        )?;
        tx.commit()?;
        Ok(CodeConsume::Fresh(CodeGrant {
            client_id,
            user_id,
            redirect_uri,
            code_challenge,
            scope,
        }))
    }

    /// `label` names the device for first-party (mobile) grants; MCP
    /// clients pass "".
    #[allow(clippy::too_many_arguments)]
    pub fn insert_oauth_token(
        &self,
        access_hash: &str,
        refresh_hash: &str,
        client_id: &str,
        user: UserId,
        scope: &str,
        label: &str,
        access_expires_ms: i64,
        refresh_expires_ms: i64,
        now_ms: i64,
    ) -> Result<()> {
        self.conn().execute(
            "INSERT INTO oauth_tokens
               (access_hash, refresh_hash, client_id, user_id, scope, label,
                access_expires_at, refresh_expires_at, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                access_hash,
                refresh_hash,
                client_id,
                user.raw(),
                scope,
                label,
                access_expires_ms,
                refresh_expires_ms,
                now_ms
            ],
        )?;
        Ok(())
    }

    /// Valid access token -> user.
    pub fn lookup_access_token(&self, access_hash: &str, now_ms: i64) -> Result<Option<UserId>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT user_id FROM oauth_tokens
                 WHERE access_hash = ?1 AND revoked_at IS NULL AND access_expires_at > ?2",
                params![access_hash, now_ms],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
            .map(UserId::from_db))
    }

    /// Valid access token -> who holds it and which client minted it.
    pub fn lookup_api_token(&self, access_hash: &str, now_ms: i64) -> Result<Option<ApiTokenInfo>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT t.id, t.user_id, u.role, t.client_id, t.scope,
                        COALESCE(c.client_name, '')
                 FROM oauth_tokens t JOIN users u ON u.id = t.user_id
                 LEFT JOIN oauth_clients c ON c.client_id = t.client_id
                 WHERE t.access_hash = ?1 AND t.revoked_at IS NULL AND t.access_expires_at > ?2",
                params![access_hash, now_ms],
                |r| {
                    Ok(ApiTokenInfo {
                        id: r.get(0)?,
                        user: UserId::from_db(r.get(1)?),
                        role: r.get(2)?,
                        client_id: r.get(3)?,
                        scope: r.get(4)?,
                        client_name: r.get(5)?,
                    })
                },
            )
            .optional()?)
    }

    /// The user's live grants for one client, newest first.
    pub fn list_tokens_for_user(
        &self,
        user: UserId,
        client_id: &str,
        now_ms: i64,
    ) -> Result<Vec<TokenRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, label, created_at, last_used_at FROM oauth_tokens
             WHERE user_id = ?1 AND client_id = ?2 AND revoked_at IS NULL
               AND refresh_expires_at > ?3
             ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map(params![user.raw(), client_id, now_ms], |r| {
            Ok(TokenRow {
                id: r.get(0)?,
                label: r.get(1)?,
                created_at: r.get(2)?,
                last_used_at: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The clients holding live grants for the user, one row per client,
    /// most recently used first. A grant is live while its refresh half
    /// has not expired (or, for a token without one, its access half).
    pub fn list_grants_for_user(&self, user: UserId, now_ms: i64) -> Result<Vec<GrantRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT t.client_id, COALESCE(c.client_name, ''), COUNT(*),
                    MIN(t.created_at), MAX(t.last_used_at)
             FROM oauth_tokens t
             LEFT JOIN oauth_clients c ON c.client_id = t.client_id
             WHERE t.user_id = ?1 AND t.revoked_at IS NULL
               AND COALESCE(t.refresh_expires_at, t.access_expires_at) > ?2
             GROUP BY t.client_id
             ORDER BY MAX(COALESCE(t.last_used_at, t.created_at)) DESC",
        )?;
        let rows = stmt.query_map(params![user.raw(), now_ms], |r| {
            Ok(GrantRow {
                client_id: r.get(0)?,
                client_name: r.get(1)?,
                tokens: r.get::<_, i64>(2)?.max(0) as u32,
                first_granted_at: r.get(3)?,
                last_used_at: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Revokes one of the user's own grants (sign out this device).
    pub fn revoke_token(&self, user: UserId, id: i64, now_ms: i64) -> Result<()> {
        crate::exec_expect_row(
            &self.conn(),
            "token",
            "UPDATE oauth_tokens SET revoked_at = ?1
             WHERE id = ?2 AND user_id = ?3 AND revoked_at IS NULL",
            params![now_ms, id, user.raw()],
        )
    }

    /// Revokes every live grant of one client for the user except `keep`
    /// (the caller's own): password changes and "sign out other devices".
    pub fn revoke_tokens_for_user(
        &self,
        user: UserId,
        client_id: &str,
        keep: Option<i64>,
        now_ms: i64,
    ) -> Result<usize> {
        Ok(self.conn().execute(
            "UPDATE oauth_tokens SET revoked_at = ?1
             WHERE user_id = ?2 AND client_id = ?3 AND revoked_at IS NULL
               AND (?4 IS NULL OR id != ?4)",
            params![now_ms, user.raw(), client_id, keep],
        )?)
    }

    /// Drops rows that have been revoked or refresh-expired for longer
    /// than the grace window. Housekeeping for the scheduler.
    pub fn purge_expired_tokens(&self, now_ms: i64) -> Result<usize> {
        let cutoff = now_ms - TOKEN_PURGE_GRACE_MS;
        Ok(self.conn().execute(
            "DELETE FROM oauth_tokens
             WHERE (revoked_at IS NOT NULL AND revoked_at < ?1)
                OR refresh_expires_at < ?1",
            [cutoff],
        )?)
    }

    /// Rotates a refresh token in place; returns the grant if valid.
    pub fn rotate_refresh_token(
        &self,
        old_refresh_hash: &str,
        new_access_hash: &str,
        new_refresh_hash: &str,
        access_expires_ms: i64,
        refresh_expires_ms: i64,
        now_ms: i64,
    ) -> Result<Option<RefreshGrant>> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let row = tx
            .query_row(
                "SELECT client_id, user_id, scope FROM oauth_tokens
                 WHERE refresh_hash = ?1 AND revoked_at IS NULL AND refresh_expires_at > ?2",
                params![old_refresh_hash, now_ms],
                |r| {
                    Ok(RefreshGrant {
                        client_id: r.get(0)?,
                        user_id: UserId::from_db(r.get(1)?),
                        scope: r.get(2)?,
                    })
                },
            )
            .optional()?;
        let Some(grant) = row else {
            return Ok(None);
        };
        // Refresh is the one per-device write worth recording as "last
        // used" — every request would be too chatty for a single
        // SQLite connection.
        tx.execute(
            "UPDATE oauth_tokens
             SET access_hash = ?1, refresh_hash = ?2,
                 access_expires_at = ?3, refresh_expires_at = ?4, last_used_at = ?6
             WHERE refresh_hash = ?5",
            params![
                new_access_hash,
                new_refresh_hash,
                access_expires_ms,
                refresh_expires_ms,
                old_refresh_hash,
                now_ms
            ],
        )?;
        tx.commit()?;
        Ok(Some(grant))
    }

    /// Revokes every token for a (client, user) grant — used on code replay.
    pub fn revoke_grant(&self, client_id: &str, user: UserId, now_ms: i64) -> Result<usize> {
        Ok(self.conn().execute(
            "UPDATE oauth_tokens SET revoked_at = ?1
             WHERE client_id = ?2 AND user_id = ?3 AND revoked_at IS NULL",
            params![now_ms, client_id, user.raw()],
        )?)
    }
}
