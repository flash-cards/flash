//! Server library: composition of config, services, and (in later phases)
//! web, OAuth, and MCP surfaces. The binary in main.rs stays thin.

pub mod api;
pub mod auth;
pub mod boot;
pub mod bounds;
pub mod captcha;
pub mod charts;
pub mod config;
pub mod email;
pub mod ext;
pub mod flows;
pub mod http;
pub mod mcp;
pub mod media;
pub mod media_store;
pub mod middleware;
pub mod oauth;
pub mod password;
pub mod policy;
pub mod scheduler;
pub mod service;
pub mod state;
#[cfg(feature = "test-support")]
pub mod testing;
pub mod web;
pub mod webauthn;

use axum::Router;
use state::AppState;

/// The complete application router: web UI + auth ceremonies (same-origin
/// guarded), OAuth + MCP (token-authenticated), all under security
/// headers, with the extension's routers merged into each band.
pub fn build_app(state: AppState) -> Router {
    let ext = state.ext.clone();
    let rate_limit =
        axum::middleware::from_fn_with_state(state.clone(), middleware::auth_rate_limit);
    let public_host = state
        .config
        .base_url
        .split("//")
        .nth(1)
        .unwrap_or("localhost:8437")
        .to_string();
    let mcp_routes = Router::new()
        .nest_service(
            "/mcp",
            mcp::mcp_service(
                state.services.clone(),
                state.media.clone(),
                state.ext.clone(),
                public_host,
            ),
        )
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            oauth::require_bearer,
        ))
        // The body limit is the transport's own (`mcp::MCP_BODY_LIMIT`,
        // set on its config); the API's per-IP budget applies here.
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            middleware::mcp_rate_limit,
        ));
    Router::new()
        // Browser surfaces: same-origin mutation guard (CSRF), and the
        // per-address budget on every mutation (rate_limit_inventory_tests
        // probes each one).
        .merge(
            ext.site_routes(&state)
                .unwrap_or_else(web::site_routes)
                .merge(web::router())
                .merge(ext.browser_routes(&state))
                .layer(axum::middleware::from_fn_with_state(
                    state.clone(),
                    middleware::web_mutation_rate_limit,
                )),
        )
        .merge(
            web::auth_router()
                .merge(oauth::browser_router())
                .merge(ext.auth_routes(&state))
                .layer(rate_limit.clone()),
        )
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            middleware::same_origin_mutations,
        ))
        // Protocol surfaces: token-authenticated, no Origin requirement.
        .merge(oauth::router().layer(rate_limit))
        .merge(ext.protocol_routes(&state))
        .merge(mcp_routes)
        // Bearer-authenticated JSON with its own limiter, and whatever
        // the extension serves at the site root for the platforms.
        .merge(api::router(&state))
        .merge(ext.well_known_routes(&state))
        .fallback(web::not_found)
        // A panic anywhere in a request is a logged 500 for that request,
        // never a dropped connection, and (with no poisonable lock in
        // the process, see lock_hygiene_tests) never anything for the
        // next request. Inside the header layer so the reply is still a
        // normal reply.
        .layer(tower_http::catch_panic::CatchPanicLayer::custom(
            middleware::panic_response,
        ))
        // Before routing: every path segment and query string a handler
        // will see is bounded here (bounded_input_tests).
        .layer(axum::middleware::from_fn(middleware::uri_length))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            middleware::security_headers,
        ))
        // No request lives forever. hyper's own header-read timeout is
        // inert without a timer, so this is the only bound on a client
        // that connects and trickles; it is long enough for a 100 MB
        // upload on a slow link and answers 408 past that.
        .layer(tower_http::timeout::TimeoutLayer::with_status_code(
            axum::http::StatusCode::REQUEST_TIMEOUT,
            REQUEST_TIMEOUT,
        ))
        .with_state(state)
}

/// Upper bound on one request, body included.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);
