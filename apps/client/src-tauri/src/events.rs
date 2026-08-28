//! IPC event registry for the Locast desktop client.
//!
//! P0-T06 does not define any events. This module exists so the
//! `tauri-specta` bindings generator has a stable path to call
//! `collect_events![]` against. P2-T03 added the
//! `SignalingStateChanged` type alias for the signaling
//! subsystem's `signaling://state` event. P3+ tasks (download
//! progress, room state, etc.) add concrete event types here
//! and register them via `tauri_specta::collect_events!` in the
//! bindings generator.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

pub use crate::net::state::ConnectionState as SignalingConnectionState;

/// The `signaling://state` event payload. Emitted by the
/// `SignalingClient` whenever the connection state changes;
/// the frontend listens via `listen("signaling://state", ...)`.
///
/// The payload is the redacted `ConnectionState` shape (no
/// bearer, no signature, no nonce, no private key material).
pub type SignalingStateChanged = SignalingConnectionState;
