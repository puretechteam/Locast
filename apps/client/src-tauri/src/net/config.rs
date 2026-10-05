//! `net::config` - runtime configuration of the signaling client.
//!
//! The URL is read once, at the moment the `SignalingClient` is
//! constructed, from `LOCAST_SIGNALING_URL` or else from the address
//! saved in Settings (`SETTING_URL_KEY`). The default is the local dev
//! server (`ws://127.0.0.1:8787/ws`). Production deployments MUST
//! supply one of the two; the client never bakes a production
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
        Self::from_env_with_stored(None)
    }

    /// Like [`Self::from_env`], but with the URL the user saved in
    /// Settings as a fallback. Precedence: the `LOCAST_SIGNALING_URL`
    /// env var, then the stored URL (re-validated here, so a value
    /// edited in the database cannot bypass the rules), then the
    /// local-dev default.
    pub fn from_env_with_stored(stored_url: Option<&str>) -> Self {
        let url = std::env::var(ENV_URL)
            .ok()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| {
                let stored = stored_url?;
                match validate_signaling_url(stored) {
                    Ok(valid) => Some(valid),
                    Err(e) => {
                        tracing::warn!(error = %e, "ignoring the saved server address");
                        None
                    }
                }
            })
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

/// `settings` table key holding the user's chosen signaling URL.
pub const SETTING_URL_KEY: &str = "network.signaling_url";

/// Why a signaling URL was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SignalingUrlError {
    #[error("enter a server address")]
    Empty,
    #[error("not a valid URL")]
    Invalid,
    #[error("the address must start with wss:// (or ws:// for this computer only)")]
    BadScheme,
    #[error("the address needs a host name")]
    MissingHost,
    #[error("ws:// is only allowed for localhost; use wss:// for a remote server")]
    InsecureRemote,
    #[error("the address must not contain a user name or password")]
    HasCredentials,
    #[error("the address needs a path, for example /ws")]
    MissingPath,
}

/// Validate a signaling URL entered by the user and return its
/// normalized form. Only `wss://` is accepted for remote hosts: the
/// handshake and every room message ride this socket, so a plaintext
/// remote endpoint is refused. `ws://` is allowed for loopback so local
/// development keeps working. (The env var is the unrestricted override
/// for other setups, such as a server on a trusted LAN.)
pub fn validate_signaling_url(raw: &str) -> Result<String, SignalingUrlError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(SignalingUrlError::Empty);
    }
    let url = url::Url::parse(raw).map_err(|_| SignalingUrlError::Invalid)?;
    // The parser reads `wss:///ws` as host `ws` (and treats `\` as `/`);
    // an empty authority is a typo, not a host.
    if raw
        .split_once("://")
        .is_some_and(|(_, rest)| rest.starts_with(['/', '\\']))
    {
        return Err(SignalingUrlError::MissingHost);
    }
    let host = url.host().ok_or(SignalingUrlError::MissingHost)?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err(SignalingUrlError::HasCredentials);
    }
    match url.scheme() {
        "wss" => {}
        "ws" => {
            let loopback = match host {
                url::Host::Domain(d) => d.eq_ignore_ascii_case("localhost"),
                url::Host::Ipv4(ip) => ip.is_loopback(),
                url::Host::Ipv6(ip) => ip.is_loopback(),
            };
            if !loopback {
                return Err(SignalingUrlError::InsecureRemote);
            }
        }
        _ => return Err(SignalingUrlError::BadScheme),
    }
    // The server serves the socket on a path (`/ws`); a bare host would
    // connect to `/` and fail in a way that is hard to diagnose.
    if url.path() == "/" {
        return Err(SignalingUrlError::MissingPath);
    }
    Ok(url.to_string())
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
    fn validate_accepts_wss_and_loopback_ws() {
        assert_eq!(
            validate_signaling_url("  wss://locast.example.com/ws ").as_deref(),
            Ok("wss://locast.example.com/ws")
        );
        for ok in [
            "ws://127.0.0.1:8787/ws",
            "ws://localhost:8787/ws",
            "ws://LOCALHOST/ws",
            "ws://[::1]:8787/ws",
        ] {
            assert!(validate_signaling_url(ok).is_ok(), "{ok}");
        }
    }

    #[test]
    fn validate_returns_the_normalized_form_that_is_stored() {
        assert_eq!(
            validate_signaling_url("WSS://Locast.Example.COM/ws").as_deref(),
            Ok("wss://locast.example.com/ws")
        );
        assert_eq!(
            validate_signaling_url("ws://127.1/ws").as_deref(),
            Ok("ws://127.0.0.1/ws")
        );
    }

    #[test]
    fn validate_rejects_unsafe_or_malformed_urls() {
        use SignalingUrlError::*;
        let cases = [
            ("", Empty),
            ("   ", Empty),
            ("not a url", Invalid),
            ("http://locast.example.com/ws", BadScheme),
            ("https://locast.example.com/ws", BadScheme),
            ("ftp://locast.example.com/ws", BadScheme),
            ("ws://locast.example.com/ws", InsecureRemote),
            ("ws://192.168.1.5:8787/ws", InsecureRemote),
            ("wss://user:pw@locast.example.com/ws", HasCredentials),
            ("wss://user@locast.example.com/ws", HasCredentials),
            ("wss:///ws", MissingHost),
            ("ws://\\evil.com/ws", MissingHost),
            // Look-alike and userinfo tricks must not pass as loopback.
            ("wss://localhost@evil.com/ws", HasCredentials),
            ("ws://localhost.evil.com/ws", InsecureRemote),
            ("ws://127.0.0.1.evil.com/ws", InsecureRemote),
            ("ws://localhost./ws", InsecureRemote),
            ("ws://0.0.0.0/ws", InsecureRemote),
            ("ws://[::]/ws", InsecureRemote),
            ("WS://evil.com/ws", InsecureRemote),
            // The server serves the socket on a path.
            ("wss://locast.example.com", MissingPath),
            ("wss://locast.example.com/", MissingPath),
        ];
        for (input, want) in cases {
            assert_eq!(validate_signaling_url(input), Err(want), "{input:?}");
        }
    }

    #[test]
    fn stored_url_is_used_but_never_unvalidated() {
        // Only meaningful when the env override is not set in this process.
        if std::env::var(ENV_URL).is_ok_and(|s| !s.trim().is_empty()) {
            return;
        }
        let good = SignalingConfig::from_env_with_stored(Some("wss://locast.example.com/ws"));
        assert_eq!(good.url, "wss://locast.example.com/ws");
        let bad = SignalingConfig::from_env_with_stored(Some("ws://evil.example.com/ws"));
        assert_eq!(bad.url, DEFAULT_URL, "an invalid stored value falls back");
        let none = SignalingConfig::from_env_with_stored(None);
        assert_eq!(none.url, DEFAULT_URL);
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
