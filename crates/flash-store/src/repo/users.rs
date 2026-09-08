//! Users and their settings.

use flash_core::{GradingMode, UserId, UserSettings};
use rusqlite::{params, OptionalExtension};

use crate::{exec_expect_row, Result, Store, StoreError};

/// Lockout window for repeated password failures (see
/// record_password_failure and the login handler's policy).
pub const PW_LOCKOUT_WINDOW_MS: i64 = 15 * 60 * 1000;
pub const PW_LOCKOUT_THRESHOLD: u32 = 5;

/// The account as a client sees itself: profile plus theme.
#[derive(Debug, Clone, PartialEq)]
pub struct AccountRow {
    pub display_name: String,
    pub email: Option<String>,
    pub role: String,
    pub theme: String,
}

/// What the password login handler needs about an account.
#[derive(Debug, Clone)]
pub struct PasswordLogin {
    pub user: UserId,
    pub password_hash: Option<String>,
    pub pw_failed_count: u32,
    pub pw_failed_at: Option<i64>,
}

/// One row of the admin users overview: identity plus activity signals.
#[derive(Debug, Clone)]
pub struct AdminUserRow {
    pub id: UserId,
    pub display_name: String,
    pub email: Option<String>,
    pub role: String,
    pub created_at: i64,
    pub deck_count: u32,
    pub card_count: u32,
    pub review_count: u32,
    pub last_review_at: Option<i64>,
    pub last_review_source: Option<String>,
    /// Most recent sign-in: passkey use or session creation, whichever is
    /// newer. Sessions cover enrollment and logins where the authenticator
    /// reported no state change.
    pub last_login_at: Option<i64>,
    /// client_names with an unrevoked, unexpired OAuth grant — a user can
    /// have several MCP clients attached at once (Claude, ChatGPT, ...).
    pub mcp_clients: Vec<String>,
}

impl Store {
    /// Creates a user with default settings; returns the new id.
    pub fn create_user(
        &self,
        display_name: &str,
        email: Option<&str>,
        role: &str,
        now_ms: i64,
    ) -> Result<UserId> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO users (display_name, email, role, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![display_name, email, role, now_ms],
        )?;
        let id = conn.last_insert_rowid();
        // The application default is the source of truth for a new account's
        // settings; the column defaults in the schema are history.
        let defaults = UserSettings::default();
        conn.execute(
            "INSERT INTO user_settings (user_id, timezone) VALUES (?1, ?2)",
            params![id, defaults.timezone],
        )?;
        Ok(UserId::from_db(id))
    }

    pub fn get_settings(&self, user: UserId) -> Result<UserSettings> {
        let conn = self.conn();
        conn.query_row(
            "SELECT grading_mode, desired_retention, new_per_day, day_cutoff_hour, timezone,
                    fsrs_params, reviews_per_day, boost_new, boost_day
             FROM user_settings WHERE user_id = ?1",
            [user.raw()],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, f64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, Option<String>>(5)?,
                    r.get::<_, i64>(6)?,
                    r.get::<_, i64>(7)?,
                    r.get::<_, i64>(8)?,
                ))
            },
        )
        .optional()?
        .ok_or(StoreError::NotFound("user_settings"))
        .and_then(
            |(mode, retention, new_per_day, cutoff, tz, params_json, reviews, boost, boost_day)| {
                let fsrs_params =
                    match params_json {
                        None => None,
                        Some(json) => Some(parse_params(&json).ok_or_else(|| {
                            StoreError::Invalid(format!("bad fsrs_params: {json}"))
                        })?),
                    };
                Ok(UserSettings {
                    grading_mode: GradingMode::from_str(&mode)
                        .ok_or_else(|| StoreError::Invalid(format!("bad grading_mode: {mode}")))?,
                    desired_retention: retention as f32,
                    new_per_day: new_per_day as u32,
                    reviews_per_day: reviews as u32,
                    boost_new: boost as u32,
                    boost_day,
                    day_cutoff_hour: cutoff as u8,
                    timezone: tz,
                    fsrs_params,
                })
            },
        )
    }

    /// Account-wide daily limits (per-deck overrides live on decks).
    pub fn set_daily_limits(
        &self,
        user: UserId,
        new_per_day: u32,
        reviews_per_day: u32,
    ) -> Result<()> {
        self.conn().execute(
            "UPDATE user_settings SET new_per_day = ?1, reviews_per_day = ?2 WHERE user_id = ?3",
            params![new_per_day, reviews_per_day, user.raw()],
        )?;
        Ok(())
    }

    /// Sets the account's "more new cards today" boost for the study day
    /// starting at `day_ms` (replaces any earlier value).
    pub fn set_account_boost(&self, user: UserId, extra: u32, day_ms: i64) -> Result<()> {
        self.conn().execute(
            "UPDATE user_settings SET boost_new = ?1, boost_day = ?2 WHERE user_id = ?3",
            params![extra, day_ms, user.raw()],
        )?;
        Ok(())
    }

    /// Login-relevant password state for an email, if any user owns it.
    pub fn user_by_email(&self, email: &str) -> Result<Option<PasswordLogin>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT id, password_hash, pw_failed_count, pw_failed_at
                 FROM users WHERE email = ?1",
                [email],
                |r| {
                    Ok(PasswordLogin {
                        user: UserId::from_db(r.get(0)?),
                        password_hash: r.get(1)?,
                        pw_failed_count: r.get::<_, i64>(2)? as u32,
                        pw_failed_at: r.get(3)?,
                    })
                },
            )
            .optional()?)
    }

    /// Sets or clears (None) the password verifier.
    pub fn set_password_hash(&self, user: UserId, phc: Option<&str>) -> Result<()> {
        exec_expect_row(
            &self.conn(),
            "user",
            "UPDATE users SET password_hash = ?1 WHERE id = ?2",
            params![phc, user.raw()],
        )
    }

    pub fn get_password_hash(&self, user: UserId) -> Result<Option<String>> {
        self.conn()
            .query_row(
                "SELECT password_hash FROM users WHERE id = ?1",
                [user.raw()],
                |r| r.get(0),
            )
            .optional()?
            .ok_or(StoreError::NotFound("user"))
    }

    pub fn record_password_failure(&self, user: UserId, now_ms: i64) -> Result<()> {
        // Failures outside the lockout window restart the count at 1.
        self.conn().execute(
            "UPDATE users SET
               pw_failed_count = CASE
                 WHEN pw_failed_at IS NULL OR ?1 - pw_failed_at > ?2 THEN 1
                 ELSE pw_failed_count + 1 END,
               pw_failed_at = ?1
             WHERE id = ?3",
            params![now_ms, PW_LOCKOUT_WINDOW_MS, user.raw()],
        )?;
        Ok(())
    }

    pub fn clear_password_failures(&self, user: UserId) -> Result<()> {
        self.conn().execute(
            "UPDATE users SET pw_failed_count = 0, pw_failed_at = NULL WHERE id = ?1",
            [user.raw()],
        )?;
        Ok(())
    }

    /// Whether any user already claimed this (normalized) email.
    pub fn email_taken(&self, email: &str) -> Result<bool> {
        let taken: bool = self.conn().query_row(
            "SELECT EXISTS(SELECT 1 FROM users WHERE email = ?1)",
            [email],
            |r| r.get(0),
        )?;
        Ok(taken)
    }

    pub fn set_display_name(&self, user: UserId, display_name: &str) -> Result<()> {
        exec_expect_row(
            &self.conn(),
            "user",
            "UPDATE users SET display_name = ?1 WHERE id = ?2",
            params![display_name, user.raw()],
        )
    }

    /// (display_name, role)
    pub fn get_user_info(&self, user: UserId) -> Result<(String, String)> {
        self.conn()
            .query_row(
                "SELECT display_name, role FROM users WHERE id = ?1",
                [user.raw()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .ok_or(StoreError::NotFound("user"))
    }

    /// All users with per-user activity aggregates, oldest first.
    /// Admin-only surface; the users table is single-digit rows by design,
    /// so correlated subqueries are fine.
    pub fn list_users_admin(&self, now_ms: i64) -> Result<Vec<AdminUserRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT u.id, u.display_name, u.email, u.role, u.created_at,
               (SELECT COUNT(*) FROM decks d WHERE d.user_id = u.id),
               (SELECT COUNT(*) FROM cards c
                 WHERE c.user_id = u.id AND c.deleted_at IS NULL),
               (SELECT COUNT(*) FROM review_log r WHERE r.user_id = u.id),
               (SELECT MAX(reviewed_at) FROM review_log r WHERE r.user_id = u.id),
               (SELECT source FROM review_log r WHERE r.user_id = u.id
                 ORDER BY reviewed_at DESC LIMIT 1),
               (SELECT MAX(ts) FROM (
                  SELECT MAX(last_used_at) AS ts FROM webauthn_credentials w
                    WHERE w.user_id = u.id
                  UNION ALL
                  SELECT MAX(created_at) FROM sessions s WHERE s.user_id = u.id)),
               (SELECT GROUP_CONCAT(client_name, ', ') FROM (
                  SELECT DISTINCT oc.client_name FROM oauth_tokens t
                    JOIN oauth_clients oc ON oc.client_id = t.client_id
                    WHERE t.user_id = u.id AND t.revoked_at IS NULL
                      AND (t.access_expires_at > ?1
                           OR (t.refresh_expires_at IS NOT NULL AND t.refresh_expires_at > ?1))
                    ORDER BY oc.client_name))
             FROM users u
             ORDER BY u.created_at, u.id",
        )?;
        let rows = stmt.query_map([now_ms], |r| {
            Ok(AdminUserRow {
                id: UserId::from_db(r.get(0)?),
                display_name: r.get(1)?,
                email: r.get(2)?,
                role: r.get(3)?,
                created_at: r.get(4)?,
                deck_count: r.get::<_, i64>(5)? as u32,
                card_count: r.get::<_, i64>(6)? as u32,
                review_count: r.get::<_, i64>(7)? as u32,
                last_review_at: r.get(8)?,
                last_review_source: r.get(9)?,
                last_login_at: r.get(10)?,
                mcp_clients: r
                    .get::<_, Option<String>>(11)?
                    .map(|s| s.split(", ").map(str::to_string).collect())
                    .unwrap_or_default(),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Applies imported Anki study settings (each field only when
    /// present). Caller validates the FSRS params build a scheduler first.
    pub fn adopt_study_settings(
        &self,
        user: UserId,
        desired_retention: Option<f32>,
        fsrs_params: Option<&[f32]>,
        new_per_day: Option<u32>,
    ) -> Result<()> {
        let conn = self.conn();
        if let Some(r) = desired_retention {
            conn.execute(
                "UPDATE user_settings SET desired_retention = ?1 WHERE user_id = ?2",
                params![r as f64, user.raw()],
            )?;
        }
        if let Some(p) = fsrs_params {
            let json = format!(
                "[{}]",
                p.iter().map(f32::to_string).collect::<Vec<_>>().join(", ")
            );
            conn.execute(
                "UPDATE user_settings SET fsrs_params = ?1 WHERE user_id = ?2",
                params![json, user.raw()],
            )?;
        }
        if let Some(n) = new_per_day {
            // Anki presets can carry absurd values; keep the settable range.
            let n = n.min(flash_core::settings::MAX_NEW_PER_DAY);
            conn.execute(
                "UPDATE user_settings SET new_per_day = ?1 WHERE user_id = ?2",
                params![n, user.raw()],
            )?;
        }
        Ok(())
    }

    /// Whether the user dismissed the Today page's "Connect your AI" card.
    pub fn connect_cta_hidden(&self, user: UserId) -> Result<bool> {
        let hidden: i64 = self
            .conn()
            .query_row(
                "SELECT hide_connect_cta FROM user_settings WHERE user_id = ?1",
                [user.raw()],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0);
        Ok(hidden != 0)
    }

    pub fn set_connect_cta_hidden(&self, user: UserId, hidden: bool) -> Result<()> {
        self.conn().execute(
            "UPDATE user_settings SET hide_connect_cta = ?1 WHERE user_id = ?2",
            params![hidden as i64, user.raw()],
        )?;
        Ok(())
    }

    pub fn set_grading_mode(&self, user: UserId, mode: GradingMode) -> Result<()> {
        self.conn().execute(
            "UPDATE user_settings SET grading_mode = ?1 WHERE user_id = ?2",
            params![mode.as_str(), user.raw()],
        )?;
        Ok(())
    }

    pub fn account_row(&self, user: UserId) -> Result<AccountRow> {
        self.conn()
            .query_row(
                "SELECT u.display_name, u.email, u.role, s.theme
                 FROM users u JOIN user_settings s ON s.user_id = u.id
                 WHERE s.user_id = ?1",
                [user.raw()],
                |r| {
                    Ok(AccountRow {
                        display_name: r.get(0)?,
                        email: r.get(1)?,
                        role: r.get(2)?,
                        theme: r.get(3)?,
                    })
                },
            )
            .optional()?
            .ok_or(StoreError::NotFound("user"))
    }

    /// The UI theme (validated by the caller against the known set).
    pub fn set_theme(&self, user: UserId, theme: &str) -> Result<()> {
        exec_expect_row(
            &self.conn(),
            "user_settings",
            "UPDATE user_settings SET theme = ?1 WHERE user_id = ?2",
            params![theme, user.raw()],
        )
    }

    /// Timezone, day cutoff, and desired retention; `None` leaves a
    /// field as it is. Validation is the caller's job.
    pub fn set_study_settings(
        &self,
        user: UserId,
        timezone: Option<&str>,
        day_cutoff_hour: Option<u8>,
        desired_retention: Option<f32>,
    ) -> Result<()> {
        exec_expect_row(
            &self.conn(),
            "user_settings",
            "UPDATE user_settings SET
               timezone = COALESCE(?1, timezone),
               day_cutoff_hour = COALESCE(?2, day_cutoff_hour),
               desired_retention = COALESCE(?3, desired_retention)
             WHERE user_id = ?4",
            params![
                timezone,
                day_cutoff_hour.map(|h| h as i64),
                desired_retention.map(|r| r as f64),
                user.raw()
            ],
        )
    }

    /// Addresses of admins that have one, for whatever an extension needs
    /// to tell them.
    pub fn admin_emails(&self) -> Result<Vec<String>> {
        let conn = self.conn();
        let mut stmt =
            conn.prepare("SELECT email FROM users WHERE role = 'admin' AND email IS NOT NULL")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Number of admin accounts; the last one can't delete itself.
    pub fn admin_count(&self) -> Result<u32> {
        let n: i64 =
            self.conn()
                .query_row("SELECT COUNT(*) FROM users WHERE role = 'admin'", [], |r| {
                    r.get(0)
                })?;
        Ok(n as u32)
    }

    /// Removes an account and everything it owns in one transaction:
    /// cards (and their state/tags/media links), decks, tags, media rows,
    /// study sessions, web sessions, password resets, OAuth codes and
    /// tokens, passkeys, settings, the user row, and finally its review
    /// log (permitted by the V008 trigger once the user row is gone).
    /// Invites the user issued survive with `created_by` nulled; an
    /// extension's own audit rows are its `before_delete_user` hook's call.
    ///
    /// Foreign keys are ON with no cascades, so the order below matters.
    /// Media blobs are content-addressed and shared across users: the
    /// caller removes only the hashes returned in `orphan_blobs`.
    pub fn delete_user_account(&self, user: UserId) -> Result<DeletedAccount> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM users WHERE id = ?1)",
            [user.raw()],
            |r| r.get(0),
        )?;
        if !exists {
            return Err(StoreError::NotFound("user"));
        }
        let hashes: Vec<String> = {
            let mut stmt = tx.prepare("SELECT DISTINCT sha256 FROM media WHERE user_id = ?1")?;
            let rows = stmt.query_map([user.raw()], |r| r.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        for ext in &self.extensions {
            ext.before_delete_user(&tx, user)?;
        }

        let uid = user.raw();
        // Card children, then cards.
        for sql in [
            "DELETE FROM card_media WHERE card_id IN (SELECT id FROM cards WHERE user_id = ?1)",
            "DELETE FROM card_tags WHERE card_id IN (SELECT id FROM cards WHERE user_id = ?1)",
            "DELETE FROM card_state WHERE card_id IN (SELECT id FROM cards WHERE user_id = ?1)",
            "DELETE FROM cards WHERE user_id = ?1",
            "DELETE FROM tags WHERE user_id = ?1",
            "DELETE FROM decks WHERE user_id = ?1",
            "DELETE FROM media WHERE user_id = ?1",
            "DELETE FROM study_sessions WHERE user_id = ?1",
            "DELETE FROM sessions WHERE user_id = ?1",
            "DELETE FROM password_resets WHERE user_id = ?1",
            "DELETE FROM oauth_codes WHERE user_id = ?1",
            "DELETE FROM oauth_tokens WHERE user_id = ?1",
            "DELETE FROM webauthn_credentials WHERE user_id = ?1",
            "DELETE FROM user_settings WHERE user_id = ?1",
            "UPDATE invites SET created_by = NULL WHERE created_by = ?1",
            "DELETE FROM users WHERE id = ?1",
            // Last: the trigger only permits this once the user row is gone.
            "DELETE FROM review_log WHERE user_id = ?1",
        ] {
            tx.execute(sql, [uid])?;
        }

        let mut orphan_blobs = Vec::new();
        for sha in hashes {
            if self.blob_refs(&tx, &sha)? == 0 {
                orphan_blobs.push(sha);
            }
        }
        tx.commit()?;
        Ok(DeletedAccount { orphan_blobs })
    }
}

/// What the server still has to clean up after a purge.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeletedAccount {
    /// Media hashes no remaining user references; safe to remove from disk.
    pub orphan_blobs: Vec<String>,
}

fn parse_params(json: &str) -> Option<Vec<f32>> {
    // Format: JSON array of numbers, e.g. "[0.2172, 1.1771, ...]".
    let inner = json.trim().strip_prefix('[')?.strip_suffix(']')?;
    inner
        .split(',')
        .map(|s| s.trim().parse::<f32>().ok())
        .collect()
}
