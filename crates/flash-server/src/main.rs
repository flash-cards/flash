//! flash-server: the open, self-hosted Flash binary — web UI, JSON API,
//! MCP and OAuth surfaces composed over the shared Services layer, with
//! nothing plugged in. A downstream extension builds its own binary the
//! same way with its routers merged into each band.

use std::sync::Arc;

use axum::routing::get;
use flash_server::boot;
use flash_server::config::Config;
use flash_server::ext::CoreOnly;
use flash_server::service::{now_ms, Services};
use flash_server::state::AppState;
use flash_store::Store;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().with_target(false).init();

    let config = Config::from_env().expect("config");
    boot::warn_about_shared_buckets(&config);
    std::fs::create_dir_all(&config.data_dir).expect("create data dir");
    let store = Store::open(&config.db_path()).expect("open database");
    let services = Services::new(Arc::new(store));
    boot::bootstrap_invite(&services, &config.base_url);

    let bind = config.bind;
    let state = AppState::new(services, config, Arc::new(CoreOnly), now_ms()).expect("app state");

    tokio::spawn(flash_server::scheduler::run_housekeeping(
        state.services.clone(),
        state.media.clone(),
        state.config.data_dir.clone(),
    ));

    let app = flash_server::build_app(state.clone())
        .route("/healthz", get(boot::healthz).with_state(state));

    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .unwrap_or_else(|e| panic!("bind {bind}: {e}"));
    tracing::info!(%bind, "flash-server listening");
    // Peer addresses reach the rate limiter through ConnectInfo.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await
    .expect("serve");
}
