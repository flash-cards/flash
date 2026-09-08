//! SSR web UI: page handlers and htmx partials. All domain work goes
//! through Services; this module only translates HTTP <-> service calls.

use std::sync::Arc;

use askama::Template;
use axum::extract::{DefaultBodyLimit, Multipart, Path, Query, RawQuery, State};
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{delete, get, post};
use axum::{Form, Router};
use flash_core::queue::StudyScope;
use flash_core::{CardId, DeckId, Rating, SessionId, UserId};
use flash_store::notes::NoteType;
use flash_store::{CardRow, DeckSummary};
use serde::Deserialize;

use crate::auth::{self, hash_token, AdminUser, ApiUser, AuthUser, OptionalUser, Theme};
use crate::ext::{NavSection, Slot, Surface};
use crate::flows::{enroll, import_flow, Failure};
use crate::service::{
    now_ms, DayCount, DiffSpan, EditorSeed, HeatCell, MonthSpan, NoteInput, ReviewOrigin,
    ServiceError, Services, StudyCard,
};
use crate::state::{AppState, Site};
use crate::webauthn;

/// Signup verification links: single-use, email-bound invites.
/// Request-body ceiling, sized to what a typical edge proxy allows; larger
/// decks need the chunked upload that is still on the roadmap.
const IMPORT_MAX_BYTES: usize = import_flow::MAX_BYTES;
/// Deck page: cards per page.
pub const CARDS_PER_PAGE: u32 = 25;
/// One editor upload: the per-file media cap plus multipart framing.
pub(crate) const MEDIA_UPLOAD_MAX_BYTES: usize =
    flash_store::media::MAX_FILE_BYTES as usize + 64 * 1024;

/// The core's site root: the dashboard for members, the login page for
/// everyone else, and a robots.txt that keeps crawlers out of a private
/// server. A hosted build replaces this bundle with its marketing site.
pub fn site_routes() -> Router<AppState> {
    Router::new()
        .route("/", get(home))
        .route("/robots.txt", get(robots_private))
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/login", get(login_page))
        .route("/connect", get(connect_page))
        .route("/enroll/{token}", get(enroll_page))
        .route("/reset", get(crate::password::reset_request_page))
        .route("/reset/{token}", get(crate::password::reset_confirm_page))
        .route(
            "/settings/password/set",
            post(crate::password::settings_set_password),
        )
        .route(
            "/settings/password/change",
            post(crate::password::settings_change_password),
        )
        .route(
            "/settings/password/remove",
            post(crate::password::settings_remove_password),
        )
        .route("/decks", get(decks_page).post(create_deck))
        .route("/decks/{id}", get(deck_page))
        .route("/decks/{id}/cards", get(card_rows).post(create_card))
        .route("/decks/{id}/notes", post(create_note))
        .route("/cards/{id}/edit", get(card_editor).post(save_card))
        .route(
            "/media",
            post(crate::media::upload_media).layer(DefaultBodyLimit::max(MEDIA_UPLOAD_MAX_BYTES)),
        )
        .route("/decks/{id}/settings", get(deck_settings_page))
        .route("/decks/{id}/rename", post(rename_deck))
        .route("/decks/{id}/limits", post(save_deck_limits))
        .route("/decks/{id}/boost", post(boost_deck))
        .route("/decks/{id}/delete", post(delete_deck))
        .route("/study/boost", post(boost_all))
        .route("/connect/dismiss", post(dismiss_connect_cta))
        .route("/cards/{id}", delete(delete_card))
        .route("/study", get(study_page))
        .route("/study/reveal", post(study_reveal))
        .route("/study/review", post(study_review))
        .route("/stats", get(stats_page))
        .route("/users", get(users_page))
        .route("/settings", get(settings_page))
        .route("/settings/grading", post(save_grading))
        .route("/settings/appearance", post(save_appearance))
        .route("/settings/name", post(save_name))
        .route("/settings/scheduling", post(save_scheduling))
        .route("/settings/invite", post(create_invite))
        .route("/settings/apps/disconnect", post(disconnect_app))
        .route(
            "/settings/delete-account",
            post(crate::password::settings_delete_account),
        )
        .route("/import", get(import_page))
        .route(
            "/import/preview",
            post(import_preview).layer(DefaultBodyLimit::max(IMPORT_MAX_BYTES)),
        )
        .route("/import/commit", post(import_commit))
        .route("/media/{id}", get(crate::media::serve_media))
        .route("/export.apkg", get(export_apkg))
        .route("/export.csv", get(export_csv))
        .route("/static/og.png", get(static_og))
        .route("/static/app.css", get(static_css))
        .route("/static/htmx.min.js", get(static_htmx))
        .route("/static/webauthn.js", get(static_webauthn))
        .route("/static/study.js", get(static_study))
        .route("/static/favicon.svg", get(static_favicon))
        // Icon scrapers (connector dialogs, link previews) try this path
        // before parsing <link rel="icon">; serve the same square tile.
        .route("/favicon.ico", get(static_favicon))
        .route("/static/ui.js", get(static_ui))
        .route("/static/editor.js", get(static_editor))
        .route("/static/fonts/Geist-Regular.woff2", get(font_sans_regular))
        .route("/static/fonts/Geist-Medium.woff2", get(font_sans_medium))
        .route(
            "/static/fonts/Geist-SemiBold.woff2",
            get(font_sans_semibold),
        )
        .route(
            "/static/fonts/GeistMono-Regular.woff2",
            get(font_mono_regular),
        )
        .route(
            "/static/fonts/GeistMono-Medium.woff2",
            get(font_mono_medium),
        )
        .route(
            "/static/fonts/GeistMono-SemiBold.woff2",
            get(font_mono_semibold),
        )
        .route("/static/katex/katex.min.css", get(katex_css))
        .route("/static/katex/katex.min.js", get(katex_js))
        .route("/static/katex/auto-render.min.js", get(katex_autorender))
        .route("/static/katex/fonts/{file}", get(katex_font))
}

/// Credential-ceremony endpoints, rate-limited separately in lib.rs.
pub fn auth_router() -> Router<AppState> {
    Router::new()
        .route("/reset", post(crate::password::reset_request_submit))
        .route(
            "/auth/login/password",
            post(crate::password::login_password),
        )
        .route(
            "/auth/reset/{token}",
            post(crate::password::reset_confirm_submit),
        )
        .route(
            "/auth/enroll/{token}/password",
            post(crate::password::enroll_password),
        )
        .route("/auth/enroll/{token}/start", post(webauthn::enroll_start))
        .route("/auth/enroll/finish", post(webauthn::enroll_finish))
        .route("/auth/login/start", post(webauthn::login_start))
        .route("/auth/login/finish", post(webauthn::login_finish))
        .route("/auth/logout", post(webauthn::logout))
}

// ---- plumbing ----

/// Per-request context consumed by base.html: theme + active nav item,
/// the viewer's role so the sidebar can show admin-only entries, and the
/// extension's own sidebar items.
pub struct Shell {
    pub theme: &'static str,
    pub active: &'static str,
    pub is_admin: bool,
    /// The extension's items after "Stats".
    pub nav_html: String,
    /// The extension's items after "Users", in the Account section.
    pub account_nav_html: String,
}

impl Shell {
    pub fn new(state: &AppState, theme: Theme, active: &'static str, is_admin: bool) -> Shell {
        Shell {
            theme: theme.as_str(),
            active,
            is_admin,
            nav_html: state.ext.nav_html(NavSection::Primary, active, is_admin),
            account_nav_html: state.ext.nav_html(NavSection::Account, active, is_admin),
        }
    }
}

/// One value from a raw query string, for the flags the extension's own
/// redirects carry (`?g=linked`, `?share=published`): plain tokens, no
/// percent-decoding needed.
pub fn query_param<'a>(raw: &'a str, key: &str) -> Option<&'a str> {
    raw.split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v)
}

/// A post-login destination is honored only when it is a local path.
pub fn clean_next(raw: Option<String>) -> String {
    match raw {
        Some(n) if is_local_path(&n) => n,
        _ => "/".to_string(),
    }
}

/// A path browsers will resolve against this origin and nothing else.
/// Besides the obvious `//host`, the WHATWG URL parser treats `\` as `/`
/// for http(s) and strips tabs and newlines before parsing, so `/\host`
/// and `/<tab>/host` are protocol-relative too. Control characters are
/// also refused because they cannot be placed in a Location header.
pub fn is_local_path(n: &str) -> bool {
    n.len() <= 512
        && n.starts_with('/')
        && !n.starts_with("//")
        && !n.starts_with("/\\")
        && !n.bytes().any(|b| b < 0x20 || b == 0x7f || b == b'\\')
}

/// Channel codes from ?ref=: lowercased, short, [a-z0-9_-] only. Anything
/// else (including hostile junk) collapses to None.
pub fn clean_ref(raw: Option<String>) -> Option<String> {
    let r = raw?.trim().to_lowercase();
    if r.is_empty()
        || r.len() > 32
        || !r
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
    {
        return None;
    }
    Some(r)
}

/// Why a page request is not a person looking at a page, or None when it
/// probably is: a crawler or script user agent (`ua`, or `no_ua` when
/// absent), a `HEAD` probe, a request that doesn't accept HTML
/// (`accept`), a browser prefetch or prerender (`prefetch`), or a
/// subresource fetch (`fetch`). Headers are only inspected, never stored.
pub fn bot_reason(
    method: &axum::http::Method,
    headers: &axum::http::HeaderMap,
) -> Option<&'static str> {
    let text = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    if method == axum::http::Method::HEAD {
        return Some("head");
    }
    if let Some(reason) = agent_reason(headers) {
        return Some(reason);
    }
    if let Some(accept) = text("accept") {
        if !accept.contains("text/html") && !accept.contains("*/*") {
            return Some("accept");
        }
    }
    let purpose = text("sec-purpose")
        .or_else(|| text("purpose"))
        .unwrap_or("");
    if purpose.contains("prefetch") || purpose.contains("prerender") {
        return Some("prefetch");
    }
    if let Some(dest) = text("sec-fetch-dest") {
        if dest != "document" {
            return Some("fetch");
        }
    }
    None
}

/// The user-agent half of the gate alone: `no_ua` when absent, `ua` when
/// it names a crawler or script. The right check for a request a page's
/// own script sends, which carries no document headers.
pub fn agent_reason(headers: &axum::http::HeaderMap) -> Option<&'static str> {
    let Some(ua) = headers.get("user-agent").and_then(|v| v.to_str().ok()) else {
        return Some("no_ua");
    };
    let ua = ua.to_ascii_lowercase();
    [
        "bot",
        "crawl",
        "spider",
        "slurp",
        "preview",
        "curl",
        "wget",
        "python",
        "httpx",
        "go-http",
        "headless",
        "lighthouse",
    ]
    .iter()
    .any(|m| ua.contains(m))
    .then_some("ua")
}

/// True when the request looks like a crawler or script rather than a
/// person — those must not inflate view counters.
pub fn is_probably_bot(headers: &axum::http::HeaderMap) -> bool {
    bot_reason(&axum::http::Method::GET, headers).is_some()
}

/// The extension's anonymous product counter (event + ref only — never a
/// user id, session, or IP).
pub(crate) fn count_event(state: &AppState, event: &'static str, ref_code: Option<&str>) {
    state.ext.count_event(&state.services, event, ref_code);
}

/// The extension's per-account counter for something a signed-in user
/// did on the web (never a request address or user agent).
pub(crate) fn account_event(
    state: &AppState,
    user: UserId,
    event: &'static str,
    detail: Option<&str>,
    amount: Option<i64>,
) {
    state
        .ext
        .account_event(&state.services, user, event, Surface::Web, detail, amount);
}

pub fn err_response(err: ServiceError) -> Response {
    match err {
        ServiceError::Invalid(msg) => (StatusCode::BAD_REQUEST, msg).into_response(),
        // The store's own validation messages are written for people.
        ServiceError::Store(flash_store::StoreError::Invalid(msg)) => {
            (StatusCode::BAD_REQUEST, msg).into_response()
        }
        ServiceError::Store(flash_store::StoreError::NotFound(what)) => {
            (StatusCode::NOT_FOUND, format!("not found: {what}")).into_response()
        }
        ServiceError::OverCap { .. } => (StatusCode::FORBIDDEN, err.to_string()).into_response(),
        ServiceError::Store(e) if e.is_constraint() => {
            (StatusCode::CONFLICT, "that already exists").into_response()
        }
        other => {
            // Debug, so an `Internal` carries its detail to the log while
            // its Display stays "internal error".
            tracing::error!("service error: {other:?}");
            (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
        }
    }
}

/// Runs a blocking service call and renders the resulting template.
pub async fn respond<T, F>(services: Services, f: F) -> Response
where
    T: Template + Send + 'static,
    F: FnOnce(Services) -> Result<T, ServiceError> + Send + 'static,
{
    match tokio::task::spawn_blocking(move || f(services)).await {
        Ok(Ok(template)) => match template.render() {
            Ok(html) => Html(html).into_response(),
            Err(e) => {
                tracing::error!("template render: {e}");
                (StatusCode::INTERNAL_SERVER_ERROR, "render error").into_response()
            }
        },
        Ok(Err(err)) => err_response(err),
        Err(e) => {
            tracing::error!("join: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
        }
    }
}

pub async fn act<T, F>(services: Services, f: F) -> Result<T, Response>
where
    T: Send + 'static,
    F: FnOnce(Services) -> Result<T, ServiceError> + Send + 'static,
{
    match tokio::task::spawn_blocking(move || f(services)).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(err)) => Err(err_response(err)),
        Err(e) => {
            tracing::error!("join: {e}");
            Err((StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response())
        }
    }
}

// ---- crawler surfaces ----

/// A private server has nothing for crawlers.
async fn robots_private() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            (header::CACHE_CONTROL, "max-age=3600"),
        ],
        "User-agent: *\nDisallow: /\n",
    )
}

// ---- static assets (embedded: the binary is self-contained) ----

async fn static_og() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "image/png"),
            (header::CACHE_CONTROL, "max-age=86400"),
        ],
        include_bytes!("../static/og.png").as_slice(),
    )
}

async fn static_css() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/css"),
            (header::CACHE_CONTROL, "max-age=3600"),
        ],
        include_str!("../static/app.css"),
    )
}

async fn static_htmx() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "application/javascript"),
            (header::CACHE_CONTROL, "max-age=3600"),
        ],
        include_str!("../static/htmx.min.js"),
    )
}

async fn static_webauthn() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "application/javascript"),
            (header::CACHE_CONTROL, "max-age=3600"),
        ],
        include_str!("../static/webauthn.js"),
    )
}

async fn static_ui() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "application/javascript"),
            (header::CACHE_CONTROL, "max-age=3600"),
        ],
        include_str!("../static/ui.js"),
    )
}

async fn static_favicon() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "image/svg+xml"),
            (header::CACHE_CONTROL, "max-age=86400"),
        ],
        include_str!("../static/favicon.svg"),
    )
}

async fn static_editor() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "application/javascript"),
            (header::CACHE_CONTROL, "max-age=3600"),
        ],
        include_str!("../static/editor.js"),
    )
}

async fn static_study() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "application/javascript"),
            (header::CACHE_CONTROL, "max-age=3600"),
        ],
        include_str!("../static/study.js"),
    )
}

macro_rules! font_handler {
    ($name:ident, $file:literal) => {
        async fn $name() -> impl IntoResponse {
            (
                [
                    (header::CONTENT_TYPE, "font/woff2"),
                    (header::CACHE_CONTROL, "max-age=86400"),
                ],
                include_bytes!(concat!("../static/fonts/", $file)).as_slice(),
            )
        }
    };
}

font_handler!(font_sans_regular, "Geist-Regular.woff2");
font_handler!(font_sans_medium, "Geist-Medium.woff2");
font_handler!(font_sans_semibold, "Geist-SemiBold.woff2");
font_handler!(font_mono_regular, "GeistMono-Regular.woff2");
font_handler!(font_mono_medium, "GeistMono-Medium.woff2");
font_handler!(font_mono_semibold, "GeistMono-SemiBold.woff2");

// ---- KaTeX (self-hosted; CSP forbids CDNs) ----

async fn katex_css() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/css"),
            (header::CACHE_CONTROL, "max-age=3600"),
        ],
        include_str!("../static/katex/katex.min.css"),
    )
}

async fn katex_js() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "application/javascript"),
            (header::CACHE_CONTROL, "max-age=3600"),
        ],
        include_str!("../static/katex/katex.min.js"),
    )
}

async fn katex_autorender() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "application/javascript"),
            (header::CACHE_CONTROL, "max-age=3600"),
        ],
        include_str!("../static/katex/auto-render.min.js"),
    )
}

async fn katex_font(Path(file): Path<String>) -> Response {
    macro_rules! fonts {
        ($($name:literal),+ $(,)?) => {
            match file.as_str() {
                $($name => Some(
                    include_bytes!(concat!("../static/katex/fonts/", $name)).as_slice(),
                ),)+
                _ => None,
            }
        };
    }
    let bytes: Option<&'static [u8]> = fonts!(
        "KaTeX_AMS-Regular.woff2",
        "KaTeX_Caligraphic-Bold.woff2",
        "KaTeX_Caligraphic-Regular.woff2",
        "KaTeX_Fraktur-Bold.woff2",
        "KaTeX_Fraktur-Regular.woff2",
        "KaTeX_Main-Bold.woff2",
        "KaTeX_Main-BoldItalic.woff2",
        "KaTeX_Main-Italic.woff2",
        "KaTeX_Main-Regular.woff2",
        "KaTeX_Math-BoldItalic.woff2",
        "KaTeX_Math-Italic.woff2",
        "KaTeX_SansSerif-Bold.woff2",
        "KaTeX_SansSerif-Italic.woff2",
        "KaTeX_SansSerif-Regular.woff2",
        "KaTeX_Script-Regular.woff2",
        "KaTeX_Size1-Regular.woff2",
        "KaTeX_Size2-Regular.woff2",
        "KaTeX_Size3-Regular.woff2",
        "KaTeX_Size4-Regular.woff2",
        "KaTeX_Typewriter-Regular.woff2",
    );
    match bytes {
        Some(b) => (
            [
                (header::CONTENT_TYPE, "font/woff2"),
                (header::CACHE_CONTROL, "max-age=86400"),
            ],
            b,
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

// ---- auth pages ----

#[derive(Template)]
#[template(path = "login.html")]
struct LoginPage {
    next: String,
    theme: &'static str,
    /// The extension's external sign-in buttons (and their outcome line).
    providers_html: String,
    /// Where "Create an account" points; None hides it.
    signup_path: Option<String>,
    email: String,
    /// The password form's own failure line.
    error: Option<String>,
    /// Neutral status line (e.g. after account deletion), not an error.
    notice: Option<String>,
}

fn render_login(
    state: &AppState,
    theme: Theme,
    next: String,
    provider_err: Option<&str>,
    email: String,
    error: Option<String>,
    notice: Option<String>,
) -> Response {
    let providers_html = match state.ext.render(
        state,
        Slot::LoginProviders {
            next: &next,
            err: provider_err,
            ref_code: None,
        },
    ) {
        Ok(html) => html,
        Err(e) => return err_response(e),
    };
    let page = LoginPage {
        next,
        theme: theme.as_str(),
        providers_html,
        signup_path: state.ext.signup_path(state),
        email,
        error,
        notice,
    };
    match page.render() {
        Ok(html) => Html(html).into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "render error").into_response(),
    }
}

/// Re-renders the login page with the uniform password-failure message
/// (used by the password login handler in `password.rs`).
pub(crate) fn render_login_error(
    state: &AppState,
    theme: Theme,
    next: String,
    email: String,
    msg: &str,
) -> Response {
    render_login(state, theme, next, None, email, Some(msg.to_string()), None)
}

#[derive(Deserialize)]
struct NextQuery {
    #[serde(default, deserialize_with = "crate::bounds::opt_next_path")]
    next: Option<String>,
    deleted: Option<u8>,
    /// External sign-in outcomes that need a word on the login page; the
    /// extension owns the codes and their copy.
    #[serde(default, deserialize_with = "crate::bounds::opt_keyword")]
    err: Option<String>,
}

async fn login_page(
    State(state): State<AppState>,
    theme: Theme,
    Query(query): Query<NextQuery>,
) -> Response {
    // `next` lands in a data attribute (HTML-escaped by askama); only
    // local paths are honored.
    let next = clean_next(query.next);
    render_login(
        &state,
        theme,
        next,
        query.err.as_deref(),
        String::new(),
        None,
        (query.deleted == Some(1))
            .then(|| "Your account and all its data have been deleted.".to_string()),
    )
}

#[derive(Template)]
#[template(path = "enroll.html")]
struct EnrollPage {
    display_name: String,
    token: String,
    theme: &'static str,
    /// Set for signup invites: the verified address, shown read-only.
    email: Option<String>,
    error: Option<String>,
    /// Where to land after enrolling (e.g. the shared deck that brought
    /// them here); always a local path.
    next: String,
}

/// Re-renders the enroll page with an error (password-chooser failures).
pub(crate) fn render_enroll_error(
    theme: Theme,
    token: &str,
    enrollment: &enroll::Enrollment,
    msg: &str,
    next: &str,
) -> Response {
    let page = EnrollPage {
        display_name: enrollment.invite.display_name.clone(),
        token: token.to_string(),
        theme: theme.as_str(),
        email: enrollment.preset_email().map(str::to_string),
        error: Some(msg.to_string()),
        next: next.to_string(),
    };
    Html(page.render().unwrap_or_default()).into_response()
}

#[derive(Deserialize)]
struct EnrollQuery {
    #[serde(default, deserialize_with = "crate::bounds::opt_next_path")]
    next: Option<String>,
}

async fn enroll_page(
    State(state): State<AppState>,
    theme: Theme,
    Path(token): Path<String>,
    Query(query): Query<EnrollQuery>,
    headers: axum::http::HeaderMap,
) -> Response {
    let next = clean_next(query.next);
    if !crate::password::token_shape_ok(&token) {
        return (StatusCode::BAD_REQUEST, "malformed invite").into_response();
    }
    let hash = hash_token(&token);
    let now = now_ms();
    let hooks = state.ext.clone();
    let enrollment = act(state.services.clone(), move |s| {
        Ok(enroll::load(s.store(), hooks.as_ref(), &hash, now)?)
    })
    .await;
    match enrollment {
        Ok(Some(enrollment)) => {
            if !is_probably_bot(&headers) {
                let ref_code = enrollment
                    .signup
                    .as_ref()
                    .and_then(|s| s.ref_code.as_deref());
                count_event(&state, "enroll_view", ref_code);
            }
            let page = EnrollPage {
                email: enrollment.preset_email().map(str::to_string),
                display_name: enrollment.invite.display_name,
                token,
                theme: theme.as_str(),
                error: None,
                next,
            };
            Html(page.render().unwrap_or_default()).into_response()
        }
        Ok(None) => (
            StatusCode::FORBIDDEN,
            "This invite link is invalid, expired, or already used.",
        )
            .into_response(),
        Err(response) => response,
    }
}

#[derive(Template)]
#[template(path = "notfound.html")]
struct NotFoundPage {
    theme: &'static str,
}

/// Explicit app-wide fallback: without one, unknown paths fall through to
/// the bearer-auth layer's wrapped default fallback and 401 instead of 404.
pub async fn not_found(theme: Theme) -> Response {
    (
        StatusCode::NOT_FOUND,
        Html(
            NotFoundPage {
                theme: theme.as_str(),
            }
            .render()
            .unwrap_or_default(),
        ),
    )
        .into_response()
}

// ---- connect (public how-to for attaching an AI) ----

#[derive(Template)]
#[template(path = "connect.html")]
struct ConnectPage {
    site: Arc<Site>,
    theme: &'static str,
    /// Header shows "Back to Flash" instead of "Log in" for signed-in users.
    signed_in: bool,
    /// The extension's head tags (`Slot::PublicHead`).
    head_html: String,
}

async fn connect_page(
    State(state): State<AppState>,
    OptionalUser(session): OptionalUser,
    theme: Theme,
    headers: axum::http::HeaderMap,
) -> Response {
    // A signed-out load is only a load: whether anyone was behind it is
    // the extension's to find out (its head script reports a visitor who
    // interacts with the page).
    match session {
        Some((user, _)) => account_event(&state, user, "view", Some("connect"), None),
        None if !is_probably_bot(&headers) => count_event(&state, "connect_load", None),
        None => {}
    }
    let page = ConnectPage {
        site: state.site.clone(),
        theme: theme.as_str(),
        signed_in: session.is_some(),
        head_html: state
            .ext
            .render(&state, crate::ext::Slot::PublicHead)
            .unwrap_or_default(),
    };
    match page.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            tracing::error!("connect render: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "render error").into_response()
        }
    }
}

/// A public page rendered whole; a template failure is logged and
/// answered with a plain 500 (public pages have no shell to fall back to).
pub fn render_public(name: &str, page: Result<String, askama::Error>) -> Response {
    match page {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            tracing::error!(template = name, "render failed: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "render error").into_response()
        }
    }
}

// ---- dashboard ----

#[derive(Template)]
#[template(path = "dashboard.html")]
struct DashboardPage {
    shell: Shell,
    due_now: u32,
    new_available: u32,
    reviewed_today: u32,
    /// Account-wide "more new cards today" already granted.
    boost_today: u32,
    decks: Vec<DeckSummary>,
    /// The "Connect your AI" card, until the user dismisses it.
    show_connect_cta: bool,
}

/// `/` on the open server: members get the dashboard, everyone else the
/// login page.
async fn home(
    State(state): State<AppState>,
    OptionalUser(session): OptionalUser,
    theme: Theme,
) -> Response {
    match session {
        Some((user, is_admin)) => dashboard_for(&state, user, is_admin, theme).await,
        None => Redirect::to("/login").into_response(),
    }
}

/// The signed-in dashboard, shared by every owner of `/`.
pub async fn dashboard_for(
    state: &AppState,
    user: UserId,
    is_admin: bool,
    theme: Theme,
) -> Response {
    let shell = Shell::new(state, theme, "today", is_admin);
    account_event(state, user, "view", Some("today"), None);
    respond(state.services.clone(), move |s| {
        let d = s.dashboard(user, now_ms())?;
        Ok(DashboardPage {
            shell,
            due_now: d.counts.due,
            new_available: d.counts.new_available,
            reviewed_today: d.counts.reviewed_today,
            boost_today: d.boost_today,
            decks: d.decks,
            show_connect_cta: d.show_connect_cta,
        })
    })
    .await
}

async fn dismiss_connect_cta(
    State(state): State<AppState>,
    AuthUser { id: user, .. }: AuthUser,
) -> Response {
    match act(state.services.clone(), move |s| s.dismiss_connect_cta(user)).await {
        Ok(()) => Redirect::to("/").into_response(),
        Err(response) => response,
    }
}

// ---- decks & cards ----

#[derive(Template)]
#[template(path = "decks.html")]
struct DecksPage {
    shell: Shell,
    decks: Vec<DeckSummary>,
    usage_banner: String,
    usage_count: String,
}

async fn decks_page(
    State(state): State<AppState>,
    AuthUser { id: user, is_admin }: AuthUser,
    theme: Theme,
) -> Response {
    let shell = Shell::new(&state, theme, "decks", is_admin);
    account_event(&state, user, "view", Some("decks"), None);
    let app = state.clone();
    respond(state.services.clone(), move |s| {
        Ok(DecksPage {
            shell,
            decks: s.list_decks(user, now_ms())?,
            usage_banner: app.ext.render(
                &app,
                Slot::UsageBanner {
                    user,
                    limit_hit: false,
                },
            )?,
            usage_count: app.ext.render(&app, Slot::UsageCount { user })?,
        })
    })
    .await
}

#[derive(Deserialize)]
struct NewDeckForm {
    #[serde(deserialize_with = "crate::bounds::deck_name")]
    name: String,
}

async fn create_deck(
    State(state): State<AppState>,
    AuthUser { id: user, .. }: AuthUser,
    Form(form): Form<NewDeckForm>,
) -> Response {
    match act(state.services.clone(), move |s| {
        s.create_deck(user, &form.name, "", now_ms())
    })
    .await
    {
        Ok(deck) => {
            account_event(&state, user, "deck_created", None, None);
            Redirect::to(&format!("/decks/{deck}")).into_response()
        }
        Err(response) => response,
    }
}

struct CardView {
    id: i64,
    front: String,
    back: String,
    front_html: Option<String>,
    back_html: Option<String>,
    tags: String,
    /// "cloze 2", "reversed", "typed" — how this card relates to its note.
    kind: Option<String>,
}

impl CardView {
    fn from_rows(rows: Vec<CardRow>) -> Vec<CardView> {
        rows.into_iter()
            .map(|c| CardView {
                id: c.id.0,
                kind: crate::service::card_kind(&c),
                front: c.front,
                back: c.back,
                front_html: c.front_html,
                back_html: c.back_html,
                tags: c.tags.join(", "),
            })
            .collect()
    }
}

/// One page of a deck's cards plus what the pager needs; shared by the
/// full deck page and the htmx rows partial (same field names, so
/// `card_rows.html` renders from either context).
struct CardsView {
    cards: Vec<CardView>,
    deck_id: i64,
    page: u32,
    pages: u32,
    total: u32,
    q: String,
}

fn cards_view(
    s: &Services,
    user: UserId,
    deck: DeckId,
    q: Option<String>,
    page: Option<u32>,
) -> Result<CardsView, ServiceError> {
    let q = q.unwrap_or_default().trim().to_string();
    let view = s.deck_cards_page(
        user,
        deck,
        Some(&q),
        page.unwrap_or(1),
        CARDS_PER_PAGE,
        now_ms(),
    )?;
    Ok(CardsView {
        cards: CardView::from_rows(view.rows),
        deck_id: deck.0,
        page: view.page,
        pages: view.pages,
        total: view.total,
        q,
    })
}

/// The advanced editor's prefill (empty for the deck page's add form).
struct EditorView {
    card_id: i64,
    note_type: &'static str,
    front_html: String,
    back_html: String,
    tags: String,
    sibling_count: usize,
    types: Vec<(&'static str, &'static str)>,
    hues: Vec<&'static str>,
}

impl EditorView {
    fn empty() -> Self {
        EditorView {
            card_id: 0,
            note_type: NoteType::Basic.as_str(),
            front_html: String::new(),
            back_html: String::new(),
            tags: String::new(),
            sibling_count: 0,
            types: NoteType::ALL
                .iter()
                .map(|t| (t.as_str(), t.label()))
                .collect(),
            hues: flash_store::richtext::HIGHLIGHT_HUES.to_vec(),
        }
    }

    fn from_seed(seed: EditorSeed) -> Self {
        EditorView {
            card_id: seed.card_id.0,
            note_type: seed.note_type.as_str(),
            front_html: seed.front_html,
            back_html: seed.back_html,
            tags: seed.tags.join(", "),
            sibling_count: seed.sibling_count,
            ..EditorView::empty()
        }
    }
}

#[derive(Deserialize)]
struct NoteForm {
    #[serde(deserialize_with = "crate::bounds::keyword")]
    note_type: String,
    #[serde(default, deserialize_with = "crate::bounds::field_html")]
    front_html: String,
    #[serde(default, deserialize_with = "crate::bounds::field_html")]
    back_html: String,
    #[serde(default, deserialize_with = "crate::bounds::tag_list_text")]
    tags: String,
}

impl NoteForm {
    fn into_input(self) -> Result<NoteInput, ServiceError> {
        let note_type = NoteType::parse(&self.note_type)
            .ok_or_else(|| ServiceError::Invalid("unknown note type".into()))?;
        Ok(NoteInput {
            note_type,
            front_html: self.front_html,
            back_html: self.back_html,
            tags: split_tags(&self.tags),
        })
    }
}

fn split_tags(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

/// Short query-string code for an editor rejection, so the deck page can
/// show a friendly banner after the redirect.
fn editor_error_code(err: &ServiceError) -> &'static str {
    match err {
        ServiceError::Invalid(m) if m.contains("cloze deletion") => "nocloze",
        ServiceError::Invalid(m) if m.contains("is empty") => "empty",
        ServiceError::Invalid(m) if m.contains("exceeds") => "long",
        ServiceError::Invalid(m) if m.contains("upload") => "media",
        _ => "invalid",
    }
}

fn editor_error_copy(code: &str) -> Option<&'static str> {
    Some(match code {
        "nocloze" => "That card wasn't added: a cloze note needs at least one {{c1::…}} deletion. Select the answer and press Ctrl+Shift+C.",
        "empty" => "That card wasn't added: both sides need some content.",
        "long" => "That card wasn't added: a side is too long.",
        "media" => "That card wasn't added: it references media that isn't yours.",
        "invalid" => "That card wasn't added: the content couldn't be saved.",
        _ => return None,
    })
}

/// Deck-page view of daily limits: the deck's overrides (None = inherit)
/// beside the account defaults they'd inherit, plus today's boost.
struct DeckLimitsView {
    new_override: Option<u32>,
    reviews_override: Option<u32>,
    default_new: u32,
    default_reviews: u32,
    boost_today: u32,
}

#[derive(Template)]
#[template(path = "deck.html")]
struct DeckPage {
    shell: Shell,
    deck_id: i64,
    deck_name: String,
    cards: Vec<CardView>,
    page: u32,
    pages: u32,
    total: u32,
    q: String,
    /// The extension's over-limit banner (after a blocked add, `?limit=1`
    /// makes it say why).
    usage_banner: String,
    /// Why the last advanced add was refused, if it was.
    editor_error: Option<&'static str>,
    editor: EditorView,
}

#[derive(Deserialize)]
struct DeckPageQuery {
    limit: Option<u8>,
    page: Option<u32>,
    #[serde(default, deserialize_with = "crate::bounds::opt_keyword")]
    err: Option<String>,
}

async fn deck_page(
    State(state): State<AppState>,
    AuthUser { id: user, is_admin }: AuthUser,
    theme: Theme,
    Path(id): Path<i64>,
    Query(query): Query<DeckPageQuery>,
) -> Response {
    let shell = Shell::new(&state, theme, "decks", is_admin);
    let limit_hit = query.limit == Some(1);
    let editor_error = query.err.as_deref().and_then(editor_error_copy);
    let app = state.clone();
    respond(state.services.clone(), move |s| {
        let deck = DeckId(id);
        let name = s.deck_by_id(user, deck, now_ms())?.name;
        let view = cards_view(&s, user, deck, None, query.page)?;
        Ok(DeckPage {
            shell,
            deck_id: id,
            deck_name: name,
            cards: view.cards,
            page: view.page,
            pages: view.pages,
            total: view.total,
            q: view.q,
            usage_banner: app
                .ext
                .render(&app, Slot::UsageBanner { user, limit_hit })?,
            editor_error,
            editor: EditorView::empty(),
        })
    })
    .await
}

#[derive(Template)]
#[template(path = "deck_settings.html")]
struct DeckSettingsPage {
    shell: Shell,
    deck_id: i64,
    deck_name: String,
    limits: DeckLimitsView,
    /// Which form just saved: "name" | "limits" | "".
    saved: String,
    /// Why the last rename was refused, if it was.
    name_error: Option<String>,
    /// The extension's sharing card.
    sharing_html: String,
}

#[derive(Deserialize)]
struct DeckSettingsQuery {
    #[serde(default, deserialize_with = "crate::bounds::opt_keyword")]
    saved: Option<String>,
    #[serde(default, deserialize_with = "crate::bounds::opt_keyword")]
    err: Option<String>,
}

async fn deck_settings_page(
    State(state): State<AppState>,
    AuthUser { id: user, is_admin }: AuthUser,
    theme: Theme,
    Path(id): Path<i64>,
    Query(query): Query<DeckSettingsQuery>,
    RawQuery(raw_query): RawQuery,
) -> Response {
    let shell = Shell::new(&state, theme, "decks", is_admin);
    let saved = query
        .saved
        .filter(|v| matches!(v.as_str(), "name" | "limits"))
        .unwrap_or_default();
    let name_error = match query.err.as_deref() {
        Some("empty") => Some("the name can't be blank".to_string()),
        Some("taken") => Some("you already have a deck with that name".to_string()),
        _ => None,
    };
    let app = state.clone();
    respond(state.services.clone(), move |s| {
        let deck = DeckId(id);
        let d = s.deck_detail(user, deck, now_ms())?;
        Ok(DeckSettingsPage {
            shell,
            deck_id: id,
            deck_name: d.deck.name,
            limits: DeckLimitsView {
                new_override: d.limits.new_per_day,
                reviews_override: d.limits.reviews_per_day,
                default_new: d.default_new,
                default_reviews: d.default_reviews,
                boost_today: d.boost_today,
            },
            saved,
            name_error,
            sharing_html: app.ext.render(
                &app,
                Slot::DeckSharing {
                    user,
                    deck,
                    query: raw_query.as_deref().unwrap_or_default(),
                },
            )?,
        })
    })
    .await
}

#[derive(Deserialize)]
struct RenameDeckForm {
    #[serde(deserialize_with = "crate::bounds::deck_name")]
    name: String,
}

async fn rename_deck(
    State(state): State<AppState>,
    AuthUser { id: user, .. }: AuthUser,
    Path(id): Path<i64>,
    Form(form): Form<RenameDeckForm>,
) -> Response {
    if form.name.trim().is_empty() {
        return Redirect::to(&format!("/decks/{id}/settings?err=empty")).into_response();
    }
    match tokio::task::spawn_blocking({
        let services = state.services.clone();
        move || services.rename_deck(user, DeckId(id), &form.name)
    })
    .await
    {
        Ok(Ok(())) => Redirect::to(&format!("/decks/{id}/settings?saved=name")).into_response(),
        Ok(Err(ServiceError::Store(flash_store::StoreError::Invalid(_)))) => {
            Redirect::to(&format!("/decks/{id}/settings?err=taken")).into_response()
        }
        Ok(Err(err)) => err_response(err),
        Err(e) => {
            tracing::error!("join: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
        }
    }
}

#[derive(Deserialize)]
struct DeckLimitsForm {
    #[serde(deserialize_with = "crate::bounds::keyword")]
    new_per_day: String,
    #[serde(deserialize_with = "crate::bounds::keyword")]
    reviews_per_day: String,
}

/// Blank = inherit (None); otherwise a non-negative integer.
fn parse_limit(raw: &str) -> Option<Option<u32>> {
    let t = raw.trim();
    if t.is_empty() {
        Some(None)
    } else {
        t.parse::<u32>().ok().map(Some)
    }
}

async fn save_deck_limits(
    State(state): State<AppState>,
    AuthUser { id: user, .. }: AuthUser,
    Path(id): Path<i64>,
    Form(form): Form<DeckLimitsForm>,
) -> Response {
    let (Some(new), Some(reviews)) = (
        parse_limit(&form.new_per_day),
        parse_limit(&form.reviews_per_day),
    ) else {
        return (StatusCode::BAD_REQUEST, "limits must be whole numbers").into_response();
    };
    match act(state.services.clone(), move |s| {
        s.set_deck_limits(user, DeckId(id), new, reviews)
    })
    .await
    {
        Ok(()) => Redirect::to(&format!("/decks/{id}/settings?saved=limits")).into_response(),
        Err(response) => response,
    }
}

#[derive(Deserialize)]
struct BoostForm {
    extra: u32,
    /// `study` sends the user back into the study queue they came from.
    #[serde(default, deserialize_with = "crate::bounds::opt_keyword")]
    back: Option<String>,
}

async fn boost_deck(
    State(state): State<AppState>,
    AuthUser { id: user, .. }: AuthUser,
    Path(id): Path<i64>,
    Form(form): Form<BoostForm>,
) -> Response {
    match act(state.services.clone(), move |s| {
        s.boost_new_today(user, Some(DeckId(id)), form.extra, now_ms())
    })
    .await
    {
        Ok(_) if form.back.as_deref() == Some("study") => {
            Redirect::to(&format!("/study?deck={id}")).into_response()
        }
        Ok(_) => Redirect::to(&format!("/decks/{id}/settings")).into_response(),
        Err(response) => response,
    }
}

async fn boost_all(
    State(state): State<AppState>,
    AuthUser { id: user, .. }: AuthUser,
    Form(form): Form<BoostForm>,
) -> Response {
    match act(state.services.clone(), move |s| {
        s.boost_new_today(user, None, form.extra, now_ms())
    })
    .await
    {
        Ok(_) if form.back.as_deref() == Some("study") => Redirect::to("/study").into_response(),
        Ok(_) => Redirect::to("/").into_response(),
        Err(response) => response,
    }
}

#[derive(Template)]
#[template(path = "card_rows.html")]
struct CardRowsPartial {
    cards: Vec<CardView>,
    deck_id: i64,
    page: u32,
    pages: u32,
    total: u32,
    q: String,
}

#[derive(Deserialize)]
struct SearchQuery {
    #[serde(default, deserialize_with = "crate::bounds::opt_search")]
    q: Option<String>,
    page: Option<u32>,
}

async fn card_rows(
    State(state): State<AppState>,
    ApiUser(user): ApiUser,
    Path(id): Path<i64>,
    Query(query): Query<SearchQuery>,
) -> Response {
    respond(state.services.clone(), move |s| {
        let view = cards_view(&s, user, DeckId(id), query.q, query.page)?;
        Ok(CardRowsPartial {
            cards: view.cards,
            deck_id: view.deck_id,
            page: view.page,
            pages: view.pages,
            total: view.total,
            q: view.q,
        })
    })
    .await
}

/// The advanced editor's create path: a full-page form post, like the
/// quick add, so the over-cap and validation banners ride the redirect.
async fn create_note(
    State(state): State<AppState>,
    AuthUser { id: user, .. }: AuthUser,
    Path(id): Path<i64>,
    Form(form): Form<NoteForm>,
) -> Response {
    let result = act(state.services.clone(), move |s| {
        let input = form.into_input()?;
        match s.create_note(user, DeckId(id), &input, now_ms()) {
            Ok(_) => Ok(None),
            Err(ServiceError::OverCap { .. }) => Ok(Some("limit=1")),
            Err(err @ ServiceError::Invalid(_)) => Ok(Some(match editor_error_code(&err) {
                "nocloze" => "err=nocloze",
                "empty" => "err=empty",
                "long" => "err=long",
                "media" => "err=media",
                _ => "err=invalid",
            })),
            Err(other) => Err(other),
        }
    })
    .await;
    match result {
        Ok(None) => {
            account_event(&state, user, "note_created", None, None);
            Redirect::to(&format!("/decks/{id}")).into_response()
        }
        Ok(Some(query)) => Redirect::to(&format!("/decks/{id}?{query}")).into_response(),
        Err(response) => response,
    }
}

#[derive(Template)]
#[template(path = "card_editor.html")]
struct CardEditorDialog {
    editor: EditorView,
    error: Option<String>,
}

/// The edit dialog, prefilled from the card's note (htmx partial).
async fn card_editor(
    State(state): State<AppState>,
    ApiUser(user): ApiUser,
    Path(id): Path<i64>,
) -> Response {
    respond(state.services.clone(), move |s| {
        Ok(CardEditorDialog {
            editor: EditorView::from_seed(s.editor_seed(user, CardId(id))?),
            error: None,
        })
    })
    .await
}

/// Saves the edit dialog. Success asks htmx to reload the page (rows,
/// counts and pager all change); a refusal re-renders the dialog with
/// the reason so nothing typed is lost.
async fn save_card(
    State(state): State<AppState>,
    ApiUser(user): ApiUser,
    Path(id): Path<i64>,
    Form(form): Form<NoteForm>,
) -> Response {
    let result = act(state.services.clone(), move |s| {
        let input = form.into_input()?;
        let saved = s.save_card_editor(user, CardId(id), &input, now_ms());
        match saved {
            Ok(_) => Ok(None),
            Err(err @ (ServiceError::Invalid(_) | ServiceError::OverCap { .. })) => {
                let seed = s.editor_seed(user, CardId(id))?;
                let mut editor = EditorView::from_seed(seed);
                // Echo what was typed, not what is stored.
                editor.note_type = input.note_type.as_str();
                editor.front_html =
                    flash_store::richtext::sanitize_with_media(&input.front_html).into_string();
                editor.back_html =
                    flash_store::richtext::sanitize_with_media(&input.back_html).into_string();
                editor.tags = input.tags.join(", ");
                Ok(Some(CardEditorDialog {
                    editor,
                    error: Some(err.to_string()),
                }))
            }
            Err(other) => Err(other),
        }
    })
    .await;
    match result {
        Ok(None) => {
            account_event(&state, user, "card_edited", Some("editor"), None);
            (StatusCode::NO_CONTENT, [("HX-Refresh", "true")]).into_response()
        }
        Ok(Some(dialog)) => match dialog.render() {
            Ok(html) => Html(html).into_response(),
            Err(e) => {
                tracing::error!("template render: {e}");
                (StatusCode::INTERNAL_SERVER_ERROR, "render error").into_response()
            }
        },
        Err(response) => response,
    }
}

#[derive(Deserialize)]
struct NewCardForm {
    #[serde(deserialize_with = "crate::bounds::card_side")]
    front: String,
    #[serde(deserialize_with = "crate::bounds::card_side")]
    back: String,
    #[serde(default, deserialize_with = "crate::bounds::tag_list_text")]
    tags: String,
}

async fn create_card(
    State(state): State<AppState>,
    AuthUser { id: user, .. }: AuthUser,
    Path(id): Path<i64>,
    Form(form): Form<NewCardForm>,
) -> Response {
    let result = act(state.services.clone(), move |s| {
        let tags = split_tags(&form.tags);
        match s.create_cards_in_deck(user, DeckId(id), &[(form.front, form.back, tags)], now_ms()) {
            Ok(_) => Ok(true),
            // Land back on the deck page with the inline over-limit banner
            // instead of a bare error page.
            Err(ServiceError::OverCap { .. }) => Ok(false),
            Err(other) => Err(other),
        }
    })
    .await;
    match result {
        Ok(true) => {
            account_event(&state, user, "cards_created", Some("manual"), Some(1));
            Redirect::to(&format!("/decks/{id}")).into_response()
        }
        Ok(false) => Redirect::to(&format!("/decks/{id}?limit=1")).into_response(),
        Err(response) => response,
    }
}

async fn delete_card(
    State(state): State<AppState>,
    ApiUser(user): ApiUser,
    Path(id): Path<i64>,
) -> Response {
    match act(state.services.clone(), move |s| {
        s.delete_card(user, CardId(id), now_ms())
    })
    .await
    {
        Ok(()) => StatusCode::OK.into_response(),
        Err(response) => response,
    }
}

// ---- study ----

/// What the study card shows: rich HTML when the import carried it, and
/// whether this card asks for a typed answer.
struct StudyCardView {
    card_id: i64,
    front: String,
    front_html: Option<String>,
    wants_typing: bool,
    /// Images on this card's (still hidden) back: emitted as
    /// `<link rel="preload">` so they're cached before the reveal.
    preload: Vec<i64>,
    /// Images on the next card (both sides): `<link rel="prefetch">`.
    prefetch: Vec<i64>,
}

struct RevealView {
    back: String,
    back_html: Option<String>,
    /// Typed-answer comparison, char-aligned against the expected answer.
    typed_diff: Option<Vec<DiffSpan>>,
}

impl From<StudyCard> for StudyCardView {
    fn from(c: StudyCard) -> Self {
        StudyCardView {
            card_id: c.card_id.0,
            front: c.front,
            front_html: c.front_html,
            wants_typing: c.wants_typing,
            preload: c.preload_media,
            prefetch: c.prefetch_media,
        }
    }
}

#[derive(Template)]
#[template(path = "study.html")]
struct StudyPage {
    shell: Shell,
    card: Option<StudyCardView>,
    revealed: Option<RevealView>,
    session_id: i64,
    remaining: u32,
    total: u32,
    /// Eligible cards today's limits kept out of the queue — the empty
    /// state explains itself instead of a bare "nothing due" (same fix
    /// the MCP queue_note applies for assistants).
    held_by_limit: u32,
    /// The deck being studied, if the session is deck-scoped — the done
    /// state offers a boost for that deck rather than the account.
    scope_deck: Option<i64>,
}

#[derive(Template)]
#[template(path = "study_card.html")]
struct StudyCardPartial {
    card: Option<StudyCardView>,
    revealed: Option<RevealView>,
    session_id: i64,
    remaining: u32,
    total: u32,
    held_by_limit: u32,
    /// The deck being studied, if the session is deck-scoped — the done
    /// state offers a boost for that deck rather than the account.
    scope_deck: Option<i64>,
}

#[derive(Deserialize)]
struct StudyQuery {
    deck: Option<i64>,
    #[serde(default, deserialize_with = "crate::bounds::opt_tag")]
    tag: Option<String>,
}

async fn study_page(
    State(state): State<AppState>,
    AuthUser { id: user, is_admin }: AuthUser,
    theme: Theme,
    Query(query): Query<StudyQuery>,
) -> Response {
    let shell = Shell::new(&state, theme, "study", is_admin);
    account_event(&state, user, "view", Some("study"), None);
    respond(state.services.clone(), move |s| {
        let scope_deck = query.deck;
        let scope = match (query.deck, query.tag) {
            (Some(id), _) => StudyScope::Deck(DeckId(id)),
            (None, Some(tag)) => StudyScope::Tag(tag),
            _ => StudyScope::All,
        };
        let session = s.start_session(user, scope, now_ms())?;
        Ok(StudyPage {
            shell,
            card: session
                .first_card
                .map(|c| s.study_card(user, c, session.next_card_id))
                .transpose()?
                .map(Into::into),
            revealed: None,
            session_id: session.session_id.0,
            remaining: session.cards_due,
            total: session.cards_due,
            held_by_limit: session.held_by_limit,
            scope_deck,
        })
    })
    .await
}

#[derive(Deserialize)]
struct RevealForm {
    session: i64,
    card: i64,
    remaining: u32,
    #[serde(default)]
    total: u32,
    /// What the user typed, when the card asks for it.
    #[serde(default, deserialize_with = "crate::bounds::opt_typed_answer")]
    typed: Option<String>,
}

async fn study_reveal(
    State(state): State<AppState>,
    ApiUser(user): ApiUser,
    Form(form): Form<RevealForm>,
) -> Response {
    respond(state.services.clone(), move |s| {
        let reveal = s.reveal_card(user, CardId(form.card), form.typed.as_deref())?;
        Ok(StudyCardPartial {
            card: Some(StudyCardView {
                card_id: reveal.card_id.0,
                front: reveal.front,
                front_html: reveal.front_html,
                wants_typing: false,  // already answered; reveal shows the back
                preload: Vec::new(),  // the back is on screen now
                prefetch: Vec::new(), // already hinted while the front showed
            }),
            revealed: Some(RevealView {
                back: reveal.back,
                back_html: reveal.back_html,
                typed_diff: reveal.typed_diff,
            }),
            session_id: form.session,
            remaining: form.remaining,
            total: form.total,
            // Reveal always renders a card; the empty state never shows here.
            held_by_limit: 0,
            scope_deck: None,
        })
    })
    .await
}

#[derive(Deserialize)]
struct ReviewForm {
    session: i64,
    card: i64,
    rating: i64,
    #[serde(default)]
    total: u32,
}

async fn study_review(
    State(state): State<AppState>,
    ApiUser(user): ApiUser,
    Form(form): Form<ReviewForm>,
) -> Response {
    respond(state.services.clone(), move |s| {
        let rating = Rating::from_i64(form.rating)
            .ok_or_else(|| ServiceError::Invalid("rating must be 1-4".into()))?;
        let result = s.submit_review(
            user,
            SessionId(form.session),
            CardId(form.card),
            rating,
            ReviewOrigin::WEB,
            now_ms(),
        )?;
        let scope_deck = if result.next_card.is_none() {
            match s.session_scope(user, SessionId(form.session))? {
                StudyScope::Deck(d) => Some(d.0),
                _ => None,
            }
        } else {
            None
        };
        Ok(StudyCardPartial {
            card: result
                .next_card
                .map(|c| s.study_card(user, c, result.next_card_id))
                .transpose()?
                .map(Into::into),
            revealed: None,
            session_id: form.session,
            remaining: result.remaining,
            total: form.total,
            held_by_limit: result.held_by_limit,
            scope_deck,
        })
    })
    .await
}

/// Hard-deletes a deck and all its cards. The button is htmx-confirmed;
/// success answers 200 with HX-Redirect so htmx does a full-page hop back
/// to the deck list.
async fn delete_deck(
    State(state): State<AppState>,
    AuthUser { id: user, .. }: AuthUser,
    Path(id): Path<i64>,
) -> Response {
    match act(state.services.clone(), move |s| {
        s.delete_deck(user, DeckId(id))
    })
    .await
    {
        Ok(deleted) => {
            crate::media::remove_orphans(
                state.services.clone(),
                state.media.clone(),
                &deleted.orphan_blobs,
            );
            let mut response = StatusCode::OK.into_response();
            response.headers_mut().insert(
                axum::http::HeaderName::from_static("hx-redirect"),
                axum::http::HeaderValue::from_static("/decks"),
            );
            response
        }
        Err(response) => response,
    }
}

// ---- stats ----

#[derive(Template)]
#[template(path = "stats.html")]
struct StatsPage {
    shell: Shell,
    reviews_30d: u32,
    retention: String,
    streak: u32,
    longest_streak: u32,
    daily_avg: u32,
    days_learned_pct: u32,
    total_reviews: u32,
    active_cards: u32,
    mature_cards: u32,
    days: Vec<DayCount>,
    upcoming: Vec<DayCount>,
    heat_weeks: Vec<Vec<HeatCell>>,
    heat_months: Vec<MonthSpan>,
}

async fn stats_page(
    State(state): State<AppState>,
    AuthUser { id: user, is_admin }: AuthUser,
    theme: Theme,
) -> Response {
    let shell = Shell::new(&state, theme, "stats", is_admin);
    let heavy = match state.heavy(user) {
        Ok(permit) => permit,
        Err(busy) => return busy.into_response(),
    };
    respond(state.services.clone(), move |s| {
        let stats = s.stats(&heavy, now_ms())?;
        Ok(StatsPage {
            shell,
            reviews_30d: stats.reviews_30d,
            retention: stats
                .retention_pct
                .map(|p| format!("{p}%"))
                .unwrap_or_else(|| "—".to_string()),
            streak: stats.streak,
            longest_streak: stats.longest_streak,
            daily_avg: stats.daily_avg,
            days_learned_pct: stats.days_learned_pct,
            total_reviews: stats.total_reviews,
            active_cards: stats.active_cards,
            mature_cards: stats.mature_cards,
            days: stats.days,
            upcoming: stats.upcoming,
            heat_weeks: stats.heat_weeks,
            heat_months: stats.heat_months,
        })
    })
    .await
}

// ---- users (admin) ----

struct UserRowView {
    display_name: String,
    email: String,
    role: String,
    /// The extension's plan cell; empty on the open server.
    plan_html: String,
    joined: String,
    deck_count: u32,
    card_count: u32,
    review_count: u32,
    last_studied: String,
    last_login: String,
    mcp_client: String,
}

#[derive(Template)]
#[template(path = "users.html")]
struct UsersPage {
    shell: Shell,
    users: Vec<UserRowView>,
    /// Whether any row has a plan cell (the column is the extension's).
    has_plans: bool,
}

async fn users_page(
    State(state): State<AppState>,
    AdminUser(_user): AdminUser,
    theme: Theme,
) -> Response {
    let shell = Shell::new(&state, theme, "users", true);
    let app = state.clone();
    respond(state.services.clone(), move |s| {
        let now = now_ms();
        let mut users = Vec::new();
        for u in s.admin_users(now)? {
            users.push(UserRowView {
                plan_html: app.ext.render(&app, Slot::AdminUserPlan { user: u.id })?,
                display_name: u.display_name,
                email: u.email.unwrap_or_else(|| "—".to_string()),
                role: u.role,
                joined: format_day(u.created_at),
                deck_count: u.deck_count,
                card_count: u.card_count,
                review_count: u.review_count,
                last_studied: match (u.last_review_at, u.last_review_source) {
                    (Some(ts), Some(src)) => format!("{} · {src}", format_ago(now, ts)),
                    (Some(ts), None) => format_ago(now, ts),
                    _ => "never".to_string(),
                },
                last_login: u
                    .last_login_at
                    .map(|ts| format_ago(now, ts))
                    .unwrap_or_else(|| "never".to_string()),
                mcp_client: if u.mcp_clients.is_empty() {
                    "—".to_string()
                } else {
                    u.mcp_clients.join(", ")
                },
            });
        }
        let has_plans = users.iter().any(|u| !u.plan_html.is_empty());
        Ok(UsersPage {
            shell,
            users,
            has_plans,
        })
    })
    .await
}

/// Compact relative time for activity columns; falls back to the date
/// once it's over a month old.
fn format_ago(now_ms: i64, ts_ms: i64) -> String {
    let mins = (now_ms - ts_ms).max(0) / 60_000;
    match mins {
        0 => "just now".to_string(),
        1..=59 => format!("{mins}m ago"),
        60..=1439 => format!("{}h ago", mins / 60),
        1440..=43_199 => format!("{}d ago", mins / 1440),
        _ => format_day(ts_ms),
    }
}

// ---- settings ----

struct PasskeyView {
    label: String,
    created: String,
}

/// One row of the Connected apps card: a connector registered through
/// OAuth or the first-party app, with what disconnecting it revokes.
struct AppView {
    client_id: String,
    name: String,
    first_party: bool,
    tokens: u32,
    since: String,
    /// Empty when the grant has never been used.
    last_used: String,
}

#[derive(Template)]
#[template(path = "settings.html")]
struct SettingsPage {
    shell: Shell,
    display_name: String,
    name_saved: bool,
    name_error: bool,
    grading_mode: String,
    new_per_day: u32,
    reviews_per_day: u32,
    saved: String,
    passkeys: Vec<PasskeyView>,
    apps: Vec<AppView>,
    invite_url: Option<String>,
    card_count: u32,
    has_password: bool,
    has_passkey: bool,
    /// An external login the extension knows about keeps the account
    /// reachable, so the password may be removed.
    has_external_login: bool,
    pw_saved: bool,
    pw_error: bool,
    /// Why the last delete-account attempt was refused (see
    /// password::settings_delete_account): confirm / password / admin.
    /// Empty = no attempt; the extension's own codes render in its badge.
    delete_error: String,
    /// The extension's cards and badges: plan & billing (replaces the
    /// plain "Your cards" card when non-empty), connected accounts, and
    /// the delete card's refusal badge and subscription note.
    plan_html: String,
    providers_html: String,
    delete_badge_html: String,
    delete_note_html: String,
}

struct SettingsFlags {
    /// Which form just saved: "limits" | "grading" | "".
    saved: String,
    name_saved: bool,
    name_error: bool,
    pw_saved: bool,
    pw_error: bool,
    delete_error: String,
}

/// `query` is the raw query string, handed to the extension's slots for
/// the flags its own redirects carry.
fn settings_page_data(
    s: &Services,
    app: &AppState,
    shell: Shell,
    user: UserId,
    flags: SettingsFlags,
    invite_url: Option<String>,
    query: &str,
) -> Result<SettingsPage, ServiceError> {
    let o = s.settings_overview(user, now_ms())?;
    let passkeys: Vec<PasskeyView> = o
        .passkeys
        .iter()
        .map(|p| PasskeyView {
            label: if p.label.is_empty() {
                "passkey".into()
            } else {
                p.label.clone()
            },
            created: format_day(p.created_at),
        })
        .collect();
    let has_passkey = !passkeys.is_empty();
    let apps: Vec<AppView> = s
        .connected_apps(user, now_ms())?
        .into_iter()
        .map(|g| {
            let first_party = g.client_id == crate::api::MOBILE_CLIENT_ID;
            AppView {
                name: if first_party {
                    "Flash app".to_string()
                } else if g.client_name.trim().is_empty() {
                    "Connector".to_string()
                } else {
                    g.client_name.clone()
                },
                client_id: g.client_id,
                first_party,
                tokens: g.tokens,
                since: format_day(g.first_granted_at),
                last_used: g.last_used_at.map(format_day).unwrap_or_default(),
            }
        })
        .collect();
    let has_external_login = app.ext.has_external_login(s.store(), user)?;
    Ok(SettingsPage {
        shell,
        display_name: o.display_name,
        name_saved: flags.name_saved,
        name_error: flags.name_error,
        grading_mode: o.grading_mode.as_str().to_string(),
        new_per_day: o.new_per_day,
        reviews_per_day: o.reviews_per_day,
        saved: flags.saved,
        passkeys,
        apps,
        invite_url,
        card_count: o.card_count,
        has_password: o.has_password,
        has_passkey,
        has_external_login,
        pw_saved: flags.pw_saved,
        pw_error: flags.pw_error,
        delete_error: flags.delete_error,
        plan_html: app.ext.render(app, Slot::SettingsPlan { user, query })?,
        providers_html: app
            .ext
            .render(app, Slot::SettingsProviders { user, query })?,
        delete_badge_html: app.ext.render(app, Slot::SettingsDeleteBadge { query })?,
        delete_note_html: app.ext.render(app, Slot::SettingsDeleteNote { user })?,
    })
}

pub fn format_day(ts_ms: i64) -> String {
    jiff::Timestamp::from_millisecond(ts_ms)
        .map(|t| {
            let z = t.to_zoned(jiff::tz::TimeZone::UTC);
            format!("{:04}-{:02}-{:02}", z.year(), z.month(), z.day())
        })
        .unwrap_or_default()
}

#[derive(Deserialize)]
struct SavedQuery {
    #[serde(default, deserialize_with = "crate::bounds::opt_keyword")]
    saved: Option<String>,
    name: Option<u8>,
    nameerr: Option<u8>,
    pw: Option<u8>,
    pwerr: Option<u8>,
    #[serde(default, deserialize_with = "crate::bounds::opt_keyword")]
    delerr: Option<String>,
}

async fn settings_page(
    State(state): State<AppState>,
    AuthUser { id: user, is_admin }: AuthUser,
    theme: Theme,
    Query(query): Query<SavedQuery>,
    RawQuery(raw_query): RawQuery,
) -> Response {
    let flags = SettingsFlags {
        saved: query
            .saved
            .filter(|v| matches!(v.as_str(), "limits" | "grading" | "apps"))
            .unwrap_or_default(),
        name_saved: query.name == Some(1),
        name_error: query.nameerr == Some(1),
        pw_saved: query.pw == Some(1),
        pw_error: query.pwerr == Some(1),
        delete_error: query
            .delerr
            .filter(|e| matches!(e.as_str(), "confirm" | "password" | "admin"))
            .unwrap_or_default(),
    };
    let shell = Shell::new(&state, theme, "settings", is_admin);
    account_event(&state, user, "view", Some("settings"), None);
    let app = state.clone();
    respond(state.services.clone(), move |s| {
        settings_page_data(
            &s,
            &app,
            shell,
            user,
            flags,
            None,
            raw_query.as_deref().unwrap_or_default(),
        )
    })
    .await
}

#[derive(Deserialize)]
struct DisconnectForm {
    #[serde(deserialize_with = "crate::bounds::token")]
    client_id: String,
}

/// Revokes every live grant of one connected app or connector; the card
/// re-renders without it.
async fn disconnect_app(
    State(state): State<AppState>,
    AuthUser { id: user, .. }: AuthUser,
    Form(form): Form<DisconnectForm>,
) -> Response {
    match act(state.services.clone(), move |s| {
        s.disconnect_app(user, &form.client_id, now_ms())
    })
    .await
    {
        Ok(()) => Redirect::to("/settings?saved=apps").into_response(),
        Err(response) => response,
    }
}

#[derive(Deserialize)]
struct NameForm {
    #[serde(deserialize_with = "crate::bounds::display_name")]
    display_name: String,
}

/// Same bounds as signup's name field.
async fn save_name(
    State(state): State<AppState>,
    AuthUser { id: user, .. }: AuthUser,
    Form(form): Form<NameForm>,
) -> Response {
    match act(state.services.clone(), move |s| {
        s.set_display_name(user, &form.display_name)
    })
    .await
    {
        Ok(()) => Redirect::to("/settings?name=1").into_response(),
        Err(response) if response.status() == StatusCode::BAD_REQUEST => {
            Redirect::to("/settings?nameerr=1").into_response()
        }
        Err(response) => response,
    }
}

#[derive(Deserialize)]
struct AppearanceForm {
    #[serde(deserialize_with = "crate::bounds::keyword")]
    theme: String,
}

/// The theme lives in a cookie, not the account, but only a signed-in
/// user has the form: a stranger gets no route that sets cookies.
async fn save_appearance(_user: AuthUser, Form(form): Form<AppearanceForm>) -> Response {
    let Some(theme) = Theme::from_str(&form.theme) else {
        return (StatusCode::BAD_REQUEST, "unknown theme").into_response();
    };
    let mut response = Redirect::to("/settings").into_response();
    if let Ok(value) =
        auth::cookie_header(auth::THEME_COOKIE, theme.as_str(), 365 * 24 * 60 * 60).parse()
    {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
    response
}

#[derive(Deserialize)]
struct GradingForm {
    #[serde(deserialize_with = "crate::bounds::keyword")]
    mode: String,
}

async fn save_grading(
    State(state): State<AppState>,
    AuthUser { id: user, .. }: AuthUser,
    Form(form): Form<GradingForm>,
) -> Response {
    match act(state.services.clone(), move |s| {
        s.set_grading_mode(user, &form.mode)
    })
    .await
    {
        Ok(()) => Redirect::to("/settings?saved=grading").into_response(),
        Err(response) => response,
    }
}

#[derive(Deserialize)]
struct SchedulingForm {
    new_per_day: u32,
    reviews_per_day: u32,
}

async fn save_scheduling(
    State(state): State<AppState>,
    AuthUser { id: user, .. }: AuthUser,
    Form(form): Form<SchedulingForm>,
) -> Response {
    match act(state.services.clone(), move |s| {
        s.set_daily_limits(user, form.new_per_day, form.reviews_per_day)
    })
    .await
    {
        Ok(()) => Redirect::to("/settings?saved=limits").into_response(),
        Err(response) => response,
    }
}

#[derive(Deserialize)]
struct InviteForm {
    #[serde(deserialize_with = "crate::bounds::display_name")]
    display_name: String,
}

async fn create_invite(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    theme: Theme,
    Form(form): Form<InviteForm>,
) -> Response {
    let base_url = state.config.base_url.clone();
    let shell = Shell::new(&state, theme, "settings", true);
    let app = state.clone();
    respond(state.services.clone(), move |s| {
        let (token, _) = s.create_member_invite(user, &form.display_name, now_ms())?;
        let url = format!("{base_url}/enroll/{token}");
        let flags = SettingsFlags {
            saved: String::new(),
            name_saved: false,
            name_error: false,
            pw_saved: false,
            pw_error: false,
            delete_error: String::new(),
        };
        settings_page_data(&s, &app, shell, user, flags, Some(url), "")
    })
    .await
}

// ---- import ----

struct ImportPreview {
    total: usize,
    /// What was uploaded, echoed back since a file input can't be re-filled.
    file_name: String,
    file_size: String,
    /// Prefill for the required destination field (may be empty).
    deck: String,
    /// Extra line when the file names several decks that will merge.
    deck_note: Option<String>,
    warnings: u32,
    sample: Vec<(String, String)>,
    messages: Vec<String>,
    token: String,
    /// Total Anki reviews found — >0 offers the bring-your-progress box.
    progress_reviews: usize,
    /// Human summary of importable Anki FSRS settings, when found.
    settings_desc: Option<String>,
    /// Cards whose deck colors were remapped to the Flash palette — >0
    /// offers the keep-colors/clean-look choice.
    colored_cards: usize,
}

#[derive(Template)]
#[template(path = "import.html")]
struct ImportPage {
    shell: Shell,
    preview: Option<ImportPreview>,
    imported: Option<usize>,
    usage_banner: String,
    usage_count: String,
}

/// The extension's usage fragments for the import page; empty on the
/// preview, so the file's own numbers are what's on screen.
fn import_usage(app: &AppState, user: UserId) -> Result<(String, String), ServiceError> {
    Ok((
        app.ext.render(
            app,
            Slot::UsageBanner {
                user,
                limit_hit: false,
            },
        )?,
        app.ext.render(app, Slot::UsageCount { user })?,
    ))
}

async fn import_page(
    State(state): State<AppState>,
    AuthUser { id: user, is_admin }: AuthUser,
    theme: Theme,
) -> Response {
    let shell = Shell::new(&state, theme, "decks", is_admin);
    let app = state.clone();
    respond(state.services.clone(), move |_| {
        let (usage_banner, usage_count) = import_usage(&app, user)?;
        Ok(ImportPage {
            shell,
            preview: None,
            imported: None,
            usage_banner,
            usage_count,
        })
    })
    .await
}

impl From<import_flow::ImportPreview> for ImportPreview {
    fn from(p: import_flow::ImportPreview) -> Self {
        ImportPreview {
            total: p.total,
            file_name: p.file_name,
            file_size: human_size(p.file_size_bytes),
            deck: p.deck,
            deck_note: p.deck_note,
            warnings: p.warnings,
            sample: p.sample,
            messages: p.messages,
            token: p.token,
            progress_reviews: p.progress_reviews,
            settings_desc: p.settings_desc,
            colored_cards: p.colored_cards,
        }
    }
}

/// "8.2 MB", "168 KB", "512 B".
fn human_size(bytes: usize) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    let b = bytes as f64;
    if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= 1024.0 {
        format!("{:.0} KB", b / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

async fn import_preview(
    State(state): State<AppState>,
    AuthUser { id: user, is_admin }: AuthUser,
    theme: Theme,
    mut multipart: Multipart,
) -> Response {
    let shell = Shell::new(&state, theme, "decks", is_admin);
    // Bound how many upload bodies are in memory before reading this one:
    // the body limit is per request, and a handful of concurrent 100 MB
    // uploads would otherwise exceed the whole process's budget.
    let Ok(_upload) = state.upload_semaphore.clone().try_acquire_owned() else {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            "too many uploads at once; try again in a moment",
        )
            .into_response();
    };
    let mut file: Option<(String, axum::body::Bytes)> = None;
    let mut deck = String::new();
    while let Ok(Some(field)) = multipart.next_field().await {
        match field.name() {
            Some("file") => {
                let name = field.file_name().unwrap_or("upload").to_string();
                match field.bytes().await {
                    Ok(bytes) => file = Some((name, bytes)),
                    // Includes the over-limit case: the body cap bites while
                    // streaming the field, so oversized uploads land here too.
                    Err(e) => {
                        tracing::warn!(user = user.raw(), file = ?name, "import upload failed: {e}");
                        return (e.status(), format!("upload: {e}")).into_response();
                    }
                }
            }
            Some("deck") => {
                deck = field.text().await.unwrap_or_default().trim().to_string();
                // The one text field of the form, bounded at the edge like
                // a request struct's field would be.
                if deck.len() > crate::bounds::DECK_NAME {
                    return (StatusCode::BAD_REQUEST, "deck name is too long").into_response();
                }
            }
            _ => {}
        }
    }
    let Some((filename, bytes)) = file else {
        tracing::warn!(user = user.raw(), "import rejected: no file in upload");
        return (StatusCode::BAD_REQUEST, "no file").into_response();
    };
    if filename.len() > crate::bounds::FILENAME {
        return (StatusCode::BAD_REQUEST, "the file's name is too long").into_response();
    }
    // Kept for the failure log after the closure takes ownership.
    let (log_file, log_bytes) = (filename.clone(), bytes.len());
    let Ok(_permit) = state.import_semaphore.clone().try_acquire_owned() else {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            "another import is running; try again in a moment",
        )
            .into_response();
    };
    let heavy = match state.heavy(user) {
        Ok(permit) => permit,
        Err(busy) => return busy.into_response(),
    };

    let dir = import_flow::dir(&state.config);
    let response = tokio::task::spawn_blocking(move || -> Result<ImportPage, Failure> {
        let preview = import_flow::preview(&heavy, &dir, &filename, &bytes, &deck)?;
        Ok(ImportPage {
            shell,
            preview: Some(preview.into()),
            imported: None,
            usage_banner: String::new(),
            usage_count: String::new(),
        })
    })
    .await;
    match response {
        Ok(Ok(page)) => {
            account_event(&state, user, "import_preview", None, None);
            Html(page.render().unwrap_or_default()).into_response()
        }
        Ok(Err(Failure::User(msg))) => {
            tracing::warn!(
                user = user.raw(),
                file = ?log_file,
                bytes = log_bytes,
                "import preview rejected: {msg}"
            );
            (StatusCode::BAD_REQUEST, msg).into_response()
        }
        Ok(Err(Failure::Internal(detail))) => {
            tracing::error!(
                user = user.raw(),
                file = ?log_file,
                "import preview failed: {detail}"
            );
            (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
        }
        Err(e) => {
            tracing::error!(user = user.raw(), file = ?log_file, "import join: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
        }
    }
}

#[derive(Deserialize)]
struct CommitForm {
    #[serde(deserialize_with = "crate::bounds::token")]
    token: String,
    /// Destination deck, as edited on the preview. Required.
    #[serde(default, deserialize_with = "crate::bounds::deck_name")]
    deck: String,
    /// "on" when the user chose to bring their Anki study progress.
    #[serde(default, deserialize_with = "crate::bounds::opt_keyword")]
    progress: Option<String>,
    /// "on" when the user chose to adopt the package's FSRS settings.
    #[serde(default, deserialize_with = "crate::bounds::opt_keyword")]
    settings: Option<String>,
    /// "keep" (default) leaves remapped deck colors; "clean" strips them.
    #[serde(default, deserialize_with = "crate::bounds::opt_keyword")]
    colors: Option<String>,
}

async fn import_commit(
    State(state): State<AppState>,
    AuthUser { id: user, is_admin }: AuthUser,
    theme: Theme,
    Form(form): Form<CommitForm>,
) -> Response {
    let Ok(_permit) = state.import_semaphore.clone().try_acquire_owned() else {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            "another import is running; try again in a moment",
        )
            .into_response();
    };
    let heavy = match state.heavy(user) {
        Ok(permit) => permit,
        Err(busy) => return busy.into_response(),
    };
    let dir = import_flow::dir(&state.config);
    let media_store = state.media.clone();
    let services = state.services.clone();
    let opts = import_flow::CommitOptions {
        token: form.token,
        deck: form.deck,
        progress: form.progress.is_some(),
        settings: form.settings.is_some(),
        clean_colors: form.colors.as_deref() == Some("clean"),
    };
    let app = state.clone();
    let result =
        tokio::task::spawn_blocking(move || -> Result<(usize, (String, String)), Failure> {
            let total = import_flow::commit(
                &heavy,
                &services,
                media_store.as_ref(),
                &dir,
                is_admin,
                &opts,
            )?;
            // Post-import usage so an oversized import immediately shows its
            // grace-period banner.
            let usage = import_usage(&app, user).unwrap_or_default();
            Ok((total, usage))
        })
        .await;
    match result {
        Ok(Ok((n, (usage_banner, usage_count)))) => {
            account_event(&state, user, "import_commit", None, Some(n as i64));
            Html(
                ImportPage {
                    shell: Shell::new(&state, theme, "decks", is_admin),
                    preview: None,
                    imported: Some(n),
                    usage_banner,
                    usage_count,
                }
                .render()
                .unwrap_or_default(),
            )
            .into_response()
        }
        Ok(Err(Failure::User(msg))) => {
            tracing::warn!(user = user.raw(), "import commit rejected: {msg}");
            (StatusCode::BAD_REQUEST, msg).into_response()
        }
        Ok(Err(Failure::Internal(detail))) => {
            tracing::error!(user = user.raw(), "import commit failed: {detail}");
            (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
        }
        Err(e) => {
            tracing::error!(user = user.raw(), "import commit join: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
        }
    }
}

// ---- export ----
// Never gated: "your cards are always yours" even over the free cap.

async fn export_apkg(
    State(state): State<AppState>,
    AuthUser { id: user, .. }: AuthUser,
) -> Response {
    let heavy = match state.heavy(user) {
        Ok(permit) => permit,
        Err(busy) => return busy.into_response(),
    };
    let media_store = state.media.clone();
    let config = state.config.clone();
    let now = now_ms();
    let result = act(state.services.clone(), move |s| {
        crate::flows::export_flow::apkg(&heavy, &s, media_store.as_ref(), &config, now)
    })
    .await;
    if result.is_ok() {
        account_event(&state, user, "export", Some("apkg"), None);
    }
    match result {
        Ok(export) => {
            crate::flows::export_flow::attachment_stream(
                export,
                "application/octet-stream",
                crate::flows::export_flow::file_name("apkg", now),
            )
            .await
        }
        Err(response) => response,
    }
}

async fn export_csv(
    State(state): State<AppState>,
    AuthUser { id: user, .. }: AuthUser,
) -> Response {
    let heavy = match state.heavy(user) {
        Ok(permit) => permit,
        Err(busy) => return busy.into_response(),
    };
    let now = now_ms();
    let result = act(state.services.clone(), move |s| {
        crate::flows::export_flow::csv(&heavy, &s)
    })
    .await;
    if result.is_ok() {
        account_event(&state, user, "export", Some("csv"), None);
    }
    match result {
        Ok(csv) => (
            [
                (header::CONTENT_TYPE, "text/csv; charset=utf-8".to_string()),
                (
                    header::CONTENT_DISPOSITION,
                    format!(
                        "attachment; filename=\"{}\"",
                        crate::flows::export_flow::file_name("csv", now)
                    ),
                ),
            ],
            csv,
        )
            .into_response(),
        Err(response) => response,
    }
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderMap, HeaderName, Method};

    use super::{bot_reason, clean_next};

    #[test]
    fn next_accepts_only_paths_this_origin_will_serve() {
        for good in ["/", "/decks", "/decks/3?limit=1", "/s/abc#top"] {
            assert_eq!(clean_next(Some(good.to_string())), good, "{good}");
        }
        // A percent-encoded control character is just a path byte on this
        // origin, so `/%0aevil` is fine; the raw characters are not.
        assert_eq!(clean_next(Some("/%0aevil".to_string())), "/%0aevil");
        for bad in [
            "//evil.example",
            "/\\evil.example",
            "/\t/evil.example",
            "/x\ny",
            "https://evil.example/",
            "javascript:alert(1)",
            "decks",
            "",
        ] {
            assert_eq!(clean_next(Some(bad.to_string())), "/", "{bad:?}");
        }
        assert_eq!(clean_next(Some("/".repeat(600))), "/");
        assert_eq!(clean_next(None), "/");
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        h
    }

    const BROWSER: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) Safari/605.1";

    #[test]
    fn a_browser_page_load_is_a_person() {
        let h = headers(&[
            ("user-agent", BROWSER),
            ("accept", "text/html,application/xhtml+xml,*/*;q=0.8"),
            ("sec-fetch-dest", "document"),
        ]);
        assert_eq!(bot_reason(&Method::GET, &h), None);
        // A bare client that says nothing about itself beyond a UA passes too.
        assert_eq!(
            bot_reason(&Method::GET, &headers(&[("user-agent", BROWSER)])),
            None
        );
    }

    #[test]
    fn crawlers_probes_and_prefetches_are_named() {
        type Case = (&'static str, Method, Vec<(&'static str, &'static str)>);
        let cases: [Case; 6] = [
            ("no_ua", Method::GET, vec![]),
            ("ua", Method::GET, vec![("user-agent", "Googlebot/2.1")]),
            ("head", Method::HEAD, vec![("user-agent", BROWSER)]),
            (
                "accept",
                Method::GET,
                vec![("user-agent", BROWSER), ("accept", "application/json")],
            ),
            (
                "prefetch",
                Method::GET,
                vec![
                    ("user-agent", BROWSER),
                    ("sec-purpose", "prefetch;prerender"),
                ],
            ),
            (
                "fetch",
                Method::GET,
                vec![("user-agent", BROWSER), ("sec-fetch-dest", "empty")],
            ),
        ];
        for (reason, method, pairs) in cases {
            assert_eq!(bot_reason(&method, &headers(&pairs)), Some(reason));
        }
    }
}
