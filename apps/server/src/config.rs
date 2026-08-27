//! Environment-driven configuration for the Locast signaling server.
//!
//! P0-T03 establishes the loader and the small set of variables the skeleton
//! needs (`LOCAST_BIND_ADDR`, `LOCAST_LOG`). P2-T02 adds the auth /
//! handshake / transport limits the WebSocket layer needs:
//! database URL, bearer TTL, challenge TTL, max frame bytes,
//! and the post-TCP-accept handshake deadline.

use std::env;
use std::net::SocketAddr;

/// Default bind address if `LOCAST_BIND_ADDR` is not set.
pub const DEFAULT_BIND_ADDR: &str = "0.0.0.0:8787";

/// Default database URL. `:memory:` is fine for the test harness;
/// production deployments use a file on disk via the Dockerfile
/// and compose file.
pub const DEFAULT_DATABASE_URL: &str = "sqlite::memory:";

/// Default bearer TTL. 15 minutes per `docs/ARCHITECTURE.md`
/// sections 20.4.4 and 21.3.
pub const DEFAULT_BEARER_TTL_SECONDS: i64 = 15 * 60;

/// Default CHALLENGE nonce TTL. 30 seconds per section 18.4.1.
pub const DEFAULT_CHALLENGE_TTL_MS: i64 = 30_000;

/// Default per-frame ceiling at the WS transport layer.
/// 1 MiB per section 18.5 and 20.6.
pub const DEFAULT_MAX_FRAME_BYTES: usize = 1_048_576;

/// Default post-TCP-accept deadline for completing the full
/// HELLO -> AUTH_OK sequence. After this elapses the server
/// closes the connection.
pub const DEFAULT_HANDSHAKE_TIMEOUT_MS: i64 = 15_000;

/// Default per-connection msg/s sustained rate (§18.6, §20.6).
pub const DEFAULT_RATE_MSGS_PER_SEC: u32 = 100;
/// Default per-connection msg burst budget (§18.6).
pub const DEFAULT_RATE_MSG_BURST: u32 = 200;
/// Default per-connection bytes/s sustained rate (§18.6, §20.6).
pub const DEFAULT_RATE_BYTES_PER_SEC: u32 = 1_000_000;
/// Default per-connection bytes burst budget.
pub const DEFAULT_RATE_BYTES_BURST: u32 = 2_000_000;

/// Runtime configuration resolved from environment variables.
#[derive(Debug, Clone)]
pub struct Config {
    pub bind_addr: SocketAddr,
    pub log_filter: String,
    pub database_url: String,
    pub bearer_ttl_seconds: i64,
    pub challenge_ttl_ms: i64,
    pub max_frame_bytes: usize,
    pub handshake_timeout_ms: i64,
    /// Per-connection msg/s sustained rate. Exposed so tests can
    /// pin a small rate and exercise the throttle logic
    /// deterministically.
    pub rate_msgs_per_sec: u32,
    /// Per-connection msg burst budget.
    pub rate_msg_burst: u32,
    /// Per-connection bytes/s sustained rate.
    pub rate_bytes_per_sec: u32,
    /// Per-connection bytes burst budget.
    pub rate_bytes_burst: u32,
}

impl Config {
    /// Load configuration from the process environment. Falls back to the
    /// defaults declared above when a variable is missing.
    pub fn from_env() -> Result<Self, ConfigError> {
        let bind_addr =
            env::var("LOCAST_BIND_ADDR").unwrap_or_else(|_| DEFAULT_BIND_ADDR.to_string());
        let bind_addr: SocketAddr = bind_addr
            .parse()
            .map_err(|e| ConfigError::InvalidBindAddr(bind_addr, e))?;

        let log_filter = env::var("LOCAST_LOG").unwrap_or_else(|_| "info".to_string());
        let database_url =
            env::var("LOCAST_DATABASE_URL").unwrap_or_else(|_| DEFAULT_DATABASE_URL.to_string());
        let bearer_ttl_seconds =
            parse_env_i64("LOCAST_BIND_TOKEN_TTL_SECONDS")?.unwrap_or(DEFAULT_BEARER_TTL_SECONDS);
        let challenge_ttl_ms =
            parse_env_i64("LOCAST_BIND_CHALLENGE_TTL_MS")?.unwrap_or(DEFAULT_CHALLENGE_TTL_MS);
        let max_frame_bytes =
            parse_env_usize("LOCAST_BIND_MAX_FRAME_BYTES")?.unwrap_or(DEFAULT_MAX_FRAME_BYTES);
        let handshake_timeout_ms = parse_env_i64("LOCAST_BIND_HANDSHAKE_TIMEOUT_MS")?
            .unwrap_or(DEFAULT_HANDSHAKE_TIMEOUT_MS);
        let rate_msgs_per_sec =
            parse_env_u32("LOCAST_RATE_MSGS_PER_SEC")?.unwrap_or(DEFAULT_RATE_MSGS_PER_SEC);
        let rate_msg_burst =
            parse_env_u32("LOCAST_RATE_MSG_BURST")?.unwrap_or(DEFAULT_RATE_MSG_BURST);
        let rate_bytes_per_sec =
            parse_env_u32("LOCAST_RATE_BYTES_PER_SEC")?.unwrap_or(DEFAULT_RATE_BYTES_PER_SEC);
        let rate_bytes_burst =
            parse_env_u32("LOCAST_RATE_BYTES_BURST")?.unwrap_or(DEFAULT_RATE_BYTES_BURST);

        Ok(Self {
            bind_addr,
            log_filter,
            database_url,
            bearer_ttl_seconds,
            challenge_ttl_ms,
            max_frame_bytes,
            handshake_timeout_ms,
            rate_msgs_per_sec,
            rate_msg_burst,
            rate_bytes_per_sec,
            rate_bytes_burst,
        })
    }
}

fn parse_env_i64(name: &str) -> Result<Option<i64>, ConfigError> {
    match env::var(name) {
        Ok(s) => s
            .parse::<i64>()
            .map(Some)
            .map_err(|e| ConfigError::InvalidNumber(name.to_string(), s, e.to_string())),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(e) => Err(ConfigError::Env(name.to_string(), e)),
    }
}

fn parse_env_usize(name: &str) -> Result<Option<usize>, ConfigError> {
    match env::var(name) {
        Ok(s) => s
            .parse::<usize>()
            .map(Some)
            .map_err(|e| ConfigError::InvalidNumber(name.to_string(), s, e.to_string())),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(e) => Err(ConfigError::Env(name.to_string(), e)),
    }
}

fn parse_env_u32(name: &str) -> Result<Option<u32>, ConfigError> {
    match env::var(name) {
        Ok(s) => s
            .parse::<u32>()
            .map(Some)
            .map_err(|e| ConfigError::InvalidNumber(name.to_string(), s, e.to_string())),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(e) => Err(ConfigError::Env(name.to_string(), e)),
    }
}

/// Errors raised while loading configuration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("LOCAST_BIND_ADDR={0} is not a valid socket address: {1}")]
    InvalidBindAddr(String, std::net::AddrParseError),

    #[error("environment variable {0} is not a valid number (got {1:?}): {2}")]
    InvalidNumber(String, String, String),

    #[error("failed to read environment variable {0}: {1}")]
    Env(String, env::VarError),
}
