//! Media over the API: the same logic the web uses, reachable with a
//! bearer and only a bearer — `/api/v1` sits outside the same-origin
//! guard, so nothing under it may accept a session cookie. Uploads answer
//! the editor's `{id, kind}` JSON; blobs stream with Range support so
//! audio and video seek.

use axum::extract::DefaultBodyLimit;
use axum::routing::{get, post};
use axum::Router;

use crate::media::{media_location, serve_media_bearer, upload_media_bearer};
use crate::state::AppState;
use crate::web::MEDIA_UPLOAD_MAX_BYTES;

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/media",
            post(upload_media_bearer).layer(DefaultBodyLimit::max(MEDIA_UPLOAD_MAX_BYTES)),
        )
        .route("/media/{id}", get(serve_media_bearer))
        // Where to fetch the bytes from directly, when the store offers a
        // URL; the app downloads it with no headers.
        .route("/media/{id}/url", get(media_location))
}
