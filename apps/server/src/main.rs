//! Locast signaling server entry point.
//!
//! P0-T03: loads the configuration from the environment, initializes
//! tracing, and runs the axum server. SIGINT and SIGTERM trigger a
//! graceful shutdown. See `docs/ARCHITECTURE.md` section 26.3 and
//! `docs/ROADMAP.md` P0-T03.

use locast_server::{serve, Config};

fn main() {
    // P8-T05: load the config before the tokio runtime exists.
    // `from_env` removes the `sensitive` variables from the process
    // environment, which must happen while this is the only thread.
    let config = Config::from_env().unwrap_or_else(|err| {
        eprintln!("locast-server: invalid configuration: {err}");
        std::process::exit(2);
    });

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap_or_else(|err| {
            eprintln!("locast-server: failed to start the async runtime: {err}");
            std::process::exit(1);
        });

    if let Err(err) = runtime.block_on(serve(config)) {
        eprintln!("locast-server: fatal: {err}");
        std::process::exit(1);
    }
}
