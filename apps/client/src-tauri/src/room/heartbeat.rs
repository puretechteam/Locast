//! P4-T08: client-side presence heartbeat.
//!
//! `apps/client/src-tauri/src/room/heartbeat.rs` is the
//! roadmap-named home for the client's room-liveness
//! heartbeat. The actual `tokio::spawn` loop lives in
//! `apps/client/src-tauri/src/net/room.rs` ([`net::room::RoomClient::spawn_presence_loop`])
//! because the loop's lifecycle is tightly coupled to the
//! `SignalingClient` and to the per-`RoomClient` presence
//! `JoinHandle` (`presence_task`). Moving the spawn into
//! a separate module would force it to thread references
//! across the `room::` / `net::` boundary for no benefit
//! and risk spawning a duplicate loop. This file is the
//! documentation + public surface for that loop:
//!
//! - [`PRESENCE_INTERVAL`] is the constant the loop sleeps
//!   between emits (5 seconds; matches the server's
//!   `DEFAULT_PARTICIPANT_DISCONNECT_AFTER_MS` budget of 3
//!   missed intervals = 15 s).
//! - [`HeartbeatHandle`] is a tiny new-type that the
//!   existing `presence_task` slot could be migrated to in
//!   the future; for now it documents the public contract
//!   that [`net::room::RoomClient::spawn_presence_loop`]
//!   upholds (start on join/create, abort on leave /
//!   `RoomClosed` / `Drop`, single instance per
//!   `RoomClient`).
//!
//! Lifecycle invariants the underlying loop MUST honor (see
//! `net::room::RoomClient` for the implementation):
//!
//! 1. **Starts once** when the user joins or creates a
//!    room, AFTER the join/create reply is cached locally
//!    so the first `PRESENCE` is accepted by the server
//!    (the WS layer rejects `PRESENCE` from a caller who is
//!    not a current room participant).
//! 2. **Sends every 5 s** (the server threshold is 3
//!    missed intervals = 15 s; a slack of one interval
//!    tolerates one dropped PRES without disconnecting).
//! 3. **Stops on leave** (`room_leave`) so an outgoing
//!    user does not keep emitting PRESENCE after the
//!    server has removed them from the room.
//! 4. **Stops on disconnect** (inbound `RoomClosed` /
//!    `RoomError`). The signaling layer's reconnect loop
//!    transparently resumes the inbound stream once the
//!    transport is back; the next `PRESENCE` from this
//!    loop refreshes `last_seen_ms` and revives the
//!    participant via `RoomRegistry::touch`. The loop is
//!    NOT aborted on a transient signaling reconnect
//!    because the loop is coupled to the `RoomClient`,
//!    not to the underlying transport.
//! 5. **One instance only**: `spawn_presence_loop` aborts
//!    any previously-running loop before starting a new
//!    one so a re-join does not leak a duplicate task.
//! 7. **`Drop` cleanup**: `RoomClient::Drop` aborts the
//!    loop so a process teardown does not leak the task.
//!
//! Failure modes:
//!
//! - A `send_envelope` failure (signaling down, server
//!   rejecting the envelope, etc.) does NOT abort the
//!   loop in the current implementation; the next tick
//!   retries. This matches the existing v1 behavior in
//!   `net::room.rs`. The presence-timeout sweep on the
//!   server will mark the participant `Disconnected` after
//!   15 s of consecutive failures, which is the intended
//!   degradation mode.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

use std::time::Duration;

/// How often the background presence loop sends a
/// `PRESENCE` envelope while the user is in a room. The
/// server uses this to refresh `last_seen` so the
/// 15-second `DISCONNECTED` transition does not fire. The
/// same constant is used by the underlying
/// `net::room::RoomClient::spawn_presence_loop` loop;
/// this re-export is the canonical name for tests and
/// documentation that need to reference it.
pub const PRESENCE_INTERVAL: Duration = Duration::from_secs(5);

/// Public contract for the background heartbeat loop.
/// The actual `JoinHandle` is held in
/// `RoomClient::presence_task`; this new-type is reserved
/// for a future refactor that lifts the loop into its own
/// module without depending on the private `RoomClient`
/// fields.
#[derive(Debug)]
pub struct HeartbeatHandle {
    _private: (),
}

impl HeartbeatHandle {
    /// Build a sentinel handle. The real implementation
    /// lives on `RoomClient`; this constructor exists so
    /// documentation/tests can name the type without
    /// reaching into private fields.
    #[must_use]
    pub const fn sentinel() -> Self {
        Self { _private: () }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The presence interval MUST match the server's
    /// disconnect window: 3 intervals >= 15 s. A drift
    /// between client and server constants would either
    /// disconnect healthy clients or hide real outages.
    #[test]
    fn presence_interval_matches_server_window() {
        // 3 * PRESENCE_INTERVAL must be >= the server's
        // default disconnect threshold (15 s). The server
        // constant lives in apps/server/src/config.rs; we
        // hard-code 15_000 here to keep this test
        // hermetic and decoupled from the server crate.
        const SERVER_DISCONNECT_AFTER_MS: i64 = 15_000;
        let three_intervals_ms: i64 =
            (PRESENCE_INTERVAL.as_millis() as i64).saturating_mul(3);
        assert!(
            three_intervals_ms >= SERVER_DISCONNECT_AFTER_MS,
            "3 * PRESENCE_INTERVAL ({three_intervals_ms} ms) must be >= server \
             disconnect threshold ({SERVER_DISCONNECT_AFTER_MS} ms); otherwise a \
             jitter-free client could be marked Disconnected without actually \
             being offline"
        );
    }

    /// The sentinel constructor exists; it has no payload
    /// to assert beyond "does not panic".
    #[test]
    fn sentinel_constructs() {
        let _ = HeartbeatHandle::sentinel();
    }
}