//! Locast signaling server library.
//!
//! P0-T03 wires up an axum 0.7 router with three endpoints:
//! - `GET /health`  returns `200 OK` with `{"status":"ok"}`.
//! - `GET /version` returns `200 OK` with the crate version and name.
//! - `GET /metrics` returns `200 OK` with an empty Prometheus body
//!   (counters and gauges are added in P2+).
//!
//! P2-T02 adds the WebSocket endpoint, the auth handshake, the
//! SQLite-backed user/bearer store, and the per-connection rate
//! limiter. See `docs/ARCHITECTURE.md` section 26.3.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

use std::sync::Arc;

use axum::extract::Extension;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use tracing::info;

pub mod auth;
pub mod config;
pub mod db;
pub mod metrics;
pub mod rooms;
pub mod time;
pub mod ws;

pub mod test_support;

pub use config::Config;
pub use db::Db;
pub use metrics::Metrics;
pub use rooms::{RoomEvent, RoomRegistry, RoomRegistryConfig};
pub use time::{Clock, SystemClock};

/// Library version string. Bumped per release alongside the workspace.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Library name. Used by the `/version` endpoint and downstream crates.
pub fn name() -> &'static str {
    "locast-server"
}

/// Shared application state held by every axum handler.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub metrics: Metrics,
    pub db: Db,
    pub rooms: Arc<RoomRegistry>,
    pub clock: Arc<dyn Clock>,
}

/// Build the axum router. Exposed so tests and integration harnesses can
/// mount the router on a `tokio::net::TcpListener` without going through
/// the binary entry point.
pub fn router(state: AppState) -> Router {
    let metrics = state.metrics.clone();
    Router::new()
        .route("/health", get(health))
        .route("/version", get(version))
        .route("/metrics", get(metrics_handler))
        .route("/ws", get(ws::handler))
        .layer(Extension(metrics))
        .with_state(state)
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
}

async fn version() -> Json<VersionResponse> {
    Json(VersionResponse {
        name: name(),
        version: VERSION,
    })
}

async fn metrics_handler(Extension(metrics): Extension<Metrics>) -> Response {
    match metrics.render() {
        Ok((body, content_type)) => {
            (StatusCode::OK, [(header::CONTENT_TYPE, content_type)], body).into_response()
        }
        Err(err) => {
            tracing::error!(error = %err, "failed to render Prometheus metrics");
            (StatusCode::INTERNAL_SERVER_ERROR, "metrics unavailable").into_response()
        }
    }
}

#[derive(Debug, Serialize)]
struct HealthResponse {
    status: &'static str,
}

#[derive(Debug, Serialize)]
struct VersionResponse {
    name: &'static str,
    version: &'static str,
}

/// Initialize the global tracing subscriber from `Config::log_filter`.
pub fn init_tracing(config: &Config) {
    let filter = tracing_subscriber::EnvFilter::try_new(&config.log_filter)
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}

/// Bind to `config.bind_addr`, open the database, and serve until
/// SIGINT / SIGTERM.
pub async fn serve(config: Config) -> Result<(), std::io::Error> {
    init_tracing(&config);

    let db = Db::open(&config)
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))?;

    // Background cleanup of expired bearers every minute. The
    // task runs for the lifetime of the server.
    db::spawn_bearer_cleanup(db.clone(), std::time::Duration::from_secs(60));

    let rooms = Arc::new(RoomRegistry::new(RoomRegistryConfig::from_config(&config)));
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    // Background grace + stale-participant ticker.
    rooms::spawn_room_ticker(
        rooms.clone(),
        clock.clone(),
        std::time::Duration::from_millis(500),
    );

    let state = AppState {
        config: Arc::new(config.clone()),
        metrics: Metrics::new(),
        db,
        rooms,
        clock,
    };

    let app = router(state);
    let listener = tokio::net::TcpListener::bind(config.bind_addr).await?;
    let local = listener.local_addr().unwrap_or(config.bind_addr);
    info!(addr = %local, "locast-server listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler");
        sigterm.recv().await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
    info!("locast-server shutting down");
}
