//! `net::state` - the safe view of the signaling connection
//! that the webview is allowed to see.
//!
//! This is the only `serde::Serialize` shape that crosses the
//! IPC boundary for the signaling subsystem. The bearer token,
//! the nonce bytes, the AUTH signature, and the private key
//! NEVER appear here. The frontend can only know that the
//! connection is `Authenticated` (or not) and what
//! server-assigned ids the user has.
//!
//! The `last_error` field is a redacted string suitable for
//! the UI. It must not contain the bearer, the signature, the
//! nonce, or the private key. Callers building error strings
//! must enforce that.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

use serde::{Deserialize, Serialize};
use specta::Type;

/// The coarse-grained phase the connection is in. The variants
/// are the same set the architecture describes in section
/// 22.3.1; the names match the prose ("Disconnected",
/// "Connecting", "Handshaking", "Authenticated",
/// "Reconnecting", "ShuttingDown").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "PascalCase")]
pub enum ConnPhase {
    /// The client has been constructed but the connection
    /// loop has not been started (or it has been shut down).
    Disconnected,
    /// The connection loop is dialing the WebSocket.
    Connecting,
    /// The socket is open and the client is mid-handshake
    /// (HELLO sent, awaiting WELCOME / CHALLENGE / AUTH_OK).
    Handshaking,
    /// The client has received AUTH_OK and holds a valid
    /// bearer. The connection loop is idle on the read side,
    /// waiting for server-initiated messages.
    Authenticated,
    /// The connection was lost and the client is sleeping for
    /// the next backoff delay before retrying.
    Reconnecting,
    /// The client has been asked to shut down and is closing
    /// the socket. This is terminal; the connection loop
    /// exits when the phase becomes `ShuttingDown`.
    ShuttingDown,
}

/// The reason the previous connection ended. The frontend can
/// show this in the status bar but the messages MUST stay
/// redacted (no token, no signature, no nonce).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "PascalCase")]
pub enum DisconnectReason {
    /// The server sent a Close frame or EOF. Normal close.
    ServerClose,
    /// A decode, version, or framing violation. The server
    /// might be speaking an incompatible dialect, or we
    /// received an oversized frame.
    ProtocolError,
    /// The server replied with AUTH_FAIL.
    AuthFailed,
    /// The HELLO / WELCOME / CHALLENGE / AUTH round trip did
    /// not complete within the configured handshake timeout.
    HandshakeTimeout,
    /// The underlying TCP / TLS / DNS layer refused or could
    /// not be reached.
    NetworkUnreachable,
    /// The local client asked to shut down.
    LocalShutdown,
}

/// The connection state the webview is allowed to see.
///
/// **Security contract:**
///
/// - `last_error` MUST NOT contain the bearer token, the AUTH
///   signature, the challenge nonce, or any private key
///   material. Callers building error strings MUST redact
///   these values before storing them here.
/// - `bearer` is intentionally absent. The token lives in
///   `SignalingInner` in `tauri::State` and never crosses
///   the IPC boundary.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct ConnectionState {
    /// The coarse phase the connection is in.
    pub phase: ConnPhase,
    /// The WebSocket URL the client is configured for. Echoed
    /// here so the UI can show "trying to reach ...".
    pub server_url: String,
    /// The server-assigned session id, set after a successful
    /// WELCOME. `None` until the handshake completes.
    pub session_id: Option<String>,
    /// The server-assigned user id, set after a successful
    /// AUTH_OK. `None` until the handshake completes.
    pub user_id: Option<String>,
    /// Convenience: `phase == Authenticated`. Mirrored so the
    /// webview can render a green dot without comparing
    /// phases itself.
    pub connected: bool,
    /// The current backoff attempt counter. `0` after a fresh
    /// start or a successful AUTH_OK; grows on each failed
    /// connect.
    pub attempt: u32,
    /// A redacted human-readable error from the most recent
    /// disconnect. Never includes the bearer, the signature,
    /// the nonce, or the private key.
    pub last_error: Option<String>,
    /// Unix-ms timestamp of the most recent disconnect.
    pub last_error_at_ms: Option<i64>,
}

impl ConnectionState {
    /// Build the initial state for a freshly-constructed
    /// client. The phase is `Disconnected`; the URL is the
    /// one the client was configured with.
    pub fn for_url(url: &str) -> Self {
        Self {
            phase: ConnPhase::Disconnected,
            server_url: url.to_string(),
            session_id: None,
            user_id: None,
            connected: false,
            attempt: 0,
            last_error: None,
            last_error_at_ms: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_state_is_disconnected() {
        let s = ConnectionState::for_url("ws://example.test/ws");
        assert_eq!(s.phase, ConnPhase::Disconnected);
        assert!(!s.connected);
        assert!(s.session_id.is_none());
        assert!(s.user_id.is_none());
        assert!(s.last_error.is_none());
    }

    #[test]
    fn disconnect_reasons_serialize_pascal_case() {
        let r = DisconnectReason::HandshakeTimeout;
        let s = serde_json::to_string(&r).unwrap();
        assert_eq!(s, "\"HandshakeTimeout\"");
    }
}
