//! The study loop: start a session, reveal, grade, end. Every call is
//! the same `Services` method the web page and the MCP tools use; the
//! app never decides what comes next.

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::routing::post;
use axum::{Json, Router};
use flash_core::queue::StudyScope;
use flash_core::{CardId, DeckId, Rating, SessionId};
use serde::{Deserialize, Serialize};

use super::dto::{Reveal, SessionSummary, StudyCard};
use super::{app_platform, call, ApiError, ApiJson, ApiResult, BearerUser};
use crate::service::{now_ms, ReviewOrigin, ServiceError, Services};
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/study/sessions", post(start))
        .route("/study/sessions/{sid}/reveal", post(reveal))
        .route("/study/sessions/{sid}/reviews", post(review))
        .route("/study/sessions/{sid}/end", post(end))
}

/// What to study: everything, one deck, or one tag.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Scope {
    All,
    Deck {
        id: i64,
    },
    Tag {
        #[serde(deserialize_with = "crate::bounds::tag")]
        tag: String,
    },
}

impl From<Scope> for StudyScope {
    fn from(s: Scope) -> Self {
        match s {
            Scope::All => StudyScope::All,
            Scope::Deck { id } => StudyScope::Deck(DeckId(id)),
            Scope::Tag { tag } => StudyScope::Tag(tag),
        }
    }
}

#[derive(Deserialize)]
struct StartBody {
    #[serde(default = "default_scope")]
    scope: Scope,
}

fn default_scope() -> Scope {
    Scope::All
}

#[derive(Serialize)]
pub struct Session {
    pub session_id: i64,
    pub grading_mode: &'static str,
    /// Queue length at the start; the app keeps it for the progress bar.
    pub total: u32,
    pub remaining: u32,
    /// Eligible cards today's limits kept out of the queue.
    pub held_by_limit: u32,
    /// The deck when the session is deck-scoped, so an empty queue can
    /// offer that deck's boost.
    pub scope_deck: Option<i64>,
    pub card: Option<StudyCard>,
}

fn scope_deck(
    s: &Services,
    user: flash_core::UserId,
    session: SessionId,
) -> Result<Option<i64>, ServiceError> {
    Ok(match s.session_scope(user, session)? {
        StudyScope::Deck(d) => Some(d.0),
        _ => None,
    })
}

async fn start(
    State(state): State<AppState>,
    user: BearerUser,
    ApiJson(body): ApiJson<StartBody>,
) -> ApiResult<Session> {
    let scope: StudyScope = body.scope.into();
    let scope_deck = match scope {
        StudyScope::Deck(d) => Some(d.0),
        _ => None,
    };
    call(state.services.clone(), move |s| {
        let info = s.start_session(user.id, scope, now_ms())?;
        let card = info
            .first_card
            .map(|c| s.study_card(user.id, c, info.next_card_id))
            .transpose()?
            .map(Into::into);
        Ok::<_, ServiceError>(Session {
            session_id: info.session_id.0,
            grading_mode: info.grading_mode.as_str(),
            total: info.cards_due,
            remaining: info.cards_due,
            held_by_limit: info.held_by_limit,
            scope_deck,
            card,
        })
    })
    .await
    .map(Json)
}

#[derive(Deserialize)]
struct RevealBody {
    card_id: i64,
    /// What the user typed, when the card asks for it.
    #[serde(default, deserialize_with = "crate::bounds::opt_typed_answer")]
    typed: Option<String>,
}

async fn reveal(
    State(state): State<AppState>,
    user: BearerUser,
    Path(_sid): Path<i64>,
    ApiJson(body): ApiJson<RevealBody>,
) -> ApiResult<Reveal> {
    let reveal = call(state.services.clone(), move |s| {
        s.reveal_card(user.id, CardId(body.card_id), body.typed.as_deref())
    })
    .await?;
    Ok(Json(reveal.into()))
}

#[derive(Deserialize)]
struct ReviewBody {
    card_id: i64,
    rating: i64,
}

#[derive(Serialize)]
pub struct Reviewed {
    pub next_card: Option<StudyCard>,
    pub remaining: u32,
    pub held_by_limit: u32,
    pub scope_deck: Option<i64>,
}

async fn review(
    State(state): State<AppState>,
    user: BearerUser,
    Path(sid): Path<i64>,
    headers: HeaderMap,
    ApiJson(body): ApiJson<ReviewBody>,
) -> ApiResult<Reviewed> {
    let rating =
        Rating::from_i64(body.rating).ok_or_else(|| ApiError::invalid("rating must be 1-4"))?;
    let platform = app_platform(&headers);
    call(state.services.clone(), move |s| {
        let session = SessionId(sid);
        let result = s.submit_review(
            user.id,
            session,
            CardId(body.card_id),
            rating,
            ReviewOrigin {
                source: "mobile",
                client: platform,
            },
            now_ms(),
        )?;
        let next_card = result
            .next_card
            .map(|c| s.study_card(user.id, c, result.next_card_id))
            .transpose()?
            .map(Into::into);
        Ok::<_, ServiceError>(Reviewed {
            next_card,
            remaining: result.remaining,
            held_by_limit: result.held_by_limit,
            scope_deck: scope_deck(&s, user.id, session)?,
        })
    })
    .await
    .map(Json)
}

async fn end(
    State(state): State<AppState>,
    user: BearerUser,
    Path(sid): Path<i64>,
) -> ApiResult<SessionSummary> {
    let stats = call(state.services.clone(), move |s| {
        s.end_session(user.id, SessionId(sid), now_ms())
    })
    .await?;
    Ok(Json(stats.into()))
}
