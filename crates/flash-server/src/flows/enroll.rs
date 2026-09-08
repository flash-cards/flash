//! Finishing an invite with a password (the passkey path lives in
//! `webauthn`). The user row is created only here, after the single-use
//! invite is burned, so a race between the two paths (or a double
//! submit) collapses to one winner.

use flash_core::UserId;
use flash_store::{InviteInfo, Store};

use crate::ext::{AccountHooks, SignupInvite};
use crate::service::Result;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnrollOutcome {
    Created(UserId),
    /// The invite was valid at lookup but is gone now (used or expired).
    InviteGone,
    EmailTaken,
    /// An admin invite needs a typed address, and this one didn't parse.
    BadEmail(&'static str),
}

impl EnrollOutcome {
    /// Pairs a `Created` outcome with a fresh web session token.
    pub fn with_session(self, store: &Store, now_ms: i64) -> Result<(Self, Option<String>)> {
        match self {
            EnrollOutcome::Created(user) => {
                let token = crate::auth::issue_session(store, user, now_ms)?;
                Ok((EnrollOutcome::Created(user), Some(token)))
            }
            other => Ok((other, None)),
        }
    }
}

/// A valid invite plus whatever the extension's signup loop attached to
/// it.
#[derive(Debug, Clone)]
pub struct Enrollment {
    pub invite: InviteInfo,
    pub signup: Option<SignupInvite>,
}

impl Enrollment {
    /// The address the account is fixed to (a signup invite), if any;
    /// shown read-only on the enroll page.
    pub fn preset_email(&self) -> Option<&str> {
        self.signup.as_ref().map(|s| s.email.as_str())
    }
}

/// The invite behind a token hash, if still valid, with its signup
/// details.
pub fn load(
    store: &Store,
    hooks: &dyn AccountHooks,
    invite_token_hash: &str,
    now_ms: i64,
) -> flash_store::Result<Option<Enrollment>> {
    let Some(invite) = store.lookup_invite(invite_token_hash, now_ms)? else {
        return Ok(None);
    };
    let signup = hooks.signup_invite(store, invite_token_hash)?;
    Ok(Some(Enrollment { invite, signup }))
}

/// The address the account gets: signup invites lock it to the mailbox
/// the verification link was mailed to; admin invites take the typed one.
pub fn resolve_email(
    enrollment: &Enrollment,
    typed: &str,
) -> std::result::Result<String, &'static str> {
    match enrollment.preset_email() {
        Some(email) => Ok(email.to_string()),
        None => crate::webauthn::normalize_email(typed),
    }
}

/// Burns the invite and creates the user with `phc` as their password.
pub fn with_password(
    store: &Store,
    hooks: &dyn AccountHooks,
    invite_token_hash: &str,
    enrollment: &Enrollment,
    typed_email: &str,
    phc: &str,
    now_ms: i64,
) -> Result<EnrollOutcome> {
    let email = match resolve_email(enrollment, typed_email) {
        Ok(email) => email,
        Err(msg) => return Ok(EnrollOutcome::BadEmail(msg)),
    };
    if store.email_taken(&email)? {
        return Ok(EnrollOutcome::EmailTaken);
    }
    if store.lookup_invite(invite_token_hash, now_ms)?.is_none() {
        return Ok(EnrollOutcome::InviteGone);
    }
    store.consume_invite(invite_token_hash, now_ms)?;
    let user = store.create_user(
        &enrollment.invite.display_name,
        Some(&email),
        &enrollment.invite.role,
        now_ms,
    )?;
    hooks.user_enrolled(
        store,
        user,
        enrollment.signup.as_ref(),
        invite_token_hash,
        now_ms,
    )?;
    store.set_password_hash(user, Some(phc))?;
    Ok(EnrollOutcome::Created(user))
}
