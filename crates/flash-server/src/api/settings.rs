//! Settings and account security: the overview the settings screen
//! renders, each form's endpoint, login-method management, billing as
//! a read-only view, export, and self-serve deletion. Every rule is the
//! web's (`flows::account`, `Services`), reached through a JSON body.

use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use flash_core::UserId;
use serde::Deserialize;

use super::auth::{ceremony_error, Ceremony};
use super::dto::Settings;
use super::{call, ApiError, ApiJson, ApiResult, BearerUser};
use crate::ext::{AccountView, Surface};
use crate::flows::account::{self, DeleteRefusal, KeepSession, RemoveOutcome};
use crate::flows::export_flow;
use crate::password;
use crate::service::{now_ms, ServiceError, StudySettingsPatch};
use crate::state::AppState;
use crate::webauthn;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/settings", get(overview))
        .route("/settings/profile", axum::routing::patch(profile))
        .route("/settings/grading", put(grading))
        .route("/settings/theme", put(theme))
        .route("/settings/limits", put(limits))
        .route("/settings/study", put(study))
        .route(
            "/settings/password",
            post(set_or_change_password).delete(remove_password),
        )
        .route("/settings/passkeys/start", post(add_passkey_start))
        .route("/settings/passkeys/finish", post(add_passkey_finish))
        .route("/settings/passkeys/{id}", delete(remove_passkey))
        .route("/account", delete(delete_account))
        .route("/export/apkg", get(export_apkg))
        .route("/export/csv", get(export_csv))
}

async fn overview(State(state): State<AppState>, user: BearerUser) -> ApiResult<Settings> {
    let app = state.clone();
    let settings = call(state.services.clone(), move |s| {
        let mut settings = Settings::from_overview(s.settings_overview(user.id, now_ms())?);
        settings.extra = app
            .ext
            .account_json(&app, &s, user.id, AccountView::Settings)?;
        Ok::<_, ServiceError>(settings)
    })
    .await?;
    Ok(Json(settings))
}

// ---- simple forms ----

#[derive(Deserialize)]
struct ProfileBody {
    #[serde(deserialize_with = "crate::bounds::display_name")]
    display_name: String,
}

async fn profile(
    State(state): State<AppState>,
    user: BearerUser,
    ApiJson(body): ApiJson<ProfileBody>,
) -> Result<StatusCode, ApiError> {
    call(state.services.clone(), move |s| {
        s.set_display_name(user.id, &body.display_name)
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct GradingBody {
    #[serde(deserialize_with = "crate::bounds::keyword")]
    mode: String,
}

async fn grading(
    State(state): State<AppState>,
    user: BearerUser,
    ApiJson(body): ApiJson<GradingBody>,
) -> Result<StatusCode, ApiError> {
    call(state.services.clone(), move |s| {
        s.set_grading_mode(user.id, &body.mode)
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct ThemeBody {
    #[serde(deserialize_with = "crate::bounds::keyword")]
    theme: String,
}

async fn theme(
    State(state): State<AppState>,
    user: BearerUser,
    ApiJson(body): ApiJson<ThemeBody>,
) -> Result<StatusCode, ApiError> {
    call(state.services.clone(), move |s| {
        s.set_theme(user.id, &body.theme)
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct LimitsBody {
    new_per_day: u32,
    reviews_per_day: u32,
}

async fn limits(
    State(state): State<AppState>,
    user: BearerUser,
    ApiJson(body): ApiJson<LimitsBody>,
) -> Result<StatusCode, ApiError> {
    call(state.services.clone(), move |s| {
        s.set_daily_limits(user.id, body.new_per_day, body.reviews_per_day)
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct StudyBody {
    #[serde(default, deserialize_with = "crate::bounds::opt_timezone")]
    timezone: Option<String>,
    day_cutoff_hour: Option<u8>,
    desired_retention: Option<f32>,
}

async fn study(
    State(state): State<AppState>,
    user: BearerUser,
    ApiJson(body): ApiJson<StudyBody>,
) -> Result<StatusCode, ApiError> {
    let patch = StudySettingsPatch {
        timezone: body.timezone,
        day_cutoff_hour: body.day_cutoff_hour,
        desired_retention: body.desired_retention,
    };
    call(state.services.clone(), move |s| {
        s.set_study_settings(user.id, &patch)
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---- password ----

/// The current password didn't match: a distinct code so the app can
/// point at the right field, and not 401 (which would mean "sign in").
fn wrong_password() -> ApiError {
    ApiError::new(
        StatusCode::FORBIDDEN,
        "wrong_password",
        "that password is incorrect",
    )
}

/// Removing the only way into the account; shared with the extension's
/// provider unlink routes.
pub fn last_method() -> ApiError {
    ApiError::conflict(
        "last_login_method",
        "that's the only way to sign in — add another method first",
    )
}

/// Verifies the account's current password, feeding the same lockout
/// counters as login. `Ok(false)` when the account has no password.
async fn verify_current(state: &AppState, user: UserId, current: &str) -> Result<bool, ApiError> {
    let phc = call(state.services.clone(), move |s| {
        s.store().get_password_hash(user)
    })
    .await?;
    let Some(phc) = phc else {
        return Ok(false);
    };
    let outcome = password::verify_blocking(state, phc, current.to_string())
        .await
        .map_err(|_| ApiError::internal())?;
    let now = now_ms();
    if !outcome.ok {
        call(state.services.clone(), move |s| {
            s.store().record_password_failure(user, now)
        })
        .await?;
        return Err(wrong_password());
    }
    call(state.services.clone(), move |s| {
        s.store().clear_password_failures(user)
    })
    .await?;
    Ok(true)
}

#[derive(Deserialize)]
struct PasswordBody {
    #[serde(deserialize_with = "crate::bounds::password")]
    password: String,
    /// Required when the account already has a password.
    #[serde(default, deserialize_with = "crate::bounds::opt_password")]
    current: Option<String>,
}

async fn set_or_change_password(
    State(state): State<AppState>,
    user: BearerUser,
    ApiJson(body): ApiJson<PasswordBody>,
) -> Result<StatusCode, ApiError> {
    password::validate_new_password(&body.password).map_err(ApiError::invalid)?;
    let has_password = call(state.services.clone(), move |s| {
        Ok::<_, ApiError>(s.store().get_password_hash(user.id)?.is_some())
    })
    .await?;
    if has_password {
        let Some(current) = body.current.as_deref() else {
            return Err(ApiError::invalid("the current password is required"));
        };
        verify_current(&state, user.id, current).await?;
    }
    let phc = password::hash_blocking(&state, body.password)
        .await
        .map_err(|_| ApiError::internal())?;
    let keep = KeepSession {
        web_session_hash: None,
        api_token_id: Some(user.token_id),
    };
    call(state.services.clone(), move |s| {
        if has_password {
            account::change_password(s.store(), user.id, &phc, &keep, now_ms())
        } else {
            account::set_password(s.store(), user.id, &phc)
        }
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct CurrentBody {
    #[serde(deserialize_with = "crate::bounds::password")]
    current: String,
}

async fn remove_password(
    State(state): State<AppState>,
    user: BearerUser,
    ApiJson(body): ApiJson<CurrentBody>,
) -> Result<StatusCode, ApiError> {
    if !verify_current(&state, user.id, &body.current).await? {
        return Err(ApiError::invalid("no password is set"));
    }
    let ext = state.ext.clone();
    match call(state.services.clone(), move |s| {
        account::remove_password(s.store(), ext.as_ref(), user.id)
    })
    .await?
    {
        RemoveOutcome::Removed => Ok(StatusCode::NO_CONTENT),
        RemoveOutcome::LastMethod => Err(last_method()),
    }
}

async fn add_passkey_start(
    State(state): State<AppState>,
    user: BearerUser,
) -> ApiResult<Ceremony<webauthn_rs::prelude::CreationChallengeResponse>> {
    let (token, options) = webauthn::start_add_ceremony(&state, user.id)
        .await
        .map_err(ceremony_error)?;
    Ok(Json(Ceremony {
        ceremony_id: token,
        options,
    }))
}

#[derive(Deserialize)]
struct AddPasskeyBody {
    #[serde(deserialize_with = "crate::bounds::token")]
    ceremony_id: String,
    credential: webauthn_rs::prelude::RegisterPublicKeyCredential,
    /// Device name for the passkeys list ("FlashTester's iPhone").
    #[serde(default, deserialize_with = "crate::bounds::device_label")]
    label: String,
}

#[derive(serde::Serialize)]
struct PasskeyAdded {
    id: i64,
}

async fn add_passkey_finish(
    State(state): State<AppState>,
    user: BearerUser,
    ApiJson(body): ApiJson<AddPasskeyBody>,
) -> Result<(StatusCode, Json<PasskeyAdded>), ApiError> {
    let id = webauthn::finish_add_ceremony(
        &state,
        &crate::auth::hash_token(&body.ceremony_id),
        user.id,
        body.credential,
        &body.label,
    )
    .await
    .map_err(ceremony_error)?;
    Ok((StatusCode::CREATED, Json(PasskeyAdded { id })))
}

async fn remove_passkey(
    State(state): State<AppState>,
    user: BearerUser,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    let ext = state.ext.clone();
    match call(state.services.clone(), move |s| {
        account::remove_passkey(s.store(), ext.as_ref(), user.id, id)
    })
    .await?
    {
        Some(RemoveOutcome::Removed) => Ok(StatusCode::NO_CONTENT),
        Some(RemoveOutcome::LastMethod) => Err(last_method()),
        None => Err(ApiError::not_found("passkey")),
    }
}

// ---- delete account ----

#[derive(Deserialize)]
struct DeleteBody {
    #[serde(deserialize_with = "crate::bounds::keyword")]
    confirm: String,
    #[serde(default, deserialize_with = "crate::bounds::opt_password")]
    current: Option<String>,
}

async fn delete_account(
    State(state): State<AppState>,
    user: BearerUser,
    ApiJson(body): ApiJson<DeleteBody>,
) -> Result<StatusCode, ApiError> {
    if body.confirm.trim() != "DELETE" {
        return Err(ApiError::invalid("type DELETE exactly to confirm"));
    }
    let has_password = call(state.services.clone(), move |s| {
        Ok::<_, ApiError>(s.store().get_password_hash(user.id)?.is_some())
    })
    .await?;
    if has_password {
        verify_current(&state, user.id, body.current.as_deref().unwrap_or_default()).await?;
    }
    let app = state.clone();
    let outcome = call(state.services.clone(), move |_| {
        account::delete_account(&app, user.id, user.is_admin, now_ms())
    })
    .await?;
    match outcome {
        Ok(deleted) => {
            crate::media::remove_orphans(
                state.services.clone(),
                state.media.clone(),
                &deleted.orphan_blobs,
            );
            Ok(StatusCode::NO_CONTENT)
        }
        Err(refusal) => Err(match refusal {
            DeleteRefusal::LastAdmin => ApiError::conflict(
                "last_admin",
                "you're the only admin — make someone else admin first",
            ),
            DeleteRefusal::Ext { code, message } => ApiError::conflict(code, message),
        }),
    }
}

// ---- export ----

fn attachment(bytes: Vec<u8>, content_type: &'static str, name: String) -> Response {
    (
        [
            (header::CONTENT_TYPE, content_type.to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{name}\""),
            ),
        ],
        bytes,
    )
        .into_response()
}

async fn export_apkg(
    State(state): State<AppState>,
    user: BearerUser,
) -> Result<Response, ApiError> {
    let heavy = state.heavy(user.id)?;
    let media = state.media.clone();
    let config = state.config.clone();
    let now = now_ms();
    let export = call(state.services.clone(), move |s| {
        export_flow::apkg(&heavy, &s, media.as_ref(), &config, now)
    })
    .await?;
    state.ext.account_event(
        &state.services,
        user.id,
        "export",
        Surface::Mobile,
        Some("apkg"),
        None,
    );
    Ok(export_flow::attachment_stream(
        export,
        "application/octet-stream",
        export_flow::file_name("apkg", now),
    )
    .await)
}

async fn export_csv(State(state): State<AppState>, user: BearerUser) -> Result<Response, ApiError> {
    let heavy = state.heavy(user.id)?;
    let now = now_ms();
    let csv = call(state.services.clone(), move |s| {
        export_flow::csv(&heavy, &s)
    })
    .await?;
    state.ext.account_event(
        &state.services,
        user.id,
        "export",
        Surface::Mobile,
        Some("csv"),
        None,
    );
    Ok(attachment(
        csv.into_bytes(),
        "text/csv; charset=utf-8",
        export_flow::file_name("csv", now),
    ))
}
