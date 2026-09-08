//! A panic in one request is that request's 500 and nothing more: the
//! connection is answered (not dropped), the reply carries the normal
//! security headers, the store's lock is not poisoned by a panic taken
//! while it was held, and the very next request is served normally.
//! Together with lock_hygiene_tests (no poisonable lock exists in the
//! process) this is what lets the release profile unwind instead of
//! abort without turning any reachable panic into an outage.

mod common;

use std::any::Any;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::routing::get;
use axum::Router;
use common::*;
use flash_server::ext::{AccountHooks, ServerExtension};
use flash_server::state::AppState;

/// An extension that mounts two routes nobody would ship: one that
/// panics in the handler, one that panics while holding the store's
/// connection lock inside the blocking pool.
struct Panicky;

impl AccountHooks for Panicky {}

impl ServerExtension for Panicky {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn browser_routes(&self, _state: &AppState) -> Router<AppState> {
        Router::new()
            .route("/__panic", get(panic_in_handler))
            .route("/__panic-locked", get(panic_under_the_lock))
    }
}

async fn panic_in_handler() -> &'static str {
    panic!("handler panic")
}

async fn panic_under_the_lock(State(state): State<AppState>) -> &'static str {
    let services = state.services.clone();
    let joined = tokio::task::spawn_blocking(move || {
        services
            .store()
            .with_conn(|_| -> flash_store::Result<()> { panic!("under the lock") })
    })
    .await;
    // The blocking task's panic surfaces as a join error; re-raise it so
    // this handler panics too.
    joined.expect("blocking task panicked").expect("store");
    "unreachable"
}

fn get_path(path: &str) -> Request<Body> {
    Request::builder().uri(path).body(Body::empty()).unwrap()
}

#[tokio::test]
async fn a_panicking_handler_answers_500_with_headers_and_the_next_request_is_fine() {
    let h = AppBuilder::new("panic").ext(Arc::new(Panicky)).build();

    let reply = send(&h.app, get_path("/__panic")).await;
    assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(reply.text(), "internal error");
    assert!(
        reply.header("content-security-policy").is_some(),
        "the 500 is a normal reply with the security headers"
    );
    assert_eq!(reply.header("x-content-type-options"), Some("nosniff"));

    let reply = send(&h.app, get_path("/login")).await;
    assert_eq!(reply.status, StatusCode::OK, "the next request is served");
}

#[tokio::test]
async fn a_panic_under_the_store_lock_does_not_take_the_store_down() {
    let h = AppBuilder::new("panic-locked")
        .ext(Arc::new(Panicky))
        .build();

    let reply = send(&h.app, get_path("/__panic-locked")).await;
    assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);

    h.store.health_check().expect("the store still answers");
    let (_, cookie) = signed_in(&h.store, "Ada", "ada@example.com");
    let reply = send(
        &h.app,
        Request::builder()
            .uri("/decks")
            .header("cookie", cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "a store-backed page renders");
}
