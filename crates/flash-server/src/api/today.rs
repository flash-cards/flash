//! Today: the dashboard, the app-icon badge, the account-wide boost, and
//! the "Connect your AI" card.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use flash_core::queue::StudyScope;
use serde::{Deserialize, Serialize};

use super::dto::Deck;
use super::{app_platform, call, ApiError, ApiJson, ApiResult, BearerUser};
use crate::ext::Surface;
use crate::service::now_ms;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/today", get(today))
        .route("/badge", get(badge))
        .route("/study/boost", post(boost))
        .route("/connect", get(connect))
        .route("/connect/dismiss", post(dismiss_connect_cta))
}

/// The guided-setup prompt the Connect page hands to Claude and ChatGPT.
const CONNECT_PROMPT: &str = "I want to connect Flash, my flashcard app, to you as a custom connector. Its MCP server URL is: {mcp}\n\nGuide me through adding it on this platform, one step at a time, waiting for me to say done after each step. Steps: open the connector settings for this app; add a custom connector; paste the URL above exactly; confirm; then I will be asked to sign in to Flash with my passkey to finish. After it connects, list my decks to prove it works.";

/// Everything the Connect page shows: the MCP URL, the one-click setup
/// links, and the copy boxes.
#[derive(Serialize)]
pub struct Connect {
    pub mcp_url: String,
    pub claude_url: String,
    pub claude_desktop_url: String,
    pub chatgpt_url: String,
    pub claude_code_cmd: String,
}

async fn connect(State(state): State<AppState>, user: BearerUser) -> ApiResult<Connect> {
    let mcp_url = format!("{}/mcp", state.config.base_url);
    let prompt = crate::auth::urlencode(&CONNECT_PROMPT.replace("{mcp}", &mcp_url));
    state.ext.account_event(
        &state.services,
        user.id,
        "view",
        Surface::Mobile,
        Some("connect"),
        None,
    );
    Ok(Json(Connect {
        claude_url: format!("https://claude.ai/new?q={prompt}"),
        claude_desktop_url: format!("claude://claude.ai/new?q={prompt}"),
        chatgpt_url: format!("https://chatgpt.com/?q={prompt}"),
        claude_code_cmd: format!("claude mcp add --transport http flash {mcp_url}"),
        mcp_url,
    }))
}

#[derive(Serialize)]
pub struct Today {
    pub due: u32,
    pub new_available: u32,
    pub reviewed_today: u32,
    /// Account-wide "more new cards today" already granted.
    pub boost_today: u32,
    /// What the app icon should show: cards due now.
    pub badge: u32,
    pub decks: Vec<Deck>,
    pub show_connect_cta: bool,
}

/// Opening the app lands here first, so this is where an app session is
/// counted (the extension collapses repeats within a sitting).
async fn today(
    State(state): State<AppState>,
    user: BearerUser,
    headers: HeaderMap,
) -> ApiResult<Today> {
    state.ext.account_event(
        &state.services,
        user.id,
        "app_session",
        Surface::Mobile,
        app_platform(&headers),
        None,
    );
    let d = call(state.services.clone(), move |s| {
        s.dashboard(user.id, now_ms())
    })
    .await?;
    Ok(Json(Today {
        due: d.counts.due,
        new_available: d.counts.new_available,
        reviewed_today: d.counts.reviewed_today,
        boost_today: d.boost_today,
        badge: d.counts.due,
        decks: d.decks.into_iter().map(Into::into).collect(),
        show_connect_cta: d.show_connect_cta,
    }))
}

/// The number the app puts on its icon: cards due right now, the same
/// figure the Today page and the daily push carry.
#[derive(Serialize)]
pub struct Badge {
    pub badge: u32,
}

async fn badge(State(state): State<AppState>, user: BearerUser) -> ApiResult<Badge> {
    let counts = call(state.services.clone(), move |s| {
        s.queue_counts(user.id, &StudyScope::All, now_ms())
    })
    .await?;
    Ok(Json(Badge { badge: counts.due }))
}

#[derive(Deserialize)]
pub struct BoostBody {
    pub extra: u32,
}

#[derive(Serialize)]
pub struct Boosted {
    /// The total boost now active for today.
    pub boost_today: u32,
}

async fn boost(
    State(state): State<AppState>,
    user: BearerUser,
    ApiJson(body): ApiJson<BoostBody>,
) -> ApiResult<Boosted> {
    let boost_today = call(state.services.clone(), move |s| {
        s.boost_new_today(user.id, None, body.extra, now_ms())
    })
    .await?;
    Ok(Json(Boosted { boost_today }))
}

async fn dismiss_connect_cta(
    State(state): State<AppState>,
    user: BearerUser,
) -> Result<StatusCode, ApiError> {
    call(state.services.clone(), move |s| {
        s.dismiss_connect_cta(user.id)
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}
