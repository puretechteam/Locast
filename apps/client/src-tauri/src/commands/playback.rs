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
//! `PLAYBACK_CMD` envelopes. The React layer's `<video>`
//! element's `play` / `pause` / `seeked` DOM event handlers
//! MUST NOT call this command (a remote-accepted event would
//! recursively re-send a command and create a feedback loop).
//! The host's UI controls (the play/pause/seek buttons in
//! `PlaybackControls.tsx`) are the only legitimate callers.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

use serde::{Deserialize, Serialize};
use specta::Type;
use tauri::State as TauriState;

use crate::commands::error::AppError;
use crate::net::room::RoomClient;
use crate::net::signaling::SignalingClient;

/// The wire-aligned playback command the host submits to the
/// server. Mirrors `locast_protocol::room::PlaybackCommandPayload`
/// but keeps the action discriminant under the IPC-friendly
/// name `action` (the wire type uses `type`; see the protocol
/// docs at `shared/protocol/src/room.rs:472-477` for why).
#[derive(Debug, Clone, Deserialize, Type)]
pub struct PlaybackCommandInput {
    /// Discriminant: `play` | `pause` | `seek`.
    pub action: String,
    /// Per-sender monotonic sequence. The host's UI must
    /// start this at `1` and increment strictly on every
    /// command it sends (including commands the user
    /// retries after a UI re-render). The server rejects
    /// anything that is not exactly `last_acked_seq + 1`.
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
    signaling: TauriState<'_, std::sync::Arc<SignalingClient>>,
) -> Result<PlaybackSendResult, AppError> {
    // The host must be in a room. The cached summary
    // (P2-T04) is the source of truth; the cap gate on
    // the server side will additionally check that the
    // caller is the current host.
    let summary = room
        .state()
        .await
        .ok_or_else(|| AppError::other("not in a room".to_string()))?;
    let room_id = uuid::Uuid::parse_str(&summary.id)
        .map_err(|e| AppError::other(format!("bad cached room id: {e}")))?;
    // Defensive parse: the wire action is one of
    // `play` | `pause` | `seek`. We do NOT trust the
    // caller's string blindly; an invalid action is a
    // programmer error and is rejected with a 4xx-style
    // AppError before any envelope is built.
    let action = match cmd.action.as_str() {
        "play" => locast_protocol::room::PlaybackAction::Play,
        "pause" => locast_protocol::room::PlaybackAction::Pause,
        "seek" => locast_protocol::room::PlaybackAction::Seek,
        other => {
            return Err(AppError::other(format!("invalid playback action: {other}")));
        }
    };
    let payload = locast_protocol::room::PlaybackCommandPayload {
        action,
        monotonic_seq: cmd.monotonic_seq,
        media_position_ms: cmd.media_position_ms,
        client_ts_ms: now_ms_i64(),
    };
    let env = locast_protocol::envelope::Envelope {
        v: 1,
        r#type: locast_protocol::envelope::MessageKind::PlaybackCmd,
        id: uuid::Uuid::now_v7(),
        room_id: Some(room_id),
        // The server uses the bearer-derived identity, not
        // the envelope `sender`. The `sender` field is
        // reserved for end-to-end signed commands in a
        // future P5+ task.
        sender: None,
        ts_ms: payload.client_ts_ms,
        seq: cmd.monotonic_seq,
        payload: serde_json::to_value(payload)
            .map_err(|e| AppError::other(format!("serialize playback payload: {e}")))?,
    };
    signaling
        .send_envelope(env.clone())
        .await
        .map_err(|e| AppError::other(format!("send_envelope: {e}")))?;
    Ok(PlaybackSendResult {
        envelope_id: env.id.to_string(),
        monotonic_seq: cmd.monotonic_seq,
    })
}

/// Helper: `std::time::SystemTime::now()` as unix-ms `i64`.
/// Duplicated from `apps/server/src/rooms/dispatch.rs` to
/// avoid a cross-crate dependency from the client to the
/// server's clock helpers. (The client already has its
/// own `apps/client/src-tauri/src/time` module; wiring
/// that into commands is a separate, broader refactor
/// and is out of scope for P4-T02.)
fn now_ms_i64() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
