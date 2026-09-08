//! Single cards: read, plain-text update, delete, and the rich editor's
//! seed/save pair plus its static vocabulary.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use flash_core::CardId;
use flash_store::notes::NoteType;
use serde::{Deserialize, Serialize};

use super::decks::NoteBody;
use super::dto::{Card, EditorSeed};
use super::{call, ApiError, ApiJson, ApiResult, BearerUser};
use crate::service::{now_ms, ServiceError};
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/cards/{id}", get(card).put(update).delete(delete))
        .route("/cards/{id}/editor", get(editor_seed).put(save_editor))
        .route("/editor/meta", get(editor_meta))
}

async fn card(
    State(state): State<AppState>,
    user: BearerUser,
    Path(id): Path<i64>,
) -> ApiResult<Card> {
    let row = call(state.services.clone(), move |s| s.card(user.id, CardId(id))).await?;
    Ok(Json(row.into()))
}

#[derive(Deserialize)]
struct UpdateBody {
    #[serde(deserialize_with = "crate::bounds::card_side")]
    front: String,
    #[serde(deserialize_with = "crate::bounds::card_side")]
    back: String,
}

/// Plain-text rewrite (the MCP tool's shape); a rich card goes through
/// the editor endpoints instead.
async fn update(
    State(state): State<AppState>,
    user: BearerUser,
    Path(id): Path<i64>,
    ApiJson(body): ApiJson<UpdateBody>,
) -> Result<StatusCode, ApiError> {
    call(state.services.clone(), move |s| {
        s.update_card(user.id, CardId(id), &body.front, &body.back, now_ms())
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn delete(
    State(state): State<AppState>,
    user: BearerUser,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    call(state.services.clone(), move |s| {
        s.delete_card(user.id, CardId(id), now_ms())
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn editor_seed(
    State(state): State<AppState>,
    user: BearerUser,
    Path(id): Path<i64>,
) -> ApiResult<EditorSeed> {
    let seed = call(state.services.clone(), move |s| {
        s.editor_seed(user.id, CardId(id))
    })
    .await?;
    Ok(Json(seed.into()))
}

#[derive(Serialize)]
struct EditorSaved {
    note_id: i64,
    card_ids: Vec<i64>,
    /// What was actually stored after sanitizing — the app shows this,
    /// not what it sent.
    front_html: String,
    back_html: String,
}

async fn save_editor(
    State(state): State<AppState>,
    user: BearerUser,
    Path(id): Path<i64>,
    ApiJson(body): ApiJson<NoteBody>,
) -> ApiResult<EditorSaved> {
    let input = body.into_input()?;
    let saved = call(state.services.clone(), move |s| {
        let (note, ids) = s.save_card_editor(user.id, CardId(id), &input, now_ms())?;
        let seed = s.editor_seed(user.id, CardId(id))?;
        Ok::<_, ServiceError>(EditorSaved {
            note_id: note.0,
            card_ids: ids.into_iter().map(|c| c.0).collect(),
            front_html: seed.front_html,
            back_html: seed.back_html,
        })
    })
    .await?;
    Ok(Json(saved))
}

#[derive(Serialize)]
struct NoteTypeMeta {
    id: &'static str,
    label: &'static str,
}

#[derive(Serialize)]
struct EditorMeta {
    note_types: Vec<NoteTypeMeta>,
    /// The `hl-{hue}` / `hl-bg-{hue}` classes the sanitizer keeps.
    hues: &'static [&'static str],
}

/// Static vocabulary, but served only to a signed-in app: no route under
/// the API answers a stranger unless it signs one in.
async fn editor_meta(_user: BearerUser) -> Json<EditorMeta> {
    Json(EditorMeta {
        note_types: NoteType::ALL
            .iter()
            .map(|t| NoteTypeMeta {
                id: t.as_str(),
                label: t.label(),
            })
            .collect(),
        hues: flash_store::richtext::HIGHLIGHT_HUES,
    })
}
