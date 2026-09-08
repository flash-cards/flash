//! Session tokens and the authenticated-user extractor.
//!
//! Tokens are 256-bit randoms; only their SHA-256 hex ever reaches the
//! database, so a DB leak leaks nothing usable.

use axum::extract::FromRequestParts;
use axum::http::{header, request::Parts, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum_extra::extract::cookie::CookieJar;
use base64::Engine;
use flash_core::UserId;
use sha2::{Digest, Sha256};

use crate::service::now_ms;
use crate::state::AppState;

pub const SESSION_COOKIE: &str = "__Host-flash";
pub const CEREMONY_COOKIE: &str = "__Host-flash-ceremony";
pub const SESSION_TTL_MS: i64 = 30 * 24 * 60 * 60 * 1000;
pub const CEREMONY_TTL_MS: i64 = 5 * 60 * 1000;

/// 256-bit random token, base64url.
pub fn new_token() -> String {
    let bytes: [u8; 32] = rand::random();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

pub fn hash_token(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    let mut out = String::with_capacity(64);
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// base64url(SHA256(verifier)): the PKCE S256 code challenge (RFC 7636),
/// used both when Flash is the client (Google) and the server (MCP OAuth).
pub fn pkce_s256(verifier: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// `Set-Cookie` value for a host-locked, HttpOnly session cookie.
/// SameSite=Lax: the OAuth authorize redirect arrives as a top-level
/// cross-site GET and must still see the session.
pub fn cookie_header(name: &str, token: &str, max_age_secs: i64) -> String {
    format!("{name}={token}; Path=/; Secure; HttpOnly; SameSite=Lax; Max-Age={max_age_secs}")
}

pub fn clear_cookie_header(name: &str) -> String {
    format!("{name}=; Path=/; Secure; HttpOnly; SameSite=Lax; Max-Age=0")
}

pub const THEME_COOKIE: &str = "flash-theme";

/// UI theme, read from a cookie and rendered as `data-theme` on <html>.
/// Server-side so pages never flash the wrong palette.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Theme {
    Dark,
    Light,
    Clay,
    Mocha,
    Taupe,
}

impl Theme {
    pub fn as_str(self) -> &'static str {
        match self {
            Theme::Dark => "dark",
            Theme::Light => "light",
            Theme::Clay => "clay",
            Theme::Mocha => "mocha",
            Theme::Taupe => "taupe",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Theme> {
        match s {
            "dark" => Some(Theme::Dark),
            "light" => Some(Theme::Light),
            "clay" => Some(Theme::Clay),
            "mocha" => Some(Theme::Mocha),
            "taupe" => Some(Theme::Taupe),
            _ => None,
        }
    }
}

impl<S: Send + Sync> FromRequestParts<S> for Theme {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let jar = CookieJar::from_headers(&parts.headers);
        Ok(jar
            .get(THEME_COOKIE)
            .and_then(|c| Theme::from_str(c.value()))
            .unwrap_or(Theme::Dark))
    }
}

/// Extractor: the logged-in user, or a redirect to /login for pages.
pub struct AuthUser {
    pub id: UserId,
    pub is_admin: bool,
}

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        match session_user(parts, state).await {
            Some((id, is_admin)) => Ok(AuthUser { id, is_admin }),
            None => Err(login_redirect(parts)),
        }
    }
}

/// Extractor variant for JSON/htmx endpoints: 401 instead of redirect.
pub struct ApiUser(pub UserId);

impl FromRequestParts<AppState> for ApiUser {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        match session_user(parts, state).await {
            Some((user, _)) => Ok(ApiUser(user)),
            None => Err((StatusCode::UNAUTHORIZED, "not signed in").into_response()),
        }
    }
}

/// Extractor for pages that render either way: the session if there is
/// one, None otherwise. Never rejects.
pub struct OptionalUser(pub Option<(UserId, bool)>);

impl FromRequestParts<AppState> for OptionalUser {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        Ok(OptionalUser(session_user(parts, state).await))
    }
}

/// Extractor for admin-only surfaces. Signed-out visitors are redirected
/// to /login like any page; signed-in members get a plain 404 so the
/// surface's existence isn't advertised.
pub struct AdminUser(pub UserId);

impl FromRequestParts<AppState> for AdminUser {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        match session_user(parts, state).await {
            Some((user, true)) => Ok(AdminUser(user)),
            Some((_, false)) => Err((StatusCode::NOT_FOUND, "not found").into_response()),
            None => Err(login_redirect(parts)),
        }
    }
}

pub(crate) fn login_redirect(parts: &Parts) -> Response {
    let next = parts
        .uri
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/");
    let to = format!("/login?next={}", urlencode(next));
    Redirect::to(&to).into_response()
}

/// The signed-in browser user behind the session cookie, if any. Shared
/// with the API's cookie-or-bearer extractor.
pub(crate) async fn session_user(parts: &Parts, state: &AppState) -> Option<(UserId, bool)> {
    let jar = CookieJar::from_headers(&parts.headers);
    let token = jar.get(SESSION_COOKIE)?.value().to_string();
    let hash = hash_token(&token);
    let services = state.services.clone();
    tokio::task::spawn_blocking(move || {
        services
            .store()
            .lookup_web_session(&hash, now_ms())
            .ok()
            .flatten()
    })
    .await
    .ok()
    .flatten()
    .map(|(user, role)| (user, role == "admin"))
}

pub fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Mints a web session for `user` inside a store call and returns the
/// cookie token (the store only ever sees its hash). Pair with
/// [`with_session_cookie`] on the response. Every login path — password,
/// passkey, reset, enrollment, Google — ends here.
pub fn issue_session(
    store: &flash_store::Store,
    user: UserId,
    now_ms: i64,
) -> flash_store::Result<String> {
    let token = new_token();
    store.create_web_session(&hash_token(&token), user, now_ms, now_ms + SESSION_TTL_MS)?;
    Ok(token)
}

/// Helper: respond with a session cookie set.
pub fn with_session_cookie(token: &str, response: Response) -> Response {
    let mut response = response;
    if let Ok(value) = cookie_header(SESSION_COOKIE, token, SESSION_TTL_MS / 1000).parse() {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
    response
}

/// Cookie *or* bearer, for the two web media routes both surfaces share
/// (upload and serving). A request that presented a bearer gets the JSON
/// 401; a plain browser request keeps the login redirect it always had.
/// It lives here with the cookie extractors, never under `api`, whose
/// handlers must accept bearers only.
#[derive(Debug, Clone, Copy)]
pub struct AnyUser {
    pub id: UserId,
    pub is_admin: bool,
}

impl FromRequestParts<AppState> for AnyUser {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        if crate::api::bearer_token(parts).is_some() {
            return match crate::api::mobile_user(parts, state).await {
                Some(u) => Ok(AnyUser {
                    id: u.id,
                    is_admin: u.is_admin,
                }),
                None => Err(crate::api::ApiError::unauthorized().into_response()),
            };
        }
        match session_user(parts, state).await {
            Some((id, is_admin)) => Ok(AnyUser { id, is_admin }),
            None => Err(login_redirect(parts)),
        }
    }
}
