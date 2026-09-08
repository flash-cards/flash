//! Admin over the API: the users table and one-time member invites.
//! Everything is `BearerAdmin`-gated; the numbers are the same the web
//! pages render.

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use super::{call, ApiError, ApiJson, ApiResult, BearerAdmin};
use crate::service::{now_ms, ServiceError};
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/admin/users", get(users))
        .route("/admin/invites", post(create_invite))
}

#[derive(Serialize)]
struct AdminUserRow {
    id: i64,
    display_name: String,
    email: Option<String>,
    role: String,
    created_at: i64,
    deck_count: u32,
    card_count: u32,
    review_count: u32,
    last_review_at: Option<i64>,
    last_review_source: Option<String>,
    last_login_at: Option<i64>,
    mcp_clients: Vec<String>,
    /// The extension's per-user fields (the plan).
    #[serde(flatten)]
    extra: crate::ext::JsonFields,
}

#[derive(Serialize)]
struct Users {
    items: Vec<AdminUserRow>,
}

async fn users(State(state): State<AppState>, _admin: BearerAdmin) -> ApiResult<Users> {
    let app = state.clone();
    let items = call(state.services.clone(), move |s| {
        let mut items = Vec::new();
        for u in s.admin_users(now_ms())? {
            items.push(AdminUserRow {
                extra: app.ext.admin_user_json(&s, u.id)?,
                id: u.id.raw(),
                display_name: u.display_name,
                email: u.email,
                role: u.role,
                created_at: u.created_at,
                deck_count: u.deck_count,
                card_count: u.card_count,
                review_count: u.review_count,
                last_review_at: u.last_review_at,
                last_review_source: u.last_review_source,
                last_login_at: u.last_login_at,
                mcp_clients: u.mcp_clients,
            });
        }
        Ok::<_, ServiceError>(items)
    })
    .await?;
    Ok(Json(Users { items }))
}

#[derive(Deserialize)]
struct InviteBody {
    #[serde(deserialize_with = "crate::bounds::display_name")]
    display_name: String,
}

#[derive(Serialize)]
struct Invite {
    url: String,
    expires_at: i64,
}

async fn create_invite(
    State(state): State<AppState>,
    BearerAdmin(admin): BearerAdmin,
    ApiJson(body): ApiJson<InviteBody>,
) -> Result<(StatusCode, Json<Invite>), ApiError> {
    let base_url = state.config.base_url.clone();
    let (token, expires_at) = call(state.services.clone(), move |s| {
        s.create_member_invite(admin, &body.display_name, now_ms())
    })
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(Invite {
            url: format!("{base_url}/enroll/{token}"),
            expires_at,
        }),
    ))
}
