//! Password reset by mailed link. `request` is uniform (unknown address,
//! cooldown, and cap all look like a sent mail); `confirm` burns the
//! token, stores the new hash, and signs out every session and device —
//! the reset's premise is a possibly-stolen password.

use flash_core::UserId;
use flash_store::Store;

use crate::auth::{hash_token, new_token};
use crate::email::{self, Mailer};
use crate::service::Result;

pub const TTL_MS: i64 = 60 * 60 * 1000;
pub const COOLDOWN_MS: i64 = 60_000;
pub const DAILY_CAP: u32 = 5;
const DAY_MS: i64 = 24 * 60 * 60 * 1000;

/// Mails a reset link when the address has an account and isn't rate
/// limited; always `Ok(())` so callers answer uniformly.
pub fn request(
    store: &Store,
    mailer: &dyn Mailer,
    base_url: &str,
    email: &str,
    now_ms: i64,
) -> Result<()> {
    let Ok(email) = crate::webauthn::normalize_email(email) else {
        return Ok(());
    };
    let Some(login) = store.user_by_email(&email)? else {
        return Ok(());
    };
    if store.recent_password_resets(login.user, now_ms - COOLDOWN_MS)? > 0
        || store.recent_password_resets(login.user, now_ms - DAY_MS)? >= DAILY_CAP
    {
        return Ok(());
    }
    let token = new_token();
    store.create_password_reset(&hash_token(&token), login.user, now_ms, now_ms + TTL_MS)?;
    let mut mail = email::password_reset_email(base_url, &token);
    mail.to = email;
    if let Err(e) = mailer.send(&mail) {
        tracing::error!("reset mail: {e}");
        let _ = store.delete_password_reset(&hash_token(&token));
    }
    Ok(())
}

/// Consumes the reset token and installs `phc`. `Ok(None)` when the
/// token is invalid, expired, or already used.
pub fn confirm(store: &Store, token_hash: &str, phc: &str, now_ms: i64) -> Result<Option<UserId>> {
    let Some(user) = store.lookup_password_reset(token_hash, now_ms)? else {
        return Ok(None);
    };
    store.consume_password_reset(token_hash, now_ms)?;
    store.set_password_hash(user, Some(phc))?;
    store.clear_password_failures(user)?;
    // The other links this user may hold (up to the daily cap) die with
    // this one; a saved link is not a second chance at the account.
    store.delete_unused_password_resets(user)?;
    store.delete_sessions_for_user(user, None)?;
    // Every grant, including connectors (see flows::account).
    store.revoke_all_tokens_for_user(user, None, now_ms)?;
    Ok(Some(user))
}
