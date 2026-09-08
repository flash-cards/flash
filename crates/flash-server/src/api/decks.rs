//! Decks: list, create, detail (settings), rename, limits, boost, delete,
//! the paged card list, quick-add, and the rich editor's create path.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use flash_core::DeckId;
use flash_store::notes::NoteType;
use serde::{Deserialize, Serialize};

use super::dto::{Card, Deck, DeckDetail};
use super::today::{BoostBody, Boosted};
use super::{call, ApiError, ApiJson, ApiResult, BearerUser, Page};
use crate::ext::Surface;
use crate::service::{now_ms, NoteInput};
use crate::state::AppState;

/// The web's page size; the app may ask for up to 100.
const DEFAULT_PER_PAGE: u32 = 25;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/decks", get(list).post(create))
        .route("/decks/{id}", get(detail).patch(rename).delete(delete_deck))
        .route("/decks/{id}/limits", axum::routing::put(set_limits))
        .route("/decks/{id}/boost", post(boost))
        .route("/decks/{id}/cards", get(cards).post(quick_add))
        .route("/decks/{id}/notes", post(create_note))
}

#[derive(Serialize)]
pub struct Decks {
    pub items: Vec<Deck>,
}

async fn list(State(state): State<AppState>, user: BearerUser) -> ApiResult<Decks> {
    let decks = call(state.services.clone(), move |s| {
        s.list_decks(user.id, now_ms())
    })
    .await?;
    Ok(Json(Decks {
        items: decks.into_iter().map(Into::into).collect(),
    }))
}

#[derive(Deserialize)]
struct NewDeckBody {
    #[serde(deserialize_with = "crate::bounds::deck_name")]
    name: String,
    #[serde(default, deserialize_with = "crate::bounds::deck_description")]
    description: String,
}

async fn create(
    State(state): State<AppState>,
    user: BearerUser,
    ApiJson(body): ApiJson<NewDeckBody>,
) -> Result<(StatusCode, Json<Deck>), ApiError> {
    let name = body.name.trim().to_string();
    if name.is_empty() {
        return Err(ApiError::invalid("the name can't be blank"));
    }
    let deck = call(state.services.clone(), move |s| {
        let now = now_ms();
        let id = s.create_deck(user.id, &name, body.description.trim(), now)?;
        s.deck_by_id(user.id, id, now)
    })
    .await?;
    state.ext.account_event(
        &state.services,
        user.id,
        "deck_created",
        Surface::Mobile,
        None,
        None,
    );
    Ok((StatusCode::CREATED, Json(deck.into())))
}

async fn detail(
    State(state): State<AppState>,
    user: BearerUser,
    Path(id): Path<i64>,
) -> ApiResult<DeckDetail> {
    let detail = call(state.services.clone(), move |s| {
        s.deck_detail(user.id, DeckId(id), now_ms())
    })
    .await?;
    Ok(Json(detail.into()))
}

#[derive(Deserialize)]
struct RenameBody {
    #[serde(deserialize_with = "crate::bounds::deck_name")]
    name: String,
}

async fn rename(
    State(state): State<AppState>,
    user: BearerUser,
    Path(id): Path<i64>,
    ApiJson(body): ApiJson<RenameBody>,
) -> ApiResult<Deck> {
    let name = body.name.trim().to_string();
    if name.is_empty() {
        return Err(ApiError::invalid("the name can't be blank"));
    }
    let deck = call(state.services.clone(), move |s| {
        s.rename_deck(user.id, DeckId(id), &name)?;
        s.deck_by_id(user.id, DeckId(id), now_ms())
    })
    .await?;
    Ok(Json(deck.into()))
}

/// Null (or absent) = inherit the account default.
#[derive(Deserialize)]
struct LimitsBody {
    #[serde(default)]
    new_per_day: Option<u32>,
    #[serde(default)]
    reviews_per_day: Option<u32>,
}

async fn set_limits(
    State(state): State<AppState>,
    user: BearerUser,
    Path(id): Path<i64>,
    ApiJson(body): ApiJson<LimitsBody>,
) -> Result<StatusCode, ApiError> {
    call(state.services.clone(), move |s| {
        s.set_deck_limits(user.id, DeckId(id), body.new_per_day, body.reviews_per_day)
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn boost(
    State(state): State<AppState>,
    user: BearerUser,
    Path(id): Path<i64>,
    ApiJson(body): ApiJson<BoostBody>,
) -> ApiResult<Boosted> {
    let boost_today = call(state.services.clone(), move |s| {
        s.boost_new_today(user.id, Some(DeckId(id)), body.extra, now_ms())
    })
    .await?;
    Ok(Json(Boosted { boost_today }))
}

#[derive(Serialize)]
struct DeckDeleted {
    cards_deleted: u32,
}

async fn delete_deck(
    State(state): State<AppState>,
    user: BearerUser,
    Path(id): Path<i64>,
) -> ApiResult<DeckDeleted> {
    let deleted = call(state.services.clone(), move |s| {
        s.delete_deck(user.id, DeckId(id))
    })
    .await?;
    crate::media::remove_orphans(
        state.services.clone(),
        state.media.clone(),
        &deleted.orphan_blobs,
    );
    Ok(Json(DeckDeleted {
        cards_deleted: deleted.cards,
    }))
}

#[derive(Deserialize)]
struct CardsQuery {
    #[serde(default, deserialize_with = "crate::bounds::opt_search")]
    q: Option<String>,
    page: Option<u32>,
    per_page: Option<u32>,
}

async fn cards(
    State(state): State<AppState>,
    user: BearerUser,
    Path(id): Path<i64>,
    Query(query): Query<CardsQuery>,
) -> ApiResult<Page<Card>> {
    let per_page = query.per_page.unwrap_or(DEFAULT_PER_PAGE).clamp(1, 100);
    let page = call(state.services.clone(), move |s| {
        s.deck_cards_page(
            user.id,
            DeckId(id),
            query.q.as_deref(),
            query.page.unwrap_or(1),
            per_page,
            now_ms(),
        )
    })
    .await?;
    Ok(Json(Page {
        items: page.rows.into_iter().map(Into::into).collect(),
        page: page.page,
        pages: page.pages,
        total: page.total,
    }))
}

#[derive(Deserialize)]
struct QuickAddBody {
    #[serde(deserialize_with = "crate::bounds::card_side")]
    front: String,
    #[serde(deserialize_with = "crate::bounds::card_side")]
    back: String,
    #[serde(default, deserialize_with = "crate::bounds::tags")]
    tags: Vec<String>,
}

#[derive(Serialize)]
pub struct CardsCreated {
    pub card_ids: Vec<i64>,
}

async fn quick_add(
    State(state): State<AppState>,
    user: BearerUser,
    Path(id): Path<i64>,
    ApiJson(body): ApiJson<QuickAddBody>,
) -> Result<(StatusCode, Json<CardsCreated>), ApiError> {
    let ids = call(state.services.clone(), move |s| {
        s.create_cards_in_deck(
            user.id,
            DeckId(id),
            &[(body.front, body.back, body.tags)],
            now_ms(),
        )
    })
    .await?;
    state.ext.account_event(
        &state.services,
        user.id,
        "cards_created",
        Surface::Mobile,
        Some("manual"),
        Some(ids.len() as i64),
    );
    Ok((
        StatusCode::CREATED,
        Json(CardsCreated {
            card_ids: ids.into_iter().map(|c| c.0).collect(),
        }),
    ))
}

/// The editor's submission, shared with `PUT /cards/{id}/editor`.
#[derive(Deserialize)]
pub struct NoteBody {
    #[serde(deserialize_with = "crate::bounds::keyword")]
    pub note_type: String,
    #[serde(default, deserialize_with = "crate::bounds::field_html")]
    pub front_html: String,
    #[serde(default, deserialize_with = "crate::bounds::field_html")]
    pub back_html: String,
    #[serde(default, deserialize_with = "crate::bounds::tags")]
    pub tags: Vec<String>,
}

impl NoteBody {
    pub fn into_input(self) -> Result<NoteInput, ApiError> {
        let note_type = NoteType::parse(&self.note_type)
            .ok_or_else(|| ApiError::invalid("unknown note type"))?;
        Ok(NoteInput {
            note_type,
            front_html: self.front_html,
            back_html: self.back_html,
            tags: self.tags,
        })
    }
}

#[derive(Serialize)]
pub struct NoteSaved {
    pub note_id: i64,
    pub card_ids: Vec<i64>,
}

async fn create_note(
    State(state): State<AppState>,
    user: BearerUser,
    Path(id): Path<i64>,
    ApiJson(body): ApiJson<NoteBody>,
) -> Result<(StatusCode, Json<NoteSaved>), ApiError> {
    let input = body.into_input()?;
    let (note, ids) = call(state.services.clone(), move |s| {
        s.create_note(user.id, DeckId(id), &input, now_ms())
    })
    .await?;
    state.ext.account_event(
        &state.services,
        user.id,
        "note_created",
        Surface::Mobile,
        None,
        None,
    );
    Ok((
        StatusCode::CREATED,
        Json(NoteSaved {
            note_id: note.0,
            card_ids: ids.into_iter().map(|c| c.0).collect(),
        }),
    ))
}
