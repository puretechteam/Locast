//! P4-T03: the 1 Hz POSITION_REPORT outbound path.
//!
//! The host's React layer periodically reads the local
//! `<video>` element's `currentTime` and `paused` state and
//! calls the `position_report` Tauri command. This module
//! builds a `POSITION_REPORT` envelope and forwards it
//! through the signaling WebSocket; the server (per
//! `apps/server/src/rooms/presence.rs`) is a pure relay and
//! broadcasts the report to every other participant. The
//! WS forwarder's originator filter prevents the sender
//! from seeing its own report echoed back.
//!
//! Design notes:
//!
//! - The command does NOT spawn a background task; the 1 Hz
//!   cadence lives in the React layer (see
//!   `apps/client/src/components/Player.tsx`) so the
//!   timer lifecycle is bound to the room page's mount /
//!   unmount rather than to the application's lifetime.
//!   A background Rust task would make the timer leak
//!   across room changes and survive the React page
//!   unmounting.
//! - The command does NOT validate the caller's media
//!   position against media duration; that is the host's
//!   job at PUBLISH time. The server is a relay (the
//!   roadmap explicitly says "server forwards without
//!   modification"). The Rust side just trusts the React
//!   layer's view of the DOM and emits the envelope.
//! - The command does NOT log success at info level; the
//!   per-second cadence would otherwise produce 1 Hz logs.
//!   Errors are surfaced to the React layer via the
//!   `AppError` return type (the React layer chooses its
//!   own logging strategy).

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

use serde::{Deserialize, Serialize};
use specta::Type;
use tauri::State as TauriState;
use uuid::Uuid;

use crate::commands::error::AppError;
use crate::net::room::RoomClient;
use crate::net::signaling::SignalingClient;

/// The IPC-aligned POSITION_REPORT payload the React layer
/// sends. Mirrors `locast_protocol::room::PositionReportPayload`
/// but uses plain integers (no i64/u64 ambiguity in TS) and
/// accepts the `currentTime` already converted to integer
/// milliseconds (rounded). The React layer is responsible for
/// `Math.round(video.currentTime * 1000)` so the Rust side
/// does not have to know about floating-point seconds.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct PositionReportInput {
    /// Local `<video>` position in integer milliseconds.
    pub media_position_ms: u64,
    /// `true` when the local `<video>.paused === false`
    /// (i.e. the element is actively playing).
    pub playing: bool,
}

/// Empty success result. The command is fire-and-forget
/// from the React layer's perspective; the authoritative
/// ACK for a position report is the next forwarded report
/// from another participant, not a per-call reply. A
/// dedicated result type keeps the Tauri specta pipeline
/// happy without leaking internals.
#[derive(Debug, Clone, Serialize, Type)]
pub struct PositionReportResult {
    /// The `id` field of the envelope the client sent. The
    /// server's broadcast (if any) carries the same `id`
    /// so future idempotency hooks can correlate. v1
    /// discards it.
    pub envelope_id: String,
}

/// P4-T03: send one POSITION_REPORT to the current room.
/// Builds a `POSITION_REPORT` envelope from the React
/// layer's local media observation and forwards it through
/// the signaling WebSocket. The server is a pure relay
/// (see `apps/server/src/rooms/presence.rs`).
///
/// The command is a one-shot; it does NOT start a timer.
/// The cadence is owned by the React layer (see the 1 Hz
/// `useEffect` in `apps/client/src/components/Player.tsx`).
#[tauri::command]
#[specta::specta]
pub async fn position_report(
    report: PositionReportInput,
    room: TauriState<'_, std::sync::Arc<RoomClient>>,
    signaling: TauriState<'_, std::sync::Arc<SignalingClient>>,
) -> Result<PositionReportResult, AppError> {
    // The caller must be in a room. The cached summary
    // (P2-T04) is the source of truth; the cap gate on
    // the server side will additionally check that the
    // caller is a current member of the named room.
    let summary = room
        .state()
        .await
        .ok_or_else(|| AppError::other("not in a room".to_string()))?;
    let room_id = Uuid::parse_str(&summary.id)
        .map_err(|e| AppError::other(format!("bad cached room id: {e}")))?;
    let wire_payload = locast_protocol::room::PositionReportPayload {
        // The server stamps `user_id` from the
        // validated bearer on rebroadcast (architecture
        // section 13.1 point 2: "Not signed (server
        // uses session token to attribute)"). The
        // outbound payload uses `Uuid::nil()` here so a
        // future server-side decode that pre-populates
        // the field (e.g. for sanity checks) does not
        // see a misleading value. The server's relay
        // overwrites this with the bearer-derived
        // user_id on the rebroadcast path; the
        // outbound nil is never observable on the wire
        // to other participants because the server
        // re-stamps it.
        user_id: Uuid::nil(),
        media_position_ms: report.media_position_ms,
        playing: report.playing,
        client_ts_ms: now_ms_i64(),
    };
    let env = locast_protocol::envelope::Envelope {
        v: 1,
        r#type: locast_protocol::envelope::MessageKind::PositionReport,
        id: Uuid::now_v7(),
        room_id: Some(room_id),
        // The server uses the bearer-derived identity,
        // not the envelope `sender`. The `sender` field is
        // reserved for end-to-end signed envelopes in a
        // future P5+ task (POSITION_REPORT is not signed in
        // v1 per architecture §13.1 point 2).
        sender: None,
        ts_ms: wire_payload.client_ts_ms,
        // POSITION_REPORT does not carry a per-sender
        // monotonic_seq (it is non-authoritative telemetry
        // per architecture §12.3). The envelope `seq`
        // field is set to 0 to match the existing pattern
        // for non-monotonic envelopes.
        seq: 0,
        payload: serde_json::to_value(&wire_payload)
            .map_err(|e| AppError::other(format!("serialize position report: {e}")))?,
    };
    signaling
        .send_envelope(env.clone())
        .await
        .map_err(|e| AppError::other(format!("send_envelope: {e}")))?;
    Ok(PositionReportResult {
        envelope_id: env.id.to_string(),
    })
}

/// Helper: `std::time::SystemTime::now()` as unix-ms `i64`.
/// Mirrors the helper in
/// `apps/client/src-tauri/src/commands/playback.rs:144-148`
/// (kept duplicated rather than extracted because the
/// existing helper is local to that module's docs).
fn now_ms_i64() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    //! v1 has no end-to-end Rust tests for this command
    //! (the signaling WebSocket is not mocked at this
    //! layer; tests live in `apps/server/src/rooms/presence.rs::tests`
    //! for the relay path and in the React-layer Playwright
    //! harness for the 1 Hz cadence). The unit tests here
    //! cover the payload-shape guarantees (serde
    //! round-trip + the integer-ms / bool wire shape).

    use super::*;

    #[test]
    fn position_report_input_serde_roundtrip() {
        let input = PositionReportInput {
            media_position_ms: 12_345,
            playing: true,
        };
        let json = serde_json::to_string(&input).expect("serialize");
        let back: PositionReportInput = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.media_position_ms, 12_345);
        assert!(back.playing);
    }

    #[test]
    fn position_report_input_paused_is_false() {
        let input = PositionReportInput {
            media_position_ms: 0,
            playing: false,
        };
        let v: serde_json::Value = serde_json::to_value(&input).unwrap();
        assert_eq!(v["media_position_ms"], 0);
        assert_eq!(v["playing"], false);
    }
}
