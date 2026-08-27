//! Locast signaling server entry point.
//!
//! P0-T03: loads the configuration from the environment, initializes
//! tracing, and runs the axum server. SIGINT and SIGTERM trigger a
//! graceful shutdown. See `docs/ARCHITECTURE.md` section 26.3 and
//! `docs/ROADMAP.md` P0-T03.

use locast_server::{serve, Config};

#[tokio::main]
async fn main() {
    let config = Config::from_env().unwrap_or_else(|err| {
        eprintln!("locast-server: invalid configuration: {err}");
        std::process::exit(2);
    });

    if let Err(err) = serve(config).await {
        eprintln!("locast-server: fatal: {err}");
        std::process::exit(1);
    }
}
