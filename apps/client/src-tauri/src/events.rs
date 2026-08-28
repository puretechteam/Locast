//! IPC event registry for the Locast desktop client.
//!
//! P0-T06 does not define any events. This module exists so the
//! `tauri-specta` bindings generator has a stable path to call
//! `collect_events![]` against. P2-T03 added the
//! `SignalingStateChanged` type alias for the signaling
//! subsystem's `signaling://state` event. P2-T05 added the
//! `RoomStateChanged` and `RoomEventEnvelope` aliases for
//! the room-lifecycle subsystem's `room://state` and
//! `room://event` events. P3+ tasks (download progress, etc.)
//! add concrete event types here and register them via
//! `tauri_specta::collect_events!` in the bindings generator.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

pub use crate::net::room::RoomSummaryIpc;
pub use crate::net::state::ConnectionState as SignalingConnectionState;

/// The `signaling://state` event payload. Emitted by the
/// `SignalingClient` whenever the connection state changes;
/// the frontend listens via `listen("signaling://state", ...)`.
///
/// The payload is the redacted `ConnectionState` shape (no
/// bearer, no signature, no nonce, no private key material).
pub type SignalingStateChanged = SignalingConnectionState;

/// The `room://state` event payload. Emitted by the
/// `RoomClient` whenever the cached room summary changes.
/// The payload is the redacted `RoomSummaryIpc` shape (no
/// bearer, no signature, no nonce) or `null` when the
/// cache is cleared (e.g. after `RoomClosed`).
pub type RoomStateChanged = Option<RoomSummaryIpc>;

/// The `room://event` event payload. Emitted by the
/// `RoomClient` for every state-changing room event
/// (`HostMigrated`, `HostReconnected`, `ParticipantJoined`,
/// `ParticipantLeft`, `RoomClosed`). The payload mirrors
/// the redacted `RoomSummaryIpc` shape so the React layer
/// can both update its cache and react to the delta with a
/// single listener.
pub type RoomEventEnvelope = RoomSummaryIpc;
