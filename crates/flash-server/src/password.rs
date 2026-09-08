//! Password authentication: Argon2id hashing plus the login, reset, and
//! settings handlers. Passwords are optional alongside passkeys — an
//! account always keeps at least one login method.
//!
//! Hashing is CPU/memory-heavy by design, so every hash/verify (a) runs
//! inside spawn_blocking, never on an async worker, and (b) holds a
//! 4-permit semaphore so at most four 64 MiB hashes are ever in flight
//! under the 1 GiB cgroup in flash.service.

use std::sync::OnceLock;

use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use argon2::{Algorithm, Argon2, Params, Version};
use askama::Template;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::Form;
use axum_extra::extract::CookieJar;
use serde::Deserialize;

use crate::auth::{self, hash_token, AuthUser, Theme};
use crate::flows::{account, enroll, reset};
use crate::service::now_ms;
use crate::state::AppState;

/// RFC 9106 §4's "uniformly safe" option when 2 GiB isn't available:
/// Argon2id, m=64 MiB, t=3 (above OWASP's m=19 MiB/t=2 floor). p=1 because
/// lanes only help with spare cores, which a small host doesn't have. PHC
/// strings self-describe, and logins rehash when stored params are below
/// these. About 0.13 s per hash on one modern server core (`argon2` CLI,
/// same params). Re-measure after a hardware change; target <1 s.
const M_COST_KIB: u32 = 64 * 1024;
const T_COST: u32 = 3;
const P_COST: u32 = 1;

pub const MIN_PASSWORD_CHARS: usize = 8;
pub const MAX_PASSWORD_BYTES: usize = 512;

fn argon2() -> Argon2<'static> {
    let params = Params::new(M_COST_KIB, T_COST, P_COST, None).expect("static argon2 params");
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
}

pub fn hash_password(password: &str) -> Result<String, String> {
    let salt: [u8; 16] = rand::random();
    argon2()
        .hash_password_with_salt(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| format!("hash: {e}"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerifyOutcome {
    pub ok: bool,
    /// Stored params are below the current constants; rehash on login.
    pub needs_rehash: bool,
}

pub fn verify_password(phc: &str, password: &str) -> VerifyOutcome {
    let Ok(parsed) = PasswordHash::new(phc) else {
        return VerifyOutcome {
            ok: false,
            needs_rehash: false,
        };
    };
    let ok = argon2()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok();
    let needs_rehash = ok
        && Params::try_from(&parsed)
            .map(|p| p.m_cost() < M_COST_KIB || p.t_cost() < T_COST)
            .unwrap_or(true);
    VerifyOutcome { ok, needs_rehash }
}

/// Verified against on unknown-email logins so response time doesn't
/// reveal whether the account exists (a ~1s hash is a loud oracle).
pub fn dummy_phc() -> &'static str {
    static DUMMY: OnceLock<String> = OnceLock::new();
    DUMMY.get_or_init(|| hash_password("correct horse battery staple").expect("dummy hash"))
}

pub fn validate_new_password(password: &str) -> Result<(), &'static str> {
    if password.chars().count() < MIN_PASSWORD_CHARS {
        return Err("password must be at least 8 characters");
    }
    if password.len() > MAX_PASSWORD_BYTES {
        return Err("password is too long");
    }
    Ok(())
}

// ---- async wrappers (semaphore + spawn_blocking) ----

pub async fn hash_blocking(state: &AppState, password: String) -> Result<String, Response> {
    let permit = state
        .pw_semaphore
        .clone()
        .acquire_owned()
        .await
        .map_err(|e| err500("pw semaphore", e))?;
    tokio::task::spawn_blocking(move || {
        let result = hash_password(&password);
        drop(permit);
        result
    })
    .await
    .map_err(|e| err500("join", e))?
    .map_err(|e| err500("hash", e))
}

pub async fn verify_blocking(
    state: &AppState,
    phc: String,
    password: String,
) -> Result<VerifyOutcome, Response> {
    let permit = state
        .pw_semaphore
        .clone()
        .acquire_owned()
        .await
        .map_err(|e| err500("pw semaphore", e))?;
    tokio::task::spawn_blocking(move || {
        let outcome = verify_password(&phc, &password);
        drop(permit);
        outcome
    })
    .await
    .map_err(|e| err500("join", e))
}

fn err500(context: &str, err: impl std::fmt::Display) -> Response {
    tracing::error!("{context}: {err}");
    (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
}

/// A `flows` call: `ServiceError` maps the way the web's `act` maps it.
async fn flow_call<T, F>(state: &AppState, f: F) -> Result<T, Response>
where
    T: Send + 'static,
    F: FnOnce(crate::service::Services) -> crate::service::Result<T> + Send + 'static,
{
    crate::web::act(state.services.clone(), f).await
}

async fn store_call<T, F>(state: &AppState, f: F) -> Result<T, Response>
where
    T: Send + 'static,
    F: FnOnce(&flash_store::Store) -> Result<T, flash_store::StoreError> + Send + 'static,
{
    let services = state.services.clone();
    tokio::task::spawn_blocking(move || f(services.store()))
        .await
        .map_err(|e| err500("join", e))?
        .map_err(|e| err500("store", e))
}

const UNIFORM_LOGIN_ERROR: &str = "Incorrect email or password.";

/// Shape check before any lookup: invite and reset tokens are base64url
/// from `new_token`, so anything else is rejected without a query.
pub(crate) fn token_shape_ok(token: &str) -> bool {
    token.len() <= 100
        && !token.is_empty()
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

// ---- password login ----

#[derive(Deserialize)]
pub struct PasswordLoginForm {
    #[serde(deserialize_with = "crate::bounds::email")]
    email: String,
    #[serde(deserialize_with = "crate::bounds::password")]
    password: String,
    #[serde(default, deserialize_with = "crate::bounds::next_path")]
    next: String,
}

pub async fn login_password(
    State(state): State<AppState>,
    theme: Theme,
    Form(form): Form<PasswordLoginForm>,
) -> Response {
    let next = crate::web::clean_next(Some(form.next));
    let render_error = |email: String| {
        crate::web::render_login_error(&state, theme, next.clone(), email, UNIFORM_LOGIN_ERROR)
    };
    let Ok(email) = crate::webauthn::normalize_email(&form.email) else {
        return render_error(form.email);
    };

    let login = {
        let email = email.clone();
        match store_call(&state, move |s| s.user_by_email(&email)).await {
            Ok(login) => login,
            Err(response) => return response,
        }
    };
    let now = now_ms();

    // Unknown email or no password set: burn a dummy verify so timing
    // doesn't reveal which, then answer uniformly.
    let Some(login) = login else {
        let _ = verify_blocking(&state, dummy_phc().to_string(), form.password).await;
        return render_error(email);
    };
    let Some(phc) = login.password_hash.clone() else {
        let _ = verify_blocking(&state, dummy_phc().to_string(), form.password).await;
        return render_error(email);
    };

    if crate::flows::login::is_locked(&login, now) {
        return render_error(email);
    }

    let outcome = match verify_blocking(&state, phc, form.password.clone()).await {
        Ok(outcome) => outcome,
        Err(response) => return response,
    };
    if !outcome.ok {
        let user = login.user;
        if let Err(response) =
            store_call(&state, move |s| s.record_password_failure(user, now)).await
        {
            return response;
        }
        return render_error(email);
    }

    // Success: clear failures, opportunistically rehash, mint a session.
    let rehashed = if outcome.needs_rehash {
        // On error the login still succeeds; rehash next time.
        hash_blocking(&state, form.password).await.ok()
    } else {
        None
    };
    let user = login.user;
    let result = store_call(&state, move |s| {
        crate::flows::login::finish(s, user, rehashed.as_deref())?;
        auth::issue_session(s, user, now)
    })
    .await;
    match result {
        Ok(token) => {
            crate::web::account_event(&state, user, "login", Some("password"), None);
            auth::with_session_cookie(&token, Redirect::to(&next).into_response())
        }
        Err(response) => response,
    }
}

// ---- forgot password ----

#[derive(Template)]
#[template(path = "reset_request.html")]
pub struct ResetRequestPage {
    pub theme: &'static str,
    pub sent: bool,
    pub error: Option<String>,
    /// The extension's challenge widget, if it puts one on this form.
    pub captcha_html: String,
}

pub async fn reset_request_page(State(state): State<AppState>, theme: Theme) -> Response {
    if state.mailer.is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    render_reset_request(&state, theme, false, None)
}

fn render_reset_request(
    state: &AppState,
    theme: Theme,
    sent: bool,
    error: Option<String>,
) -> Response {
    let captcha_html = match state.ext.render(state, crate::ext::Slot::Captcha) {
        Ok(html) => html,
        Err(e) => return crate::web::err_response(e),
    };
    let page = ResetRequestPage {
        theme: theme.as_str(),
        sent,
        error,
        captcha_html,
    };
    let mut response = Html(page.render().unwrap_or_default()).into_response();
    if let Some(csp) = state.ext.captcha_csp(state) {
        response
            .headers_mut()
            .insert(axum::http::header::CONTENT_SECURITY_POLICY, csp);
    }
    response
}

#[derive(Deserialize)]
pub struct ResetRequestForm {
    #[serde(deserialize_with = "crate::bounds::email")]
    email: String,
    #[serde(default, rename = "cf-turnstile-response")]
    #[serde(deserialize_with = "crate::bounds::captcha_token")]
    cf_turnstile_response: String,
}

pub async fn reset_request_submit(
    State(state): State<AppState>,
    theme: Theme,
    headers: axum::http::HeaderMap,
    Form(form): Form<ResetRequestForm>,
) -> Response {
    let Some(mailer) = state.mailer.clone() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if let Err(msg) =
        crate::captcha::require_captcha(&state, &form.cf_turnstile_response, &headers).await
    {
        return render_reset_request(&state, theme, false, Some(msg.to_string()));
    }
    // Uniform: a malformed or unknown email gets the same "check your
    // email" page.
    let base_url = state.config.base_url.clone();
    let result = flow_call(&state, move |s| {
        reset::request(s.store(), mailer.as_ref(), &base_url, &form.email, now_ms())
    })
    .await;
    match result {
        Ok(()) => {
            state
                .ext
                .count_event(&state.services, "reset_request", None);
            render_reset_request(&state, theme, true, None)
        }
        Err(response) => response,
    }
}

#[derive(Template)]
#[template(path = "reset_confirm.html")]
pub struct ResetConfirmPage {
    pub theme: &'static str,
    pub token: String,
    pub error: Option<String>,
}

pub async fn reset_confirm_page(
    State(state): State<AppState>,
    theme: Theme,
    Path(token): Path<String>,
) -> Response {
    if !token_shape_ok(&token) {
        return (StatusCode::BAD_REQUEST, "malformed reset link").into_response();
    }
    let hash = hash_token(&token);
    let now = now_ms();
    let found = store_call(&state, move |s| s.lookup_password_reset(&hash, now)).await;
    match found {
        Ok(Some(_)) => {
            let page = ResetConfirmPage {
                theme: theme.as_str(),
                token,
                error: None,
            };
            Html(page.render().unwrap_or_default()).into_response()
        }
        Ok(None) => (
            StatusCode::FORBIDDEN,
            "This reset link is invalid, expired, or already used.",
        )
            .into_response(),
        Err(response) => response,
    }
}

#[derive(Deserialize)]
pub struct ResetConfirmForm {
    #[serde(deserialize_with = "crate::bounds::password")]
    password: String,
    #[serde(deserialize_with = "crate::bounds::password")]
    confirm: String,
}

pub async fn reset_confirm_submit(
    State(state): State<AppState>,
    theme: Theme,
    Path(token): Path<String>,
    Form(form): Form<ResetConfirmForm>,
) -> Response {
    if !token_shape_ok(&token) {
        return (StatusCode::BAD_REQUEST, "malformed reset link").into_response();
    }
    let render_error = |msg: &str| {
        let page = ResetConfirmPage {
            theme: theme.as_str(),
            token: token.clone(),
            error: Some(msg.to_string()),
        };
        Html(page.render().unwrap_or_default()).into_response()
    };
    if form.password != form.confirm {
        return render_error("passwords don't match");
    }
    if let Err(msg) = validate_new_password(&form.password) {
        return render_error(msg);
    }
    // Look the token up before hashing: a stranger with a made-up link
    // must not be able to spend the Argon2 permits (the reset is not
    // consumed here, so a hiccup between the two steps burns nothing).
    let hash = hash_token(&token);
    {
        let hash = hash.clone();
        match store_call(&state, move |s| s.lookup_password_reset(&hash, now_ms())).await {
            Ok(Some(_)) => {}
            Ok(None) => {
                return render_error("this reset link is invalid, expired, or already used")
            }
            Err(response) => return response,
        }
    }
    // Then hash: the expensive fallible step happens while the token is
    // still intact.
    let phc = match hash_blocking(&state, form.password).await {
        Ok(phc) => phc,
        Err(response) => return response,
    };
    let result = flow_call(&state, move |s| {
        let now = now_ms();
        match reset::confirm(s.store(), &hash, &phc, now)? {
            Some(user) => Ok(Some(auth::issue_session(s.store(), user, now)?)),
            None => Ok(None),
        }
    })
    .await;
    match result {
        Ok(Some(token)) => auth::with_session_cookie(&token, Redirect::to("/").into_response()),
        Ok(None) => (
            StatusCode::FORBIDDEN,
            "This reset link is invalid, expired, or already used.",
        )
            .into_response(),
        Err(response) => response,
    }
}

// ---- enroll with a password (chooser's second path) ----

#[derive(Deserialize)]
pub struct EnrollPasswordForm {
    #[serde(default, deserialize_with = "crate::bounds::email")]
    email: String,
    #[serde(deserialize_with = "crate::bounds::password")]
    password: String,
    #[serde(deserialize_with = "crate::bounds::password")]
    confirm: String,
    #[serde(default, deserialize_with = "crate::bounds::next_path")]
    next: String,
}

pub async fn enroll_password(
    State(state): State<AppState>,
    theme: Theme,
    Path(token): Path<String>,
    Form(form): Form<EnrollPasswordForm>,
) -> Response {
    if !token_shape_ok(&token) {
        return (StatusCode::BAD_REQUEST, "malformed invite").into_response();
    }
    let invite_hash = hash_token(&token);
    let now = now_ms();
    let enrollment = {
        let invite_hash = invite_hash.clone();
        let hooks = state.ext.clone();
        match store_call(&state, move |s| {
            enroll::load(s, hooks.as_ref(), &invite_hash, now)
        })
        .await
        {
            Ok(Some(enrollment)) => enrollment,
            Ok(None) => {
                return (
                    StatusCode::FORBIDDEN,
                    "This invite link is invalid, expired, or already used.",
                )
                    .into_response()
            }
            Err(response) => return response,
        }
    };
    let next = crate::web::clean_next(Some(form.next));
    let render_error =
        |msg: &str| crate::web::render_enroll_error(theme, &token, &enrollment, msg, &next);
    if form.password != form.confirm {
        return render_error("passwords don't match");
    }
    if let Err(msg) = validate_new_password(&form.password) {
        return render_error(msg);
    }
    // Signup invites lock the email; admin invites take the typed one.
    if let Err(msg) = enroll::resolve_email(&enrollment, &form.email) {
        return render_error(msg);
    }

    let phc = match hash_blocking(&state, form.password).await {
        Ok(phc) => phc,
        Err(response) => return response,
    };
    let enrollment_for_flow = enrollment.clone();
    let hooks = state.ext.clone();
    let created = flow_call(&state, move |s| {
        let store = s.store();
        enroll::with_password(
            store,
            hooks.as_ref(),
            &invite_hash,
            &enrollment_for_flow,
            &form.email,
            &phc,
            now,
        )?
        .with_session(store, now)
    })
    .await;
    match created {
        Ok((enroll::EnrollOutcome::Created(_), Some(token))) => {
            auth::with_session_cookie(&token, Redirect::to(&next).into_response())
        }
        Ok((enroll::EnrollOutcome::EmailTaken, _)) => render_error("email already in use"),
        Ok((enroll::EnrollOutcome::BadEmail(msg), _)) => render_error(msg),
        Ok(_) => (
            StatusCode::FORBIDDEN,
            "This invite link is invalid, expired, or already used.",
        )
            .into_response(),
        Err(response) => response,
    }
}

// ---- settings: set / change / remove ----

#[derive(Deserialize)]
pub struct SetPasswordForm {
    #[serde(deserialize_with = "crate::bounds::password")]
    password: String,
    #[serde(deserialize_with = "crate::bounds::password")]
    confirm: String,
}

pub async fn settings_set_password(
    State(state): State<AppState>,
    AuthUser { id: user, .. }: AuthUser,
    Form(form): Form<SetPasswordForm>,
) -> Response {
    if form.password != form.confirm || validate_new_password(&form.password).is_err() {
        return Redirect::to("/settings?pwerr=1").into_response();
    }
    let phc = match hash_blocking(&state, form.password).await {
        Ok(phc) => phc,
        Err(response) => return response,
    };
    match flow_call(&state, move |s| {
        account::set_password(s.store(), user, &phc)
    })
    .await
    {
        Ok(()) => Redirect::to("/settings?pw=1").into_response(),
        // A password already exists: that's a change, which proves the current one.
        Err(response) if response.status() == StatusCode::BAD_REQUEST => {
            Redirect::to("/settings?pwerr=1").into_response()
        }
        Err(response) => response,
    }
}

#[derive(Deserialize)]
pub struct ChangePasswordForm {
    #[serde(deserialize_with = "crate::bounds::password")]
    current: String,
    #[serde(deserialize_with = "crate::bounds::password")]
    password: String,
    #[serde(deserialize_with = "crate::bounds::password")]
    confirm: String,
}

pub async fn settings_change_password(
    State(state): State<AppState>,
    AuthUser { id: user, .. }: AuthUser,
    jar: CookieJar,
    Form(form): Form<ChangePasswordForm>,
) -> Response {
    if form.password != form.confirm || validate_new_password(&form.password).is_err() {
        return Redirect::to("/settings?pwerr=1").into_response();
    }
    match require_current_password(&state, user, form.current).await {
        Ok(true) => {}
        Ok(false) => return Redirect::to("/settings?pwerr=1").into_response(),
        Err(response) => return response,
    }
    let phc = match hash_blocking(&state, form.password).await {
        Ok(phc) => phc,
        Err(response) => return response,
    };
    // Sign out every other session and device; the one making the
    // change survives.
    let keep = account::KeepSession {
        web_session_hash: jar.get(auth::SESSION_COOKIE).map(|c| hash_token(c.value())),
        api_token_id: None,
    };
    match flow_call(&state, move |s| {
        account::change_password(s.store(), user, &phc, &keep, now_ms())
    })
    .await
    {
        Ok(()) => Redirect::to("/settings?pw=1").into_response(),
        Err(response) => response,
    }
}

#[derive(Deserialize)]
pub struct RemovePasswordForm {
    #[serde(deserialize_with = "crate::bounds::password")]
    current: String,
}

pub async fn settings_remove_password(
    State(state): State<AppState>,
    AuthUser { id: user, .. }: AuthUser,
    Form(form): Form<RemovePasswordForm>,
) -> Response {
    match require_current_password(&state, user, form.current).await {
        Ok(true) => {}
        Ok(false) => return Redirect::to("/settings?pwerr=1").into_response(),
        Err(response) => return response,
    }
    // Never leave an account with zero login methods.
    let ext = state.ext.clone();
    match flow_call(&state, move |s| {
        account::remove_password(s.store(), ext.as_ref(), user)
    })
    .await
    {
        Ok(account::RemoveOutcome::Removed) => Redirect::to("/settings?pw=1").into_response(),
        Ok(account::RemoveOutcome::LastMethod) => Redirect::to("/settings?pwerr=1").into_response(),
        Err(response) => response,
    }
}

/// Verifies the account's current password, feeding the same lockout
/// counters as login.
#[derive(Deserialize)]
pub struct DeleteAccountForm {
    #[serde(deserialize_with = "crate::bounds::keyword")]
    confirm: String,
    /// Required when the account has a password; absent for passkey-only
    /// accounts (the field isn't rendered).
    #[serde(default, deserialize_with = "crate::bounds::opt_password")]
    current: Option<String>,
}

/// Self-serve account deletion: type-to-confirm plus the current password
/// (when there is one); the rest of the rules (never the last admin,
/// whatever the extension refuses or cleans up) are
/// `flows::account::delete_account`'s. Ends with the session cookie
/// cleared and a neutral notice on the login page.
pub async fn settings_delete_account(
    State(state): State<AppState>,
    AuthUser { id: user, is_admin }: AuthUser,
    Form(form): Form<DeleteAccountForm>,
) -> Response {
    let refuse = |why: &str| Redirect::to(&format!("/settings?delerr={why}")).into_response();
    if form.confirm.trim() != "DELETE" {
        return refuse("confirm");
    }
    let has_password = match store_call(&state, move |s| s.get_password_hash(user)).await {
        Ok(phc) => phc.is_some(),
        Err(response) => return response,
    };
    if has_password {
        match require_current_password(&state, user, form.current.unwrap_or_default()).await {
            Ok(true) => {}
            Ok(false) => return refuse("password"),
            Err(response) => return response,
        }
    }

    let app = state.clone();
    let outcome = flow_call(&state, move |_| {
        account::delete_account(&app, user, is_admin, now_ms())
    })
    .await;
    let deleted = match outcome {
        Ok(Ok(deleted)) => deleted,
        Ok(Err(refusal)) => return refuse(refusal.code()),
        Err(response) => return response,
    };
    crate::media::remove_orphans(
        state.services.clone(),
        state.media.clone(),
        &deleted.orphan_blobs,
    );

    let mut response = Redirect::to("/login?deleted=1").into_response();
    if let Ok(value) = auth::clear_cookie_header(auth::SESSION_COOKIE).parse() {
        response
            .headers_mut()
            .append(axum::http::header::SET_COOKIE, value);
    }
    response
}

async fn require_current_password(
    state: &AppState,
    user: flash_core::UserId,
    current: String,
) -> Result<bool, Response> {
    let phc = store_call(state, move |s| s.get_password_hash(user)).await?;
    let Some(phc) = phc else { return Ok(false) };
    let now = now_ms();
    let outcome = verify_blocking(state, phc, current).await?;
    if !outcome.ok {
        store_call(state, move |s| s.record_password_failure(user, now)).await?;
        return Ok(false);
    }
    store_call(state, move |s| s.clear_password_failures(user)).await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_wrong_password() {
        let phc = hash_password("hunter42hunter42").unwrap();
        assert!(phc.starts_with("$argon2id$"));
        let good = verify_password(&phc, "hunter42hunter42");
        assert!(good.ok);
        assert!(!good.needs_rehash);
        let bad = verify_password(&phc, "wrong-password");
        assert!(!bad.ok);
    }

    #[test]
    fn garbage_hash_never_verifies() {
        assert!(!verify_password("not-a-phc-string", "anything").ok);
    }

    #[test]
    fn below_current_params_need_rehash() {
        // Hash with weaker params than the constants.
        let weak = Argon2::new(
            Algorithm::Argon2id,
            Version::V0x13,
            Params::new(M_COST_KIB / 2, T_COST, P_COST, None).unwrap(),
        );
        let phc = weak
            .hash_password_with_salt(b"hunter42hunter42", &[7u8; 16])
            .unwrap()
            .to_string();
        let outcome = verify_password(&phc, "hunter42hunter42");
        assert!(outcome.ok);
        assert!(outcome.needs_rehash);
    }

    #[test]
    fn validation_bounds() {
        assert!(validate_new_password("seven77").is_err());
        assert!(validate_new_password("eight888").is_ok());
        assert!(validate_new_password(&"x".repeat(513)).is_err());
        assert!(validate_new_password(&"x".repeat(512)).is_ok());
    }

    #[test]
    fn dummy_phc_is_stable_and_valid() {
        assert!(dummy_phc().starts_with("$argon2id$"));
        assert!(!verify_password(dummy_phc(), "not the phrase").ok);
    }

    /// Run on the serving host after a hardware or parameter change:
    /// `cargo test -p flash-server --release timing_probe -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn timing_probe() {
        let start = std::time::Instant::now();
        let phc = hash_password("timing-probe-password").unwrap();
        let hash_time = start.elapsed();
        let start = std::time::Instant::now();
        verify_password(&phc, "timing-probe-password");
        let verify_time = start.elapsed();
        println!("hash: {hash_time:?}, verify: {verify_time:?}");
    }
}
