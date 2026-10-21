//! SSR web UI: page handlers and htmx partials. All domain work goes
//! through Services; this module only translates HTTP <-> service calls.

use std::sync::Arc;

use askama::Template;
use axum::extract::{DefaultBodyLimit, Multipart, Path, Query, RawQuery, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{delete, get, post};
use axum::{Form, Router};
use flash_core::queue::StudyScope;
use flash_core::{CardId, DeckId, Rating, SessionId, UserId};
use flash_store::notes::NoteType;
use flash_store::CardRow;
use serde::Deserialize;

use crate::auth::{self, hash_token, AdminUser, ApiUser, AuthUser, OptionalUser, Theme};
use crate::ext::{NavSection, Slot, Surface};
use crate::flows::{enroll, import_flow, Failure};
use crate::service::{
    now_ms, DayCount, DeckRow, DiffSpan, EditorSeed, HeatCell, MonthSpan, NoteInput, ReviewOrigin,
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
        .route("/decks/archived", post(set_show_archived))
        .route("/decks/{id}", get(deck_page))
        .route("/decks/{id}/cards", get(card_rows).post(create_card))
        .route("/decks/{id}/notes", post(create_note))
        .route("/cards/{id}/edit", get(card_editor).post(save_card))
        .route(
            "/media",
            post(crate::media::upload_media).layer(DefaultBodyLimit::max(MEDIA_UPLOAD_MAX_BYTES)),
        )
        // Card generation: the picker, the source it reads, the job the
        // runner takes, and the review of what it wrote. Every one of
        // them answers 404 on a server with no model configured.
        .route("/generate", get(crate::web_ai::generate_page))
        .route(
            "/decks/{id}/generate",
            get(crate::web_ai::deck_generate_page),
        )
        .route(
            "/ai/sources",
            // One file at the media cap plus multipart framing.
            post(crate::web_ai::create_source).layer(DefaultBodyLimit::max(MEDIA_UPLOAD_MAX_BYTES)),
        )
        .route("/ai/sources/{id}", delete(crate::web_ai::delete_source))
        .route("/ai/preflight", post(crate::web_ai::preflight_set))
        .route("/ai/jobs", post(crate::web_ai::create_job))
        .route("/ai/jobs/{id}", get(crate::web_ai::job_page))
        .route("/ai/jobs/{id}/review", get(crate::web_ai::review_page))
        .route("/ai/jobs/{id}/keep", post(crate::web_ai::keep_job))
        .route("/ai/jobs/{id}/discard", post(crate::web_ai::discard_job))
        .route("/ai/jobs/{id}/edit", post(crate::web_ai::edit_job))
        .route("/ai/jobs/{id}/thread", get(crate::web_ai::thread_partial))
        .route("/ai/drafts/{id}/keep", post(crate::web_ai::keep_draft))
        .route(
            "/ai/drafts/{id}/discard",
            post(crate::web_ai::discard_draft),
        )
        .route(
            "/ai/drafts/{id}/edit",
            get(crate::web_ai::draft_editor).post(crate::web_ai::save_draft),
        )
        .route("/decks/{id}/settings", get(deck_settings_page))
        .route("/decks/{id}/rename", post(rename_deck))
        .route("/decks/{id}/fold", post(fold_deck))
        .route("/decks/{id}/move", post(move_deck))
        .route("/decks/{id}/subdecks", post(create_subdeck))
        .route("/decks/{id}/limits", post(save_deck_limits))
        .route("/decks/{id}/boost", post(boost_deck))
        .route("/decks/{id}/archive", post(archive_deck))
        .route("/decks/{id}/unarchive", post(unarchive_deck))
        .route("/decks/{id}/delete", post(delete_deck))
        .route("/study/boost", post(boost_all))
        .route("/connect/dismiss", post(dismiss_connect_cta))
        .route("/cards/{id}", delete(delete_card))
        .route("/study", get(study_page))
        .route("/study/reveal", post(study_reveal))
        .route("/study/review", post(study_review))
        .route(
            "/study/turn",
            // A recording rides in the form as base64, past the default
            // limit once form encoding is counted.
            post(crate::web_voice::spoken_turn)
                .layer(DefaultBodyLimit::max(crate::bounds::VOICE_BODY_BYTES)),
        )
        .route(
            "/study/cards/{card}/audio",
            get(crate::web_voice::card_audio),
        )
        .route("/voice/phrases", get(crate::web_voice::phrases))
        .route(
            "/voice/phrases/{voice}/{key}/audio",
            get(crate::web_voice::phrase_audio),
        )
        .route(
            "/voice/answers/{token}/audio",
            get(crate::web_voice::answer_audio),
        )
        .route("/voice/voices", get(crate::web_voice::voices))
        .route("/stats", get(stats_page))
        .route("/users", get(users_page))
        .route("/settings", get(settings_page))
        .route("/settings/grading", post(save_grading))
        .route("/settings/voice", post(save_voice))
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
        .route("/static/speech.js", get(static_speech))
        .route("/static/glow.js", get(static_glow))
        .route("/static/voice.js", get(static_voice))
        .route("/static/cue-tick.wav", get(static_cue_tick))
        .route("/static/cue-done.wav", get(static_cue_done))
        .route("/static/ai.js", get(static_ai))
        .route("/static/favicon.svg", get(static_favicon))
        // The same tile as bitmaps: search engines and icon scrapers read
        // ICO/PNG, not SVG, and many try /favicon.ico before parsing
        // <link rel="icon">.
        .route("/static/favicon-192.png", get(static_favicon_png))
        .route("/static/apple-touch-icon.png", get(static_touch_icon))
        .route("/favicon.ico", get(static_favicon_ico))
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
    n.len() <= crate::bounds::NEXT_PATH
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
/// person. Those must not inflate view counters.
pub fn is_probably_bot(headers: &axum::http::HeaderMap) -> bool {
    bot_reason(&axum::http::Method::GET, headers).is_some()
}

/// The extension's anonymous product counter (event + ref only, never a
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
        // A generation this plan or this server will not run. The
        // sentence is the policy's own and is what the page shows; a
        // server with no model at all answers `AiOff`'s 404 instead,
        // before any service call is made.
        ServiceError::AiDenied { message, .. } => (StatusCode::FORBIDDEN, message).into_response(),
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

async fn static_favicon_ico() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "image/x-icon"),
            (header::CACHE_CONTROL, "max-age=86400"),
        ],
        include_bytes!("../static/favicon.ico").as_slice(),
    )
}

async fn static_favicon_png() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "image/png"),
            (header::CACHE_CONTROL, "max-age=86400"),
        ],
        include_bytes!("../static/favicon-192.png").as_slice(),
    )
}

async fn static_touch_icon() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "image/png"),
            (header::CACHE_CONTROL, "max-age=86400"),
        ],
        include_bytes!("../static/apple-touch-icon.png").as_slice(),
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

async fn static_voice() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "application/javascript"),
            (header::CACHE_CONTROL, "max-age=3600"),
        ],
        include_str!("../static/voice.js"),
    )
}

async fn static_speech() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "application/javascript"),
            (header::CACHE_CONTROL, "max-age=3600"),
        ],
        include_str!("../static/speech.js"),
    )
}

async fn static_glow() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "application/javascript"),
            (header::CACHE_CONTROL, "max-age=3600"),
        ],
        include_str!("../static/glow.js"),
    )
}

/// The two sound cues of voice study: a bell tap when the microphone
/// opens, a marimba pair when a rating posts. Both files are rendered by
/// `tools/cues/make-cues.mjs`; edit the recipe there, never the bytes.
async fn static_cue_tick() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "audio/wav"),
            (header::CACHE_CONTROL, "max-age=86400"),
        ],
        include_bytes!("../static/cue-tick.wav").as_slice(),
    )
}

async fn static_cue_done() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "audio/wav"),
            (header::CACHE_CONTROL, "max-age=86400"),
        ],
        include_bytes!("../static/cue-done.wav").as_slice(),
    )
}

async fn static_ai() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "application/javascript"),
            (header::CACHE_CONTROL, "max-age=3600"),
        ],
        include_str!("../static/ai.js"),
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
    decks: Vec<DeckListRow>,
    rows: DeckRowsCtx,
    /// The "Connect your AI" card, until the user dismisses it.
    show_connect_cta: bool,
}

/// `/` on the open server: members get the dashboard, everyone else the
/// login page.
async fn home(
    State(state): State<AppState>,
    OptionalUser(session): OptionalUser,
    theme: Theme,
    collapsed: auth::CollapsedDecks,
) -> Response {
    match session {
        Some((user, is_admin)) => dashboard_for(&state, user, is_admin, theme, collapsed).await,
        None => Redirect::to("/login").into_response(),
    }
}

/// The signed-in dashboard, shared by every owner of `/`.
pub async fn dashboard_for(
    state: &AppState,
    user: UserId,
    is_admin: bool,
    theme: Theme,
    collapsed: auth::CollapsedDecks,
) -> Response {
    let shell = Shell::new(state, theme, "today", is_admin);
    account_event(state, user, "view", Some("today"), None);
    let tts = state.tts.clone();
    respond(state.services.clone(), move |s| {
        let d = s.dashboard(user, now_ms())?;
        if let Some(tts) = &tts {
            crate::tts::prefetch_queue_head(tts, &s, user, now_ms());
        }
        let decks = deck_list_rows(d.decks, &collapsed, true);
        Ok(DashboardPage {
            shell,
            due_now: d.counts.due,
            new_available: d.counts.new_available,
            reviewed_today: d.counts.reviewed_today,
            boost_today: d.boost_today,
            rows: DeckRowsCtx::today(&decks),
            decks,
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
    /// The decks to list, in one order. An archived deck keeps its place
    /// among the others and is left out unless this browser asked to see
    /// them (`auth::ShowArchived`, set by the button in the page head,
    /// which is offered while `any_archived`).
    decks: Vec<DeckListRow>,
    rows: DeckRowsCtx,
    any_archived: bool,
    /// Whether any deck is not archived: when none is and archived decks
    /// are hidden, the page says so in place of an empty table.
    any_active: bool,
    show_archived: bool,
    /// Whether this server has a model behind it, so the page offers
    /// generation beside import.
    ai_enabled: bool,
}

#[derive(Deserialize)]
struct ShowArchivedForm {
    /// "1" to list archived decks, anything else to leave them out.
    #[serde(default, deserialize_with = "crate::bounds::keyword")]
    show: String,
}

/// "Show archived" and "Hide archived" on the deck list. The choice is
/// how this browser likes the page, so it goes in a cookie beside the
/// theme: a refresh keeps it, and it stays until it is turned off.
async fn set_show_archived(_user: AuthUser, Form(form): Form<ShowArchivedForm>) -> Response {
    let mut response = Redirect::to("/decks").into_response();
    let cookie = auth::pref_cookie_header(
        auth::SHOW_ARCHIVED_COOKIE,
        (form.show == "1").then_some("1"),
    );
    if let Ok(value) = cookie.parse() {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
    response
}

#[derive(Deserialize)]
struct FoldForm {
    /// "1" to fold the deck shut, anything else to open it.
    #[serde(default, deserialize_with = "crate::bounds::keyword")]
    collapsed: String,
    /// Where the row was (`DeckRowsCtx`), to come back to.
    #[serde(default, deserialize_with = "crate::bounds::opt_keyword")]
    back: Option<String>,
    #[serde(default)]
    from: Option<i64>,
}

/// The fold beside a deck that has subdecks. Every deck starts open; one
/// that is folded shut stays shut in this browser until it is opened
/// again, kept the way "Show archived" is. The ids kept are held to the
/// decks the caller has, so a deleted deck's fold is forgotten and a
/// stranger's id is never stored.
async fn fold_deck(
    State(state): State<AppState>,
    AuthUser { id: user, .. }: AuthUser,
    Path(id): Path<i64>,
    collapsed: auth::CollapsedDecks,
    Form(form): Form<FoldForm>,
) -> Response {
    let existing = match act(state.services.clone(), move |s| {
        Ok(s.list_decks(user, now_ms())?
            .into_iter()
            .map(|d| d.id.0)
            .collect::<Vec<i64>>())
    })
    .await
    {
        Ok(ids) => ids,
        Err(response) => return response,
    };
    let folded = collapsed.with(id, form.collapsed == "1", &existing);
    let mut response =
        Redirect::to(&rows_destination(form.back.as_deref(), form.from)).into_response();
    if let Ok(value) = folded.cookie().parse() {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
    response
}

async fn decks_page(
    State(state): State<AppState>,
    AuthUser { id: user, is_admin }: AuthUser,
    theme: Theme,
    auth::ShowArchived(show_archived): auth::ShowArchived,
    collapsed: auth::CollapsedDecks,
) -> Response {
    let shell = Shell::new(&state, theme, "decks", is_admin);
    let ai_enabled = state.ai.is_some();
    account_event(&state, user, "view", Some("decks"), None);
    respond(state.services.clone(), move |s| {
        let decks = s.list_decks(user, now_ms())?;
        let any_archived = decks.iter().any(|d| d.archived_at.is_some());
        let any_active = decks.iter().any(|d| d.archived_at.is_none());
        let decks = deck_list_rows(decks, &collapsed, show_archived);
        Ok(DecksPage {
            shell,
            rows: DeckRowsCtx::decks(&decks),
            decks,
            any_archived,
            any_active,
            show_archived,
            ai_enabled,
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
    /// "cloze 2", "reversed", "typed": how this card relates to its note.
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
/// `card_id` only ever reaches the markup as an element-id suffix, so a
/// generated draft seeds it with its own id and the same body renders.
pub(crate) struct EditorView {
    pub(crate) card_id: i64,
    pub(crate) note_type: &'static str,
    pub(crate) front_html: String,
    pub(crate) back_html: String,
    pub(crate) tags: String,
    pub(crate) sibling_count: usize,
    pub(crate) types: Vec<(&'static str, &'static str)>,
    pub(crate) hues: Vec<&'static str>,
}

impl EditorView {
    pub(crate) fn empty() -> Self {
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

    /// A generated draft in the same editor: no note behind it yet, so
    /// no siblings to warn about.
    pub(crate) fn from_draft(draft: &flash_store::AiDraftRow) -> Self {
        EditorView {
            card_id: draft.id.0,
            note_type: draft.note_type.as_str(),
            front_html: draft.front_html.clone(),
            back_html: draft.back_html.clone(),
            tags: draft.tags.join(", "),
            ..EditorView::empty()
        }
    }
}

#[derive(Deserialize)]
pub struct NoteForm {
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
    pub(crate) fn into_input(self) -> Result<NoteInput, ServiceError> {
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
        // Only a downstream card policy can refuse for size; the open
        // server never does.
        "limit" => "That card wasn't added: this account has reached its card limit.",
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

/// One row of a deck table (`deck_rows.html`): a deck as the lists show
/// it, with whether it has rows under it here and whether this browser
/// has folded it shut.
struct DeckListRow {
    id: i64,
    /// The whole path as a person reads it (`Bio › Cells`), for the hover.
    path: String,
    leaf: String,
    depth: u32,
    due_count: u32,
    new_count: u32,
    archived_at: Option<i64>,
    /// Whether the page shows this row as it loads. Every row is in the
    /// page, shown or not: showing archived decks and folding a deck are
    /// then a matter for the page's script, at once and with no request
    /// (`static/ui.js`, which works out the same three things from the
    /// rows' depth and state), and the page still loads as it was left.
    hidden: bool,
    /// Whether a deck sits under it at all, so it carries a fold; and
    /// whether one that is showing does, so the fold is offered. A deck
    /// whose only subdecks are archived and out of sight has one and does
    /// not offer it.
    foldable: bool,
    has_subdecks: bool,
    /// Whether this browser folded it shut, whatever is showing under it.
    collapsed: bool,
}

/// Where a deck table sits, which is where its rows' buttons (fold,
/// unarchive) come back to: `decks` is the deck list, which swaps itself
/// in place; `today` and `deck` (a deck's own page, `from` being that
/// deck) reload. Every page that includes `deck_rows.html` carries one as
/// `rows`.
struct DeckRowsCtx {
    back: &'static str,
    from: i64,
    /// Whether an archived row offers to unarchive. Today lists none.
    unarchive: bool,
    /// Whether any row has a fold, so the rows without one leave its
    /// width free and every name starts in the same column. A list with
    /// no subdecks in it looks as it always did.
    folds: bool,
}

impl DeckRowsCtx {
    fn decks(rows: &[DeckListRow]) -> Self {
        Self::at("decks", 0, true, rows)
    }
    fn today(rows: &[DeckListRow]) -> Self {
        Self::at("today", 0, false, rows)
    }
    fn deck(id: i64, rows: &[DeckListRow]) -> Self {
        Self::at("deck", id, true, rows)
    }
    fn at(back: &'static str, from: i64, unarchive: bool, rows: &[DeckListRow]) -> Self {
        Self {
            back,
            from,
            unarchive,
            folds: rows.iter().any(|r| r.foldable),
        }
    }
}

/// Where a row's button returns to, from the `back` and `from` its form
/// sent. `from` only names a page; one that is not the caller's is not
/// found when it gets there.
fn rows_destination(back: Option<&str>, from: Option<i64>) -> String {
    match (back, from) {
        (Some("today"), _) => "/".to_string(),
        (Some("deck"), Some(deck)) => format!("/decks/{deck}"),
        _ => "/decks".to_string(),
    }
}

/// A row for every one of `decks` (in tree order), each marked with how
/// the page shows it as it loads. Every deck starts open. An archived
/// deck is hidden unless `show_archived`, and so is what sits under a
/// deck this browser folded shut, whose counts already hold what it
/// hides.
fn deck_list_rows(
    decks: Vec<DeckRow>,
    collapsed: &auth::CollapsedDecks,
    show_archived: bool,
) -> Vec<DeckListRow> {
    // What the page may show: everything, or everything not archived.
    let showable: Vec<bool> = decks
        .iter()
        .map(|d| show_archived || d.archived_at.is_none())
        .collect();
    let depths: Vec<u32> = decks.iter().map(|d| d.depth).collect();
    let mut rows = Vec::with_capacity(decks.len());
    // While set, rows deeper than this are inside a folded deck.
    let mut folded_at: Option<u32> = None;
    for (i, deck) in decks.into_iter().enumerate() {
        let foldable = depths.get(i + 1).is_some_and(|next| *next > deck.depth);
        let next_showable = (i + 1..depths.len()).find(|j| showable[*j]);
        let has_subdecks = next_showable.is_some_and(|j| depths[j] > deck.depth);
        let is_collapsed = collapsed.contains(deck.id.0);
        // A fold ends at the next deck that is not under the folded one.
        if folded_at.is_some_and(|depth| deck.depth <= depth) {
            folded_at = None;
        }
        let inside_fold = folded_at.is_some();
        if showable[i] && !inside_fold && has_subdecks && is_collapsed {
            folded_at = Some(deck.depth);
        }
        rows.push(DeckListRow {
            id: deck.id.0,
            path: deck.path,
            leaf: deck.leaf,
            depth: deck.depth,
            due_count: deck.due_count,
            new_count: deck.new_count,
            archived_at: deck.archived_at,
            hidden: !showable[i] || inside_fold,
            foldable,
            has_subdecks,
            collapsed: is_collapsed,
        });
    }
    rows
}

/// One deck a deck sits in, as a link above its heading.
struct Crumb {
    id: i64,
    leaf: String,
}

/// A deck among the user's decks: the decks it sits in, itself, and the
/// decks under it. Both deck pages read the one list this way.
struct DeckFamily {
    crumbs: Vec<Crumb>,
    deck: DeckRow,
    /// In tree order, `depth` counted from the deck (its children are 0).
    subdecks: Vec<DeckRow>,
    /// Every other deck that is not under it: where it could move to.
    others: Vec<DeckRow>,
}

impl DeckFamily {
    fn of(decks: Vec<DeckRow>, id: DeckId) -> Result<Self, ServiceError> {
        let deck = decks
            .iter()
            .find(|d| d.id == id)
            .cloned()
            .ok_or(ServiceError::Store(flash_store::StoreError::NotFound(
                "deck",
            )))?;
        let mut crumbs = Vec::new();
        let mut up = deck.parent_id;
        while let Some(parent) = up.and_then(|p| decks.iter().find(|d| d.id == p)) {
            crumbs.push(Crumb {
                id: parent.id.0,
                leaf: parent.leaf.clone(),
            });
            up = parent.parent_id;
        }
        crumbs.reverse();
        let (subdecks, others): (Vec<DeckRow>, Vec<DeckRow>) = decks
            .into_iter()
            .filter(|d| d.id != id)
            .partition(|d| flash_core::deck_path::is_descendant(&d.name, &deck.name));
        let subdecks = subdecks
            .into_iter()
            .map(|mut d| {
                d.depth = d.depth.saturating_sub(deck.depth + 1);
                d
            })
            .collect();
        Ok(Self {
            crumbs,
            deck,
            subdecks,
            others,
        })
    }
}

#[derive(Template)]
#[template(path = "deck.html")]
struct DeckPage {
    shell: Shell,
    deck_id: i64,
    /// The whole path, for the page title.
    deck_name: String,
    /// The deck's own name, for the heading; the decks it sits in are the
    /// crumbs above it, outermost first.
    deck_leaf: String,
    crumbs: Vec<Crumb>,
    /// The decks under this one, in tree order, depth counted from here.
    decks: Vec<DeckListRow>,
    /// How many decks sit under this one, folded away or not: the count
    /// on the fold.
    subdeck_count: usize,
    rows: DeckRowsCtx,
    cards: Vec<CardView>,
    page: u32,
    pages: u32,
    total: u32,
    q: String,
    /// Why the last add was refused, if it was.
    editor_error: Option<&'static str>,
    editor: EditorView,
    /// Whether this server has a model behind it, so the page offers
    /// "Generate cards" beside "Study this deck".
    ai_enabled: bool,
    /// An archived deck offers "Unarchive" where "Study this deck" was.
    archived: bool,
}

#[derive(Deserialize)]
struct DeckPageQuery {
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
    collapsed: auth::CollapsedDecks,
) -> Response {
    let shell = Shell::new(&state, theme, "decks", is_admin);
    let editor_error = query.err.as_deref().and_then(editor_error_copy);
    let ai_enabled = state.ai.is_some();
    respond(state.services.clone(), move |s| {
        let deck = DeckId(id);
        let family = DeckFamily::of(s.list_decks(user, now_ms())?, deck)?;
        let summary = family.deck;
        let view = cards_view(&s, user, deck, None, query.page)?;
        let subdeck_count = family.subdecks.len();
        let decks = deck_list_rows(family.subdecks, &collapsed, true);
        Ok(DeckPage {
            shell,
            deck_id: id,
            deck_name: summary.path,
            deck_leaf: summary.leaf,
            crumbs: family.crumbs,
            subdeck_count,
            rows: DeckRowsCtx::deck(id, &decks),
            decks,
            cards: view.cards,
            page: view.page,
            pages: view.pages,
            total: view.total,
            q: view.q,
            editor_error,
            editor: EditorView::empty(),
            ai_enabled,
            archived: summary.archived_at.is_some(),
        })
    })
    .await
}

#[derive(Template)]
#[template(path = "deck_settings.html")]
struct DeckSettingsPage {
    shell: Shell,
    deck_id: i64,
    /// The whole path, for the page title and under the heading.
    deck_name: String,
    /// The deck's own name: what the rename field edits.
    deck_leaf: String,
    /// Where the deck could sit: every deck that is not under it. The one
    /// it sits in now is `selected`; none selected is the top level.
    parents: Vec<ParentOption>,
    /// Decks under it and live cards in the lot: what archiving takes
    /// along and what deleting removes.
    subdeck_count: u32,
    card_count: u32,
    limits: DeckLimitsView,
    /// Which form just saved: "name" | "parent" | "limits" | "archive" | "".
    saved: String,
    /// Why the last rename or move was refused, if it was.
    name_error: Option<&'static str>,
    parent_error: Option<&'static str>,
    /// The extension's sharing card.
    sharing_html: String,
    /// Whether the deck is archived, which flips the archive card.
    archived: bool,
}

#[derive(Deserialize)]
struct DeckSettingsQuery {
    #[serde(default, deserialize_with = "crate::bounds::opt_keyword")]
    saved: Option<String>,
    /// Why a rename was refused, and why a move was: each shows on its
    /// own card.
    #[serde(default, deserialize_with = "crate::bounds::opt_keyword")]
    err: Option<String>,
    #[serde(default, deserialize_with = "crate::bounds::opt_keyword")]
    moverr: Option<String>,
}

/// One entry of the "Parent deck" select, named by its whole path.
struct ParentOption {
    id: i64,
    name: String,
    selected: bool,
}

/// Short query-string code for a refused rename or move, so the settings
/// page can say why after the redirect.
fn deck_path_error_code(err: &ServiceError) -> Option<&'static str> {
    let (ServiceError::Invalid(m) | ServiceError::Store(flash_store::StoreError::Invalid(m))) = err
    else {
        return None;
    };
    Some(match m {
        m if m.contains("cannot contain") => "nest",
        m if m.contains("is empty") => "empty",
        m if m.contains("already exists") => "taken",
        m if m.contains("into itself") => "self",
        m if m.contains("longer than") => "long",
        m if m.contains("is archived") => "archived",
        m if m.contains("at most") => "cap",
        _ => "invalid",
    })
}

fn deck_path_error_copy(code: &str) -> Option<&'static str> {
    Some(match code {
        "nest" => "a name can't contain \"::\"; use Parent deck to nest this deck",
        "empty" => "the name can't be blank",
        "taken" => "you already have a deck with that name",
        "self" => "a deck can't move into itself",
        "long" => "that would make a deck's full name too long",
        "archived" => "that deck is archived; unarchive it first",
        "cap" => "this account has as many decks as it can hold",
        "invalid" => "that didn't work",
        _ => return None,
    })
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
        .filter(|v| matches!(v.as_str(), "name" | "parent" | "limits" | "archive"))
        .unwrap_or_default();
    let name_error = query.err.as_deref().and_then(deck_path_error_copy);
    let parent_error = query.moverr.as_deref().and_then(deck_path_error_copy);
    let app = state.clone();
    respond(state.services.clone(), move |s| {
        let deck = DeckId(id);
        let d = s.deck_detail(user, deck, now_ms())?;
        let family = DeckFamily::of(s.list_decks(user, now_ms())?, deck)?;
        let archived = d.deck.archived_at.is_some();
        // An active deck is never put inside an archived one, so an
        // archived deck is offered only to a deck that is archived too.
        let parents = family
            .others
            .into_iter()
            .filter(|p| archived || p.archived_at.is_none())
            .map(|p| ParentOption {
                id: p.id.0,
                selected: Some(p.id) == d.deck.parent_id,
                name: p.path,
            })
            .collect();
        Ok(DeckSettingsPage {
            shell,
            deck_id: id,
            archived,
            deck_leaf: d.deck.leaf,
            parents,
            subdeck_count: d.subdeck_count,
            card_count: d.card_count,
            parent_error,
            deck_name: d.deck.path,
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
    // The field holds the deck's own name; where the deck sits is the
    // Parent deck card's business.
    deck_path_change(&state, id, "name", "err", move |s| {
        s.rename_deck_leaf(user, DeckId(id), &form.name)
    })
    .await
}

#[derive(Deserialize)]
struct MoveDeckForm {
    /// The deck to move into; blank for the top level.
    #[serde(default, deserialize_with = "crate::bounds::keyword")]
    parent: String,
}

async fn move_deck(
    State(state): State<AppState>,
    AuthUser { id: user, .. }: AuthUser,
    Path(id): Path<i64>,
    Form(form): Form<MoveDeckForm>,
) -> Response {
    let parent = match form.parent.trim() {
        "" => None,
        raw => match raw.parse::<i64>() {
            Ok(parent) => Some(DeckId(parent)),
            Err(_) => return err_response(ServiceError::Invalid("no such deck".into())),
        },
    };
    deck_path_change(&state, id, "parent", "moverr", move |s| {
        s.move_deck(user, DeckId(id), parent)
    })
    .await
}

/// Runs a rename or a move and returns to the settings page: `saved` names
/// the card that shows "Saved", and a refusal the page has words for goes
/// back as a code under `err_key`, to show on that card.
async fn deck_path_change(
    state: &AppState,
    id: i64,
    saved: &'static str,
    err_key: &'static str,
    change: impl FnOnce(Services) -> Result<(), ServiceError> + Send + 'static,
) -> Response {
    let services = state.services.clone();
    match tokio::task::spawn_blocking(move || change(services)).await {
        Ok(Ok(())) => Redirect::to(&format!("/decks/{id}/settings?saved={saved}")).into_response(),
        Ok(Err(err)) => match deck_path_error_code(&err) {
            Some(code) => {
                Redirect::to(&format!("/decks/{id}/settings?{err_key}={code}")).into_response()
            }
            None => err_response(err),
        },
        Err(e) => {
            tracing::error!("join: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
        }
    }
}

#[derive(Deserialize)]
struct SubdeckForm {
    #[serde(deserialize_with = "crate::bounds::deck_name")]
    name: String,
}

/// "Add subdeck" on a deck's page: makes the deck inside it and opens it.
async fn create_subdeck(
    State(state): State<AppState>,
    AuthUser { id: user, .. }: AuthUser,
    Path(id): Path<i64>,
    Form(form): Form<SubdeckForm>,
) -> Response {
    match act(state.services.clone(), move |s| {
        s.create_subdeck(user, DeckId(id), &form.name, "", now_ms())
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

#[derive(Deserialize)]
struct ArchiveForm {
    /// Where the button lived: `decks` returns to the list (which shows
    /// its archived rows or not as the browser last asked), `deck` to the
    /// deck page; the settings page otherwise, with its saved note.
    #[serde(default, deserialize_with = "crate::bounds::opt_keyword")]
    back: Option<String>,
    /// With `deck`: the deck whose page the button was on, when that is
    /// not the deck acted on (a subdeck's row on its parent's page). It
    /// only names a page to go back to; a page that is not the caller's
    /// is not found when it gets there.
    #[serde(default)]
    from: Option<i64>,
}

impl ArchiveForm {
    fn destination(&self, id: i64) -> String {
        match self.back.as_deref() {
            Some("decks") => "/decks".to_string(),
            Some("deck") => format!("/decks/{}", self.from.unwrap_or(id)),
            _ => format!("/decks/{id}/settings?saved=archive"),
        }
    }
}

async fn archive_deck(
    State(state): State<AppState>,
    AuthUser { id: user, .. }: AuthUser,
    Path(id): Path<i64>,
    Form(form): Form<ArchiveForm>,
) -> Response {
    match act(state.services.clone(), move |s| {
        s.archive_deck(user, DeckId(id), now_ms())
    })
    .await
    {
        Ok(()) => Redirect::to(&form.destination(id)).into_response(),
        Err(response) => response,
    }
}

async fn unarchive_deck(
    State(state): State<AppState>,
    AuthUser { id: user, .. }: AuthUser,
    Path(id): Path<i64>,
    Form(form): Form<ArchiveForm>,
) -> Response {
    match act(state.services.clone(), move |s| {
        s.unarchive_deck(user, DeckId(id))
    })
    .await
    {
        Ok(()) => Redirect::to(&form.destination(id)).into_response(),
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
/// quick add, so the refusal banners ride the redirect.
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
            Err(ServiceError::OverCap { .. }) => Ok(Some("err=limit")),
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
            // Land back on the deck page with the inline refusal instead
            // of a bare error page.
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
        Ok(false) => Redirect::to(&format!("/decks/{id}?err=limit")).into_response(),
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
    /// The front read aloud, for the page's read-aloud control.
    spoken_front: String,
    wants_typing: bool,
    /// Images on this card's (still hidden) back: emitted as
    /// `<link rel="preload">` so they're cached before the reveal.
    preload: Vec<i64>,
    /// Images on the next card (both sides): `<link rel="prefetch">`.
    prefetch: Vec<i64>,
    /// The answer's words for the recognizer, as a JSON array in
    /// `data-answer-hints`; None once the back is on screen, or when
    /// there is nothing to prime with.
    hints_json: Option<String>,
}

struct RevealView {
    back: String,
    back_html: Option<String>,
    /// The back read aloud.
    spoken_back: String,
    /// Typed-answer comparison, char-aligned against the expected answer.
    typed_diff: Option<Vec<DiffSpan>>,
}

impl From<StudyCard> for StudyCardView {
    fn from(c: StudyCard) -> Self {
        StudyCardView {
            card_id: c.card_id.0,
            front: c.front,
            front_html: c.front_html,
            spoken_front: c.spoken_front,
            wants_typing: c.wants_typing,
            preload: c.preload_media,
            prefetch: c.prefetch_media,
            hints_json: if c.answer_hints.is_empty() {
                None
            } else {
                serde_json::to_string(&c.answer_hints).ok()
            },
        }
    }
}

/// The account's voice settings as the page's data attributes and the
/// settings dialog carry them.
struct VoicePrefsView {
    /// A sidecar is configured: the page may ask for clips.
    voice_on: bool,
    voice_id: String,
    /// One of the picker's steps, as its form value.
    speech_rate: String,
    voice_apply: &'static str,
    voice_cues: bool,
    voice_groups: Vec<VoiceGroupView>,
}

struct VoiceGroupView {
    label: &'static str,
    voices: Vec<VoiceOptionView>,
}

struct VoiceOptionView {
    id: &'static str,
    name: String,
    /// Where the page plays the voice's sample, once it is made.
    sample_url: Option<String>,
}

impl VoicePrefsView {
    fn new(state: &AppState, settings: &flash_core::VoiceSettings) -> Self {
        // The picker offers four steps; a rate saved by an older client
        // between two of them shows as the nearest.
        let steps = [("0.8", 0.8f32), ("1.0", 1.0), ("1.15", 1.15), ("1.3", 1.3)];
        let speech_rate = steps
            .iter()
            .min_by(|a, b| {
                (a.1 - settings.speech_rate)
                    .abs()
                    .total_cmp(&(b.1 - settings.speech_rate).abs())
            })
            .map(|(label, _)| *label)
            .unwrap_or("1.0")
            .to_string();
        // The picker only renders with a sidecar; without one the
        // groups are empty and the template says so instead.
        let listing = crate::tts::serve::voice_listing(state, |voice| {
            format!("/voice/phrases/{voice}/sample/audio")
        });
        VoicePrefsView {
            voice_on: listing.is_some(),
            voice_id: settings.voice_id.clone(),
            speech_rate,
            voice_apply: settings.apply.as_str(),
            voice_cues: settings.sound_cues,
            voice_groups: listing
                .map(|listing| {
                    listing
                        .groups
                        .into_iter()
                        .map(|g| VoiceGroupView {
                            label: g.label,
                            voices: g
                                .voices
                                .into_iter()
                                .map(|v| VoiceOptionView {
                                    id: v.id,
                                    name: v.name,
                                    sample_url: v.sample.map(|s| s.url),
                                })
                                .collect(),
                        })
                        .collect()
                })
                .unwrap_or_default(),
        }
    }
}

/// Where the page fetches a card side's clip.
fn card_audio_url(card_id: i64, side: &str) -> String {
    format!("/study/cards/{card_id}/audio?side={side}")
}

#[derive(Template)]
#[template(path = "study.html")]
struct StudyPage {
    shell: Shell,
    card: Option<StudyCardView>,
    revealed: Option<RevealView>,
    /// The card's clips in the account's voice, when made.
    audio_front: Option<String>,
    audio_back: Option<String>,
    session_id: i64,
    remaining: u32,
    total: u32,
    /// Eligible cards today's limits kept out of the queue. The empty
    /// state explains itself instead of a bare "nothing due" (same fix
    /// the MCP queue_note applies for assistants).
    held_by_limit: u32,
    /// The deck being studied, if the session is deck-scoped. The done
    /// state offers a boost for that deck rather than the account.
    scope_deck: Option<i64>,
    /// What the page's voice mode sends the grader (`transcript` or
    /// `audio`), when this server grades at all; None hides the mode.
    voice_input: Option<&'static str>,
    /// The user's voice grading setting: `self` means the page asks
    /// for a grade word instead of calling the grader.
    grading_mode: &'static str,
    /// How the account studies by voice; the page's data attributes
    /// and the settings dialog read it.
    voice_on: bool,
    voice_id: String,
    speech_rate: String,
    voice_apply: &'static str,
    voice_cues: bool,
    voice_groups: Vec<VoiceGroupView>,
}

#[derive(Template)]
#[template(path = "study_card.html")]
struct StudyCardPartial {
    card: Option<StudyCardView>,
    revealed: Option<RevealView>,
    /// The card's clips in the account's voice, when made.
    audio_front: Option<String>,
    audio_back: Option<String>,
    session_id: i64,
    remaining: u32,
    total: u32,
    held_by_limit: u32,
    /// The deck being studied, if the session is deck-scoped. The done
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
    let voice_input = state.ai.as_ref().map(|ai| ai.voice_input.as_str());
    let app = state.clone();
    respond(state.services.clone(), move |s| {
        let scope_deck = query.deck;
        let scope = match (query.deck, query.tag) {
            (Some(id), _) => StudyScope::Deck(DeckId(id)),
            (None, Some(tag)) => StudyScope::Tag(tag),
            _ => StudyScope::All,
        };
        let session = s.start_session(user, scope, now_ms())?;
        let tts = app.tts.as_deref();
        let card = session
            .first_card
            .map(|c| {
                let mut card = s.study_card(user, c, session.next_card_id)?;
                if let Some(tts) = tts {
                    crate::tts::prefetch_for_study(
                        tts,
                        &s,
                        user,
                        Some(card.card_id),
                        &session.upcoming,
                    );
                }
                crate::tts::serve::attach_study_card(tts, &s, user, &mut card);
                Ok::<_, ServiceError>(card)
            })
            .transpose()?;
        let audio_front = card
            .as_ref()
            .filter(|c| c.audio_front.is_some())
            .map(|c| card_audio_url(c.card_id.0, "front"));
        let prefs = VoicePrefsView::new(&app, &s.voice_settings(user)?);
        Ok(StudyPage {
            shell,
            card: card.map(Into::into),
            revealed: None,
            audio_front,
            audio_back: None,
            session_id: session.session_id.0,
            remaining: session.cards_due,
            total: session.cards_due,
            held_by_limit: session.held_by_limit,
            scope_deck,
            voice_input,
            grading_mode: session.grading_mode.as_str(),
            voice_on: prefs.voice_on,
            voice_id: prefs.voice_id,
            speech_rate: prefs.speech_rate,
            voice_apply: prefs.voice_apply,
            voice_cues: prefs.voice_cues,
            voice_groups: prefs.voice_groups,
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
    let app = state.clone();
    respond(state.services.clone(), move |s| {
        let mut reveal = s.reveal_card(user, CardId(form.card), form.typed.as_deref())?;
        crate::tts::serve::attach_reveal(app.tts.as_deref(), &s, user, &mut reveal);
        let audio_front = reveal
            .audio_front
            .as_ref()
            .map(|_| card_audio_url(reveal.card_id.0, "front"));
        let audio_back = reveal
            .audio_back
            .as_ref()
            .map(|_| card_audio_url(reveal.card_id.0, "back"));
        Ok(StudyCardPartial {
            audio_front,
            audio_back,
            card: Some(StudyCardView {
                card_id: reveal.card_id.0,
                front: reveal.front,
                front_html: reveal.front_html,
                spoken_front: reveal.spoken_front,
                wants_typing: false,  // already answered; reveal shows the back
                preload: Vec::new(),  // the back is on screen now
                prefetch: Vec::new(), // already hinted while the front showed
                hints_json: None,     // nothing is listened for as an answer
            }),
            revealed: Some(RevealView {
                back: reveal.back,
                back_html: reveal.back_html,
                spoken_back: reveal.spoken_back,
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
    let app = state.clone();
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
        let tts = app.tts.as_deref();
        let card = result
            .next_card
            .map(|c| {
                let mut card = s.study_card(user, c, result.next_card_id)?;
                if let Some(tts) = tts {
                    crate::tts::prefetch_for_study(
                        tts,
                        &s,
                        user,
                        Some(card.card_id),
                        &result.upcoming,
                    );
                }
                crate::tts::serve::attach_study_card(tts, &s, user, &mut card);
                Ok::<_, ServiceError>(card)
            })
            .transpose()?;
        let audio_front = card
            .as_ref()
            .filter(|c| c.audio_front.is_some())
            .map(|c| card_audio_url(c.card_id.0, "front"));
        Ok(StudyCardPartial {
            audio_front,
            audio_back: None,
            card: card.map(Into::into),
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
    /// The calendar year the heatmap shows; None is the past year.
    heat_year: Option<i32>,
    /// One link per year of history, oldest first, the trailing view
    /// last; empty when the history fits in one year.
    year_links: Vec<YearLink>,
}

struct YearLink {
    label: String,
    href: String,
    current: bool,
}

#[derive(Deserialize)]
struct StatsQuery {
    year: Option<i32>,
}

async fn stats_page(
    State(state): State<AppState>,
    AuthUser { id: user, is_admin }: AuthUser,
    theme: Theme,
    Query(query): Query<StatsQuery>,
) -> Response {
    let shell = Shell::new(&state, theme, "stats", is_admin);
    let heavy = match state.heavy(user) {
        Ok(permit) => permit,
        Err(busy) => return busy.into_response(),
    };
    respond(state.services.clone(), move |s| {
        let stats = s.stats(&heavy, now_ms(), query.year)?;
        let last = stats.heat_years.last().copied();
        let year_links = stats
            .heat_years
            .iter()
            .map(|&y| {
                if Some(y) == last {
                    YearLink {
                        label: "Past year".to_string(),
                        href: "/stats".to_string(),
                        current: stats.heat_year.is_none(),
                    }
                } else {
                    YearLink {
                        label: y.to_string(),
                        href: format!("/stats?year={y}"),
                        current: stats.heat_year == Some(y),
                    }
                }
            })
            .collect();
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
            heat_year: stats.heat_year,
            year_links,
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
    /// How the account studies by voice (the "Voice study" card).
    voice_on: bool,
    voice_id: String,
    speech_rate: String,
    voice_apply: &'static str,
    voice_cues: bool,
    voice_groups: Vec<VoiceGroupView>,
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
    let prefs = VoicePrefsView::new(app, &o.voice);
    Ok(SettingsPage {
        shell,
        display_name: o.display_name,
        name_saved: flags.name_saved,
        name_error: flags.name_error,
        grading_mode: o.grading_mode.as_str().to_string(),
        new_per_day: o.new_per_day,
        reviews_per_day: o.reviews_per_day,
        saved: flags.saved,
        voice_on: prefs.voice_on,
        voice_id: prefs.voice_id,
        speech_rate: prefs.speech_rate,
        voice_apply: prefs.voice_apply,
        voice_cues: prefs.voice_cues,
        voice_groups: prefs.voice_groups,
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
            .filter(|v| matches!(v.as_str(), "limits" | "grading" | "apps" | "voice"))
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

/// The voice study form, from the settings page or the study page's
/// dialog. The checkbox is absent when off.
#[derive(Deserialize)]
struct VoiceForm {
    #[serde(default, deserialize_with = "crate::bounds::opt_keyword")]
    voice_id: Option<String>,
    speech_rate: Option<f32>,
    #[serde(default, deserialize_with = "crate::bounds::opt_keyword")]
    voice_apply: Option<String>,
    #[serde(default, deserialize_with = "crate::bounds::opt_keyword")]
    sound_cues: Option<String>,
}

async fn save_voice(
    State(state): State<AppState>,
    AuthUser { id: user, .. }: AuthUser,
    headers: HeaderMap,
    Form(form): Form<VoiceForm>,
) -> Response {
    let patch = match crate::api::settings::voice_patch(
        form.voice_id,
        form.speech_rate,
        form.voice_apply,
        // A form always carries the whole card, so an absent checkbox
        // is "off".
        Some(form.sound_cues.is_some()),
    ) {
        Ok(patch) => patch,
        Err(message) => return (StatusCode::BAD_REQUEST, message).into_response(),
    };
    let tts = state.tts.clone();
    let from_dialog = headers.contains_key("hx-request");
    match act(state.services.clone(), move |s| {
        s.set_voice_settings(user, &patch)?;
        // A new voice starts with a cold cache: the card on screen and
        // the next few are queued in it now, ahead of anyone asking.
        if let (Some(tts), Some(_)) = (tts, &patch.voice_id) {
            crate::tts::prefetch_queue_head(&tts, &s, user, now_ms());
        }
        Ok(())
    })
    .await
    {
        Ok(()) if from_dialog => StatusCode::NO_CONTENT.into_response(),
        Ok(()) => Redirect::to("/settings?saved=voice").into_response(),
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
    /// A file that names several decks keeps them, so the field asks for
    /// an optional deck to put them under (prefilled with `parent`) and
    /// the page lists the first of them, `more_decks` standing for the
    /// rest.
    keeps_decks: bool,
    parent: String,
    file_decks: Vec<(String, usize)>,
    more_decks: usize,
    warnings: u32,
    sample: Vec<(String, String)>,
    messages: Vec<String>,
    token: String,
    /// Total Anki reviews found: >0 offers the bring-your-progress box.
    progress_reviews: usize,
    /// Human summary of importable Anki FSRS settings, when found.
    settings_desc: Option<String>,
    /// Cards whose deck colors were remapped to the Flash palette: >0
    /// offers the keep-colors/clean-look choice.
    colored_cards: usize,
}

#[derive(Template)]
#[template(path = "import.html")]
struct ImportPage {
    shell: Shell,
    preview: Option<ImportPreview>,
    imported: Option<usize>,
}

async fn import_page(
    State(state): State<AppState>,
    AuthUser { is_admin, .. }: AuthUser,
    theme: Theme,
) -> Response {
    let shell = Shell::new(&state, theme, "decks", is_admin);
    respond(state.services.clone(), move |_| {
        Ok(ImportPage {
            shell,
            preview: None,
            imported: None,
        })
    })
    .await
}

/// How many of a file's decks the preview names before "+N more".
const PREVIEW_DECKS: usize = 12;

impl From<import_flow::ImportPreview> for ImportPreview {
    fn from(p: import_flow::ImportPreview) -> Self {
        // Read as a person reads a path, not as the file spells it.
        let mut file_decks: Vec<(String, usize)> = p
            .file_decks
            .into_iter()
            .map(|(name, cards)| (flash_core::deck_path::display(&name), cards))
            .collect();
        let more_decks = file_decks.len().saturating_sub(PREVIEW_DECKS);
        let keeps_decks = file_decks.len() > 1;
        file_decks.truncate(PREVIEW_DECKS);
        ImportPreview {
            total: p.total,
            file_name: p.file_name,
            file_size: human_size(p.file_size_bytes),
            deck: p.deck,
            keeps_decks,
            parent: p.parent,
            file_decks,
            more_decks,
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
    /// The deck named on the preview: where the cards go, or, with
    /// `keep_decks`, the optional deck to put the file's decks under.
    #[serde(default, deserialize_with = "crate::bounds::deck_name")]
    deck: String,
    /// "on" from a preview of a file that names several decks.
    #[serde(default, deserialize_with = "crate::bounds::opt_keyword")]
    keep_decks: Option<String>,
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
        keep_decks: form.keep_decks.is_some(),
        progress: form.progress.is_some(),
        settings: form.settings.is_some(),
        clean_colors: form.colors.as_deref() == Some("clean"),
    };
    let result = tokio::task::spawn_blocking(move || -> Result<usize, Failure> {
        import_flow::commit(
            &heavy,
            &services,
            media_store.as_ref(),
            &dir,
            is_admin,
            &opts,
        )
    })
    .await;
    match result {
        Ok(Ok(n)) => {
            account_event(&state, user, "import_commit", None, Some(n as i64));
            Html(
                ImportPage {
                    shell: Shell::new(&state, theme, "decks", is_admin),
                    preview: None,
                    imported: Some(n),
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
