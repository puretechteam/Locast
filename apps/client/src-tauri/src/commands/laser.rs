//! P5-T04: Tauri command for the LASER_MOVE / LASER_OFF wire
//! protocol (the transient laser pointer).
//!
//! Mirrors `commands/drawing.rs` (`drawing_send` ->
//! `send_drawing`) and the POSITION_REPORT outbound path in
//! `room/report.rs`. The React layer's laser transport
//! (`src/laser/laserTransport.ts`) calls
//! `commands.laserSend(input)`; this command builds an
//! unsigned envelope for the current room and forwards it
//! through the shared `SignalingClient` (which injects the
//! bearer).
//!
//! Identity:
//!
//! Neither envelope is signed and neither payload carries an
//! identity. The server stamps the authenticated sender on the
//! relay (`Envelope::sender`), so the outbound `sender` is
//! `None` and receivers never read an id from the payload.
//!
//! Rate:
//!
//! This command does NOT throttle. The React layer sends at
//! most 60 Hz (last-point-wins) plus a ~1 Hz keepalive; the
//! server drops anything above 60 msg/s per connection
//! silently.
//!
//! Validation:
//!
//! Coordinates must be finite and within `[0, 1]`; anything
//! else is rejected with an `Err` and nothing is sent (no
//! clamping: a bad value is a caller bug, and the server would
//! drop it anyway). The server drops laser denials silently, so
//! a refused laser never evicts the user.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

use serde::{Deserialize, Serialize};
use specta::Type;
use tauri::State as TauriState;

use crate::commands::error::AppError;
use crate::net::room::RoomClient;
use crate::net::signaling::SignalingClient;

/// P5-T04: one laser action. `Move` carries the pointer
/// position normalized to the video frame; `Off` says the
/// local user released the laser.
#[derive(Debug, Clone, Deserialize, Type)]
#[serde(tag = "action", rename_all = "lowercase")]
pub enum LaserSendInput {
    Move { x: f32, y: f32 },
    Off,
}

#[derive(Debug, Clone, Serialize, Type)]
pub struct LaserSendResult {
    pub envelope_id: String,
}

fn err<S: Into<String>>(s: S) -> AppError {
    AppError::other(s.into())
}

/// The server's unit-range rule: finite and within `[0, 1]`.
fn unit_ok(n: f32) -> bool {
    n.is_finite() && (0.0..=1.0).contains(&n)
}

/// Reject, before anything is built or sent, a position the
/// server would drop: `x` and `y` must be finite and in
/// `[0, 1]`. `Off` carries nothing to validate.
pub fn validate_input(input: &LaserSendInput) -> Result<(), AppError> {
    match input {
        LaserSendInput::Move { x, y } => {
            if !unit_ok(*x) || !unit_ok(*y) {
                return Err(err("laser coordinates must be finite and within [0, 1]"));
            }
        }
        LaserSendInput::Off => {}
    }
    Ok(())
}

/// P5-T04: send one LASER_MOVE / LASER_OFF to the current room.
#[tauri::command]
#[specta::specta]
pub async fn laser_send(
    input: LaserSendInput,
    room: TauriState<'_, std::sync::Arc<RoomClient>>,
    signaling: TauriState<'_, std::sync::Arc<SignalingClient>>,
) -> Result<LaserSendResult, AppError> {
    send_laser(input, &room, &signaling).await
}

/// The body of [`laser_send`], with the managed state passed in
/// as plain references so the integration test
/// (`tests/laser_send_e2e.rs`) can run it against a real server
/// without a Tauri runtime.
pub async fn send_laser(
    input: LaserSendInput,
    room: &RoomClient,
    signaling: &SignalingClient,
) -> Result<LaserSendResult, AppError> {
    validate_input(&input)?;
    // The signaling outbound queue survives a reconnect, so a laser
    // held through an outage would pile up 60 frames a second and
    // flush them all (stale, and over the server's rate limit) on
    // reconnect. A laser position is worthless once late: refuse
    // instead of queueing while not authenticated.
    if !signaling.snapshot().await.connected {
        return Err(err("not connected"));
    }
    let summary = room.state().await.ok_or_else(|| err("not in a room"))?;
    let room_id =
        uuid::Uuid::parse_str(&summary.id).map_err(|e| err(format!("bad cached room id: {e}")))?;

    let (kind, payload) = match input {
        LaserSendInput::Move { x, y } => (
            locast_protocol::envelope::MessageKind::LaserMove,
            serde_json::to_value(locast_protocol::room::LaserMovePayload { x, y })
                .map_err(|e| err(format!("serialize laser move payload: {e}")))?,
        ),
        LaserSendInput::Off => (
            locast_protocol::envelope::MessageKind::LaserOff,
            serde_json::to_value(locast_protocol::room::LaserOffPayload {})
                .map_err(|e| err(format!("serialize laser off payload: {e}")))?,
        ),
    };

    let env = locast_protocol::envelope::Envelope {
        v: 1,
        r#type: kind,
        id: uuid::Uuid::now_v7(),
        room_id: Some(room_id),
        // The server attributes the envelope to the bearer's
        // user and stamps it on the relay.
        sender: None,
        ts_ms: now_ms(),
        // Lasers are unsequenced (superseded by the next move).
        seq: 0,
        payload,
    };

    signaling
        .send_envelope(env.clone())
        .await
        .map_err(|e| err(format!("send_envelope: {e}")))?;

    Ok(LaserSendResult {
        envelope_id: env.id.to_string(),
    })
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn move_requires_finite_unit_coordinates() {
        for (x, y) in [(0.0, 0.0), (1.0, 1.0), (0.25, 0.75)] {
            assert!(
                validate_input(&LaserSendInput::Move { x, y }).is_ok(),
                "({x}, {y}) is in range"
            );
        }
        for (x, y) in [
            (-0.01, 0.5),
            (0.5, 1.01),
            (f32::NAN, 0.5),
            (0.5, f32::INFINITY),
            (f32::NEG_INFINITY, 0.5),
        ] {
            assert!(
                validate_input(&LaserSendInput::Move { x, y }).is_err(),
                "({x}, {y}) must be rejected, not clamped"
            );
        }
    }

    #[test]
    fn off_carries_nothing_to_validate() {
        assert!(validate_input(&LaserSendInput::Off).is_ok());
    }

    #[test]
    fn input_deserializes_from_the_typescript_shape() {
        let mv: LaserSendInput =
            serde_json::from_value(serde_json::json!({ "action": "move", "x": 0.5, "y": 0.25 }))
                .expect("move");
        assert!(matches!(mv, LaserSendInput::Move { x, y } if x == 0.5 && y == 0.25));
        let off: LaserSendInput =
            serde_json::from_value(serde_json::json!({ "action": "off" })).expect("off");
        assert!(matches!(off, LaserSendInput::Off));
        // A move without coordinates is not a move.
        assert!(
            serde_json::from_value::<LaserSendInput>(serde_json::json!({ "action": "move" }))
                .is_err()
        );
    }

    #[test]
    fn wire_payloads_carry_no_identity() {
        let mv = serde_json::to_value(locast_protocol::room::LaserMovePayload { x: 0.5, y: 0.5 })
            .unwrap();
        assert_eq!(mv, serde_json::json!({ "x": 0.5, "y": 0.5 }));
        let off = serde_json::to_value(locast_protocol::room::LaserOffPayload {}).unwrap();
        assert_eq!(off, serde_json::json!({}));
    }
}
