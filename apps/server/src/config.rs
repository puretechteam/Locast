//! Environment-driven configuration for the Locast signaling server.
//!
//! P0-T03 establishes the loader and the small set of variables the skeleton
//! needs (`LOCAST_BIND_ADDR`, `LOCAST_LOG`). The full set - including
//! database URLs, TURN credential TTLs, rate-limit thresholds, and audit-log
//! destinations - lands with P2-T02 and the deployment work in P9.

use std::env;
use std::net::SocketAddr;

/// Default bind address if `LOCAST_BIND_ADDR` is not set.
pub const DEFAULT_BIND_ADDR: &str = "0.0.0.0:8787";

/// Runtime configuration resolved from environment variables.
#[derive(Debug, Clone)]
pub struct Config {
    pub bind_addr: SocketAddr,
    pub log_filter: String,
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

        Ok(Self {
            bind_addr,
            log_filter,
        })
    }
}

/// Errors raised while loading configuration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("LOCAST_BIND_ADDR={0} is not a valid socket address: {1}")]
    InvalidBindAddr(String, std::net::AddrParseError),
}
