//! `net` - the native WebSocket / connection state layer.
//!
//! P2-T03 introduces this module. It owns:
//!
//! - [`config`] - the runtime configuration of the signaling
//!   client (URL, timeouts, frame caps, platform tag).
//! - [`state`] - the safe view of connection state that the
//!   webview is allowed to see.
//! - [`reconnect`] - the 1s -> 30s exponential backoff with
//!   +/-20% jitter from architecture section 22.3.1.
//! - [`signaling`] - the connection loop that performs the
//!   HELLO / WELCOME / CHALLENGE / AUTH handshake and keeps the
//!   bearer token in memory.
//!
//! The WebView never sees the raw WebSocket. React reads only
//! [`state::ConnectionState`] via the `signaling_get_state`
//! command.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

pub mod config;
pub mod reconnect;
pub mod room;
pub mod signaling;
pub mod state;
pub mod webrtc;
pub mod webrtc_canonical;

pub use config::SignalingConfig;
pub use locast_protocol::handshake::Platform;
pub use reconnect::Backoff;
pub use room::{ParticipantIpc, RoomClient, RoomClientError, RoomErrorCodeIpc, RoomSummaryIpc};
pub use signaling::{BearerRecord, SignalingClient, SignalingError, SignalingInner};
pub use state::{ConnPhase, ConnectionState, DisconnectReason};
