//! Tauri command for the P4-T02 host playback command send.
//!
//! `playback_send` accepts a `PlaybackCommandPayload` from the
//! React layer (the host's `Player` controls), constructs the
//! `PLAYBACK_CMD` envelope, injects the current `room_id`, and
//! forwards it to the signaling WebSocket. The server (P4-T01)
//! is the only authority: it validates the host check, the
//! room lifecycle, and the per-sender monotonic sequence, then
//! rebroadcasts the accepted event to every room participant.
//!
//! The command is the only path through which a host can emit
//! `PLAYBACK_CMD` envelopes. It must be called only for what the
//! host's USER did (the buttons in `PlaybackControls.tsx`, the Sync
//! button, or a gesture on the `<video>`'s own controls, reported by
//! `services/hostNativeControls`), never for a change the app itself
//! applied from a remote-accepted event: that would re-send it and
//! create a feedback loop.
//!
//! The `monotonic_seq` in the input is ignored: the room client numbers
//! the commands (see `RoomClient::send_playback`).

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

use serde::{Deserialize, Serialize};
use specta::Type;
use tauri::State as TauriState;

use crate::commands::error::AppError;
use crate::net::room::RoomClient;

/// The wire-aligned playback command the host submits to the
/// server. Mirrors `locast_protocol::room::PlaybackCommandPayload`
/// but keeps the action discriminant under the IPC-friendly
/// name `action` (the wire type uses `type`; see the protocol
/// docs at `shared/protocol/src/room.rs:472-477` for why).
#[derive(Debug, Clone, Deserialize, Type)]
pub struct PlaybackCommandInput {
    /// Discriminant: `play` | `pause` | `seek`.
    pub action: String,
    /// Ignored. The room client assigns the per-sender monotonic
    /// sequence itself (the server accepts exactly
    /// `last_acked_seq + 1`), and re-syncs it when a command is lost.
    /// Kept so the IPC shape does not change.
    pub monotonic_seq: u64,
    /// Media position the command applies to. Required
    /// for `play` and `seek`; ignored for `pause` (the
    /// server keeps the last play/pause position
    /// authoritative).
    pub media_position_ms: u64,
}

/// The result of a successful `playback_send`. The React
/// layer does not need to consume this (the authoritative
/// answer comes back as a `playback://state` event), but
/// returning the accepted `server_seq` helps the host's
/// UI display a "command accepted" indicator without
/// waiting for the round-trip.
#[derive(Debug, Clone, Serialize, Type)]
pub struct PlaybackSendResult {
    /// The `id` field of the envelope the host sent. The
    /// server's accepted event will carry the same `id`
    /// (P4-T01 does not yet echo it back, but the field
    /// is in the envelope for future idempotency hooks).
    pub envelope_id: String,
    /// The host's `monotonic_seq` as sent. Echoes the
    /// input so the caller can correlate with the
    /// forthcoming `playback://state` event.
    pub monotonic_seq: u64,
}

/// P4-T02: send a host playback command to the current
/// room. The server validates host authority + room
/// lifecycle + monotonic_seq; on success it rebroadcasts
/// the accepted event to every room participant (the
/// caller receives it as a `playback://state` event).
#[tauri::command]
#[specta::specta]
pub async fn playback_send(
    cmd: PlaybackCommandInput,
    room: TauriState<'_, std::sync::Arc<RoomClient>>,
) -> Result<PlaybackSendResult, AppError> {
    // The room client numbers the commands (the webview's `monotonic_seq`
    // is no longer used): the server accepts exactly last+1 from a sender,
    // and only the client sees both a command lost in flight and the
    // server's answer to the next one. A wrong action string is a programmer
    // error and is rejected before anything is sent.
    let action = match cmd.action.as_str() {
        "play" => locast_protocol::room::PlaybackAction::Play,
        "pause" => locast_protocol::room::PlaybackAction::Pause,
        "seek" => locast_protocol::room::PlaybackAction::Seek,
        other => {
            return Err(AppError::other(format!("invalid playback action: {other}")));
        }
    };
    let monotonic_seq = room
        .send_playback(action, cmd.media_position_ms)
        .await
        .map_err(|e| AppError::other(format!("send_envelope: {e}")))?;
    Ok(PlaybackSendResult {
        envelope_id: uuid::Uuid::now_v7().to_string(),
        monotonic_seq,
    })
}
