//! `net::config` - runtime configuration of the signaling client.
//!
//! The URL is read from `LOCAST_SIGNALING_URL` at the moment the
//! `SignalingClient` is constructed. The default is the local dev
//! server (`ws://127.0.0.1:8787/ws`). Production deployments MUST
//! override the env var; the client never bakes a production
//! endpoint into the binary.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

use std::time::Duration;

use locast_protocol::handshake::Platform;

/// The default signaling URL for local development. The server
/// in `apps/server` binds to `0.0.0.0:8787` by default and serves
/// the WebSocket on `/ws`. This constant is intentionally
/// `ws://` (plaintext); for production a `wss://` URL must be
/// supplied via the env var.
pub const DEFAULT_URL: &str = "ws://127.0.0.1:8787/ws";

/// Default handshake timeout. Architecture section 20.4.4 does
/// not pin a specific value; 15s matches the server's default
/// `handshake_timeout_ms` of 15000 and gives the HELLO + AUTH
/// round trip comfortable headroom on slow networks.
pub const DEFAULT_HANDSHAKE_TIMEOUT_MS: u64 = 15_000;

/// Default maximum frame size, in bytes. Architecture section
/// 18.5: "WS hard ceiling 1 MiB". The client MUST refuse any
/// inbound frame larger than this.
pub const DEFAULT_MAX_FRAME_BYTES: usize = 1024 * 1024;

/// Env-var name for the signaling URL. The client reads this
/// once at construction; runtime changes are not picked up.
pub const ENV_URL: &str = "LOCAST_SIGNALING_URL";

/// Env-var name for the handshake timeout (milliseconds).
pub const ENV_HANDSHAKE_TIMEOUT_MS: &str = "LOCAST_SIGNALING_HANDSHAKE_TIMEOUT_MS";

/// Env-var name for the max frame size (bytes).
pub const ENV_MAX_FRAME_BYTES: &str = "LOCAST_SIGNALING_MAX_FRAME_BYTES";

/// Per-process configuration of the signaling client. Held by
/// the `SignalingClient` and read by the connection loop.
#[derive(Debug, Clone)]
pub struct SignalingConfig {
    /// The WebSocket URL the client connects to. Must be a
    /// `ws://` or `wss://` URL with a path component.
    pub url: String,
    /// Maximum time to wait for the full HELLO + AUTH round
    /// trip. After this elapses the connection is aborted and
    /// the client records `DisconnectReason::HandshakeTimeout`.
    pub handshake_timeout: Duration,
    /// Hard cap on inbound frame size, in bytes. Frames above
    /// this are treated as a protocol violation.
    pub max_frame_bytes: usize,
    /// The platform tag the client sends in HELLO. Detected at
    /// process start; the value is immutable for the lifetime
    /// of the client.
    pub platform: Platform,
}

impl SignalingConfig {
    /// Build a config from the process environment, falling
    /// back to the local-dev defaults. The env vars are read
    /// once; this function does not retain any reference to the
    /// environment.
    pub fn from_env() -> Self {
        let url = std::env::var(ENV_URL)
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_URL.to_string());
        let handshake_timeout_ms = std::env::var(ENV_HANDSHAKE_TIMEOUT_MS)
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(DEFAULT_HANDSHAKE_TIMEOUT_MS);
        let max_frame_bytes = std::env::var(ENV_MAX_FRAME_BYTES)
            .ok()
            .and_then(|s| s.trim().parse::<usize>().ok())
            .unwrap_or(DEFAULT_MAX_FRAME_BYTES);
        Self {
            url,
            handshake_timeout: Duration::from_millis(handshake_timeout_ms),
            max_frame_bytes,
            platform: detect_platform(),
        }
    }

    /// Test-only constructor. The other constructors are
    /// deliberately `from_env`; tests use this to pin explicit
    /// values without touching the environment. Exposed as
    /// `pub` (rather than `#[cfg(test)]`) so integration tests
    /// in `tests/` can also build configs.
    pub fn new_for_test(
        url: impl Into<String>,
        handshake_timeout: Duration,
        max_frame_bytes: usize,
        platform: Platform,
    ) -> Self {
        Self {
            url: url.into(),
            handshake_timeout,
            max_frame_bytes,
            platform,
        }
    }
}

/// Detect the host platform and map it onto the wire
/// [`Platform`] enum. Unknown OSes default to `Linux` so the
/// client never panics on a new platform; the server will reject
/// unknown values once a stricter check is added.
fn detect_platform() -> Platform {
    match std::env::consts::OS {
        "windows" => Platform::Win,
        "macos" => Platform::Mac,
        _ => Platform::Linux,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_platform_is_one_of_three() {
        let p = detect_platform();
        matches!(p, Platform::Win | Platform::Mac | Platform::Linux);
    }

    #[test]
    fn new_for_test_keeps_values() {
        let c = SignalingConfig::new_for_test(
            "ws://example.test/ws",
            Duration::from_millis(100),
            4096,
            Platform::Linux,
        );
        assert_eq!(c.url, "ws://example.test/ws");
        assert_eq!(c.handshake_timeout, Duration::from_millis(100));
        assert_eq!(c.max_frame_bytes, 4096);
        assert_eq!(c.platform, Platform::Linux);
    }
}
