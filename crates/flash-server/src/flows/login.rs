//! Password login pieces shared by the web form and the API: the lockout
//! rule and the bookkeeping after a successful verify. Hashing itself
//! stays in `password` (semaphore-bound, async), which is why the two
//! handlers each drive the sequence themselves.

use flash_core::UserId;
use flash_store::{PasswordLogin, Store, PW_LOCKOUT_THRESHOLD, PW_LOCKOUT_WINDOW_MS};

/// Too many recent failures: answer exactly like a wrong password (a
/// distinct message would leak that the account exists).
pub fn is_locked(login: &PasswordLogin, now_ms: i64) -> bool {
    login.pw_failed_count >= PW_LOCKOUT_THRESHOLD
        && login
            .pw_failed_at
            .is_some_and(|at| now_ms - at < PW_LOCKOUT_WINDOW_MS)
}

/// After a correct password: clear the failure counter and store the
/// opportunistic rehash, if the caller produced one. Minting the session
/// or tokens is the caller's business.
pub fn finish(store: &Store, user: UserId, rehashed: Option<&str>) -> flash_store::Result<()> {
    store.clear_password_failures(user)?;
    if let Some(phc) = rehashed {
        store.set_password_hash(user, Some(phc))?;
    }
    Ok(())
}
