//! Locast signaling server library.
//!
//! P0-T03 wires up an axum 0.7 router with three endpoints:
//! - `GET /health`  returns `200 OK` with `{"status":"ok"}`.
//! - `GET /version` returns `200 OK` with the crate version and name.
//! - `GET /metrics` returns `200 OK` with an empty Prometheus body
//!   (counters and gauges are added in P2+).
//!
//! The WebSocket endpoint, auth handshake, room registry, presence, and
//! rate limiting land in P2+. See `docs/ARCHITECTURE.md` section 26.3.

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

pub mod config;
pub mod metrics;

pub use config::Config;
pub use metrics::Metrics;

/// Library version string. Bumped per release alongside the workspace.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Library name. Used by the `/version` endpoint and downstream crates.
pub fn name() -> &'static str {
    "locast-server"
}

/// Shared application state held by every axum handler. The struct is
/// kept small for P0-T03; P2+ adds the room registry, the connection
/// counter, and the auth context.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub metrics: Metrics,
}

/// Build the axum router. Exposed so tests and integration harnesses can
/// mount the router on a `tokio::net::TcpListener` without going through
/// the binary entry point.
///
/// The router is built without a single stateful type. Handlers that do
/// not need shared state (health, version) take no extractor; the metrics
/// handler reads its own `Metrics` clone from the router extensions. This
/// keeps the handler signatures minimal and avoids axum 0.7's
/// `Handler<_, S>` bound issues.
pub fn router(state: AppState) -> Router {
    let metrics = state.metrics.clone();
    Router::new()
        .route("/health", get(health))
        .route("/version", get(version))
        .route("/metrics", get(metrics_handler))
        .layer(Extension(metrics))
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

/// Bind to `config.bind_addr` and serve until SIGINT / SIGTERM.
pub async fn serve(config: Config) -> Result<(), std::io::Error> {
    let state = AppState {
        config: Arc::new(config.clone()),
        metrics: Metrics::new(),
    };

    init_tracing(&config);

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
