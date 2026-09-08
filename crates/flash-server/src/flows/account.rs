//! Account security shared by the settings page and the API: passwords,
//! login-method removal, and self-serve deletion. The rules live here
//! once — never leave an account without a way in, never delete the
//! last admin — and the extension's `AccountHooks` add theirs.

use flash_core::UserId;
use flash_store::{DeletedAccount, Store};

use crate::ext::AccountHooks;
use crate::service::{Result, ServiceError};
use crate::state::AppState;

/// Which grants survive a password change: the one making it.
#[derive(Debug, Clone, Default)]
pub struct KeepSession {
    /// SHA-256 of the web session cookie, when the change came from the web.
    pub web_session_hash: Option<String>,
    /// The API grant id, when it came from the app.
    pub api_token_id: Option<i64>,
}

/// Stores a first password; refused when one already exists (that's a
/// change, which must prove the current one).
pub fn set_password(store: &Store, user: UserId, phc: &str) -> Result<()> {
    if store.get_password_hash(user)?.is_some() {
        return Err(ServiceError::Invalid(
            "a password is already set; change it instead".into(),
        ));
    }
    store.set_password_hash(user, Some(phc))?;
    store.delete_unused_password_resets(user)?;
    Ok(())
}

/// Replaces the password and signs out every other session and device;
/// the caller has already verified the current password.
pub fn change_password(
    store: &Store,
    user: UserId,
    phc: &str,
    keep: &KeepSession,
    now_ms: i64,
) -> Result<()> {
    store.set_password_hash(user, Some(phc))?;
    store.clear_password_failures(user)?;
    // A reset link mailed before the change is a way back in after it.
    store.delete_unused_password_resets(user)?;
    store.delete_sessions_for_user(user, keep.web_session_hash.as_deref())?;
    // Every grant, not just the app's: a connector authorized from a
    // hijacked session is exactly what a password change is meant to end.
    store.revoke_all_tokens_for_user(user, keep.api_token_id, now_ms)?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoveOutcome {
    Removed,
    /// Refused: it was the only way into the account.
    LastMethod,
}

/// Drops the password when a passkey or an external login can still
/// sign the user in; the caller has already verified it.
pub fn remove_password(
    store: &Store,
    hooks: &dyn AccountHooks,
    user: UserId,
) -> Result<RemoveOutcome> {
    let has_other =
        !store.passkeys_for_user(user)?.is_empty() || hooks.has_external_login(store, user)?;
    if !has_other {
        return Ok(RemoveOutcome::LastMethod);
    }
    store.set_password_hash(user, None)?;
    store.delete_unused_password_resets(user)?;
    Ok(RemoveOutcome::Removed)
}

/// Deletes one passkey unless it is the last login method. `Ok(None)`
/// when no such passkey belongs to the user.
pub fn remove_passkey(
    store: &Store,
    hooks: &dyn AccountHooks,
    user: UserId,
    id: i64,
) -> Result<Option<RemoveOutcome>> {
    let mine = store.passkeys_for_user(user)?;
    if !mine.iter().any(|p| p.id == id) {
        return Ok(None);
    }
    let has_other = mine.len() > 1
        || store.get_password_hash(user)?.is_some()
        || hooks.has_external_login(store, user)?;
    if !has_other {
        return Ok(Some(RemoveOutcome::LastMethod));
    }
    store.delete_passkey(user, id)?;
    Ok(Some(RemoveOutcome::Removed))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteRefusal {
    /// The only admin can't delete themselves.
    LastAdmin,
    /// The extension refused and nothing was deleted. `code` is the web's
    /// `?delerr=` value and the API's error code; `message` explains it.
    Ext {
        code: &'static str,
        message: &'static str,
    },
}

impl DeleteRefusal {
    /// The web's `?delerr=` value.
    pub fn code(self) -> &'static str {
        match self {
            DeleteRefusal::LastAdmin => "admin",
            DeleteRefusal::Ext { code, .. } => code,
        }
    }
}

/// Self-serve account deletion, after the caller has verified the
/// current password: refuses the last admin, lets the extension refuse
/// or clean up (subscriptions, provider grants), then purges. Returns
/// the orphaned media the caller removes from the blob store.
pub fn delete_account(
    state: &AppState,
    user: UserId,
    is_admin: bool,
    now_ms: i64,
) -> Result<std::result::Result<DeletedAccount, DeleteRefusal>> {
    let store = state.services.store();
    if is_admin && store.admin_count()? <= 1 {
        return Ok(Err(DeleteRefusal::LastAdmin));
    }
    if let Err(refusal) = state.ext.before_delete(state, user, now_ms)? {
        return Ok(Err(refusal));
    }
    let deleted = store.delete_user_account(user)?;
    tracing::info!(
        "account {} deleted ({} orphan blobs)",
        user.raw(),
        deleted.orphan_blobs.len()
    );
    Ok(Ok(deleted))
}
