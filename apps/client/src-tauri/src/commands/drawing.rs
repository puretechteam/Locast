//! P5-T02: Tauri command for the DRAW_BEGIN / DRAW_POINT /
//! DRAW_END wire protocol.
//!
//! Mirrors `apps/client/src-tauri/src/commands/playback.rs`
//! (P4-T02). The React layer's `services/drawing.ts` calls
//! `commands.drawingSend(action, payload)`; this command
//! builds the `Envelope`, attaches the signature on
//! `DRAW_BEGIN` only (DRAW_POINT and DRAW_END are
//! unsigned; the server binds `stroke_id -> sender_id`
//! from the begin signature), and forwards the envelope
//! through the shared `SignalingClient`.
//!
//! Signing ownership:
//!
//! Per architecture §15.4 + §18.9 the Ed25519 private key
//! is owned by `IdentityService` (a `TauriState`) and
//! NEVER leaves the Rust side. The React layer supplies
//! only the plaintext payload fields; the Rust command
//! looks up the identity via the TauriState and signs
//! in-process before the envelope is forwarded.
//!
//! Coalescing:
//!
//! This command does NOT enforce the 80 Hz limit; that is
//! the React layer's responsibility, in
//! `src/drawing/drawingSession.ts` (wrapped by
//! `services/drawing.ts`). The server also applies a
//! per-connection message rate limit (P2-T04).
//!
//! Validation:
//!
//! Values the server would refuse are rejected here with an
//! `Err` and nothing is sent (see [`validate_input`]). A
//! server refusal arrives as an unsolicited ROOM_ERROR, which
//! the room client treats as the end of the room, so a single
//! bad value would otherwise silently evict the user locally.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

use ed25519_dalek::Signer;
use serde::{Deserialize, Serialize};
use specta::Type;
use tauri::State as TauriState;

use crate::commands::error::AppError;
use crate::identity::keystore::IdentityService;
use crate::net::room::RoomClient;
use crate::net::signaling::SignalingClient;

/// P5-T02: dispatch a drawing envelope. `action`
/// discriminates between the three wire kinds. The
/// payload is supplied as a single tagged enum so the
/// React side only needs one typed call.
#[derive(Debug, Clone, Deserialize, Type)]
#[serde(tag = "action", rename_all = "lowercase")]
pub enum DrawingSendInput {
    Begin {
        stroke_id: String,
        tool: String,
        color: String,
        width: f32,
        x: f32,
        y: f32,
        pressure: f32,
        ts_ms: i64,
        client_seq: u64,
    },
    Point {
        stroke_id: String,
        x: f32,
        y: f32,
        pressure: f32,
        ts_ms: i64,
        client_seq: u64,
    },
    End {
        stroke_id: String,
        ts_ms: i64,
        client_seq: u64,
    },
}

#[derive(Debug, Clone, Serialize, Type)]
pub struct DrawingSendResult {
    pub envelope_id: String,
    pub stroke_id: String,
}

fn err<S: Into<String>>(s: S) -> AppError {
    AppError::other(s.into())
}

/// Validate a stroke id coming from the React layer.
///
/// Accepts only a canonical lowercase hyphenated UUID
/// (`xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx`) that is not nil.
/// `Uuid::parse_str` alone also accepts uppercase, braced,
/// URN and 32-digit forms; those would be normalized on the
/// wire and the echoed id would no longer equal the id the
/// local renderer holds, so they are rejected here.
pub fn parse_stroke_id(raw: &str) -> Result<uuid::Uuid, AppError> {
    let id = uuid::Uuid::parse_str(raw).map_err(|e| err(format!("bad stroke id: {e}")))?;
    if id.is_nil() || id.hyphenated().to_string() != raw {
        return Err(err("bad stroke id: not a canonical lowercase UUID"));
    }
    Ok(id)
}

/// Longest accepted stroke colour string, in bytes. Neither the
/// server nor the protocol validates `color` (the renderer just
/// assigns it to `ctx.strokeStyle`); this is a client-side
/// sanity cap so a hostile or buggy caller cannot push an
/// oversized string into every participant's canvas state. The
/// UI sends `#rrggbb`.
pub const MAX_COLOR_BYTES: usize = 64;

/// The server's unit-range rule (`rooms/validation.rs`
/// `validate_unit_range`): finite and within `[0, 1]`.
fn unit_ok(n: f32) -> bool {
    n.is_finite() && (0.0..=1.0).contains(&n)
}

/// Reject, before anything is built or sent, every value the
/// server would answer with ROOM_ERROR (mirrors
/// `apps/server/src/rooms/drawing.rs`):
///
/// - BEGIN: `x`, `y`, `pressure` finite and in `[0, 1]`;
///   `width` finite and `> 0`.
/// - POINT: `x`, `y`, `pressure` finite and in `[0, 1]`.
/// - END: nothing beyond the stroke id.
///
/// Also checks the stroke id, the tool name and the colour cap.
pub fn validate_input(input: &DrawingSendInput) -> Result<(), AppError> {
    match input {
        DrawingSendInput::Begin {
            stroke_id,
            tool,
            color,
            width,
            x,
            y,
            pressure,
            ..
        } => {
            parse_stroke_id(stroke_id)?;
            parse_tool(tool)?;
            if color.is_empty()
                || color.len() > MAX_COLOR_BYTES
                || color.chars().any(char::is_control)
            {
                return Err(err("bad stroke color"));
            }
            if !unit_ok(*x) || !unit_ok(*y) || !unit_ok(*pressure) {
                return Err(err("stroke coordinates and pressure must be within [0, 1]"));
            }
            if !(width.is_finite() && *width > 0.0) {
                return Err(err("stroke width must be finite and > 0"));
            }
        }
        DrawingSendInput::Point {
            stroke_id,
            x,
            y,
            pressure,
            ..
        } => {
            parse_stroke_id(stroke_id)?;
            if !unit_ok(*x) || !unit_ok(*y) || !unit_ok(*pressure) {
                return Err(err("point coordinates and pressure must be within [0, 1]"));
            }
        }
        DrawingSendInput::End { stroke_id, .. } => {
            parse_stroke_id(stroke_id)?;
        }
    }
    Ok(())
}

/// Map the wire tool name onto the protocol enum. The
/// protocol carries all six tools; `drawing_send` used to
/// accept only `pen`, which made every other toolbar tool
/// fail to send.
fn parse_tool(raw: &str) -> Result<locast_protocol::room::StrokeTool, AppError> {
    use locast_protocol::room::StrokeTool;
    Ok(match raw {
        "pen" => StrokeTool::Pen,
        "arrow" => StrokeTool::Arrow,
        "rect" => StrokeTool::Rect,
        "circle" => StrokeTool::Circle,
        "text" => StrokeTool::Text,
        "eraser" => StrokeTool::Eraser,
        other => return Err(err(format!("unsupported drawing tool: {other}"))),
    })
}

/// P5-T02: send a drawing envelope to the server.
///
/// `Begin` builds a signed DRAW_BEGIN envelope (Ed25519
/// signature over
/// `locast_crypto::drawing_signed_bytes(&payload)`,
/// domain tag `"DRAW_START"`). `Point` / `End` build
/// unsigned envelopes (the `sender` field is `None`;
/// the server validates the bearer identity against
/// the bound stroke).
///
/// `client_seq` is the sender's per-stroke counter, carried
/// in the envelope's `seq` field.
///
/// `stroke_id` must be a canonical (lowercase, hyphenated)
/// UUID, see [`parse_stroke_id`]; the React layer mints UUID
/// v7 ids and uses the same id for its local stroke store.
/// The BEGIN envelope's `sender.user_id` is the
/// server-assigned id of this connection (the room client's
/// `local_user_id`), which is what the server compares the
/// signed sender against.
#[tauri::command]
#[specta::specta]
pub async fn drawing_send(
    input: DrawingSendInput,
    room: TauriState<'_, std::sync::Arc<RoomClient>>,
    signaling: TauriState<'_, std::sync::Arc<SignalingClient>>,
    identity: TauriState<'_, std::sync::Arc<IdentityService>>,
) -> Result<DrawingSendResult, AppError> {
    send_drawing(input, &room, &signaling, &identity).await
}

/// The body of [`drawing_send`], with the managed state passed
/// in as plain references. Kept separate so the integration test
/// (`tests/drawing_send_e2e.rs`) can run the production code
/// against a real server without a Tauri runtime (Windows test
/// binaries cannot host Tauri's mock runtime).
pub async fn send_drawing(
    input: DrawingSendInput,
    room: &RoomClient,
    signaling: &SignalingClient,
    identity: &IdentityService,
) -> Result<DrawingSendResult, AppError> {
    validate_input(&input)?;
    let summary = room.state().await.ok_or_else(|| err("not in a room"))?;
    let room_id =
        uuid::Uuid::parse_str(&summary.id).map_err(|e| err(format!("bad cached room id: {e}")))?;

    let (kind, stroke_id_str, ts_ms, client_seq) = match &input {
        DrawingSendInput::Begin {
            stroke_id,
            ts_ms,
            client_seq,
            ..
        } => (
            locast_protocol::envelope::MessageKind::StrokeBegin,
            stroke_id.clone(),
            *ts_ms,
            *client_seq,
        ),
        DrawingSendInput::Point {
            stroke_id,
            ts_ms,
            client_seq,
            ..
        } => (
            locast_protocol::envelope::MessageKind::StrokePoint,
            stroke_id.clone(),
            *ts_ms,
            *client_seq,
        ),
        DrawingSendInput::End {
            stroke_id,
            ts_ms,
            client_seq,
        } => (
            locast_protocol::envelope::MessageKind::StrokeEnd,
            stroke_id.clone(),
            *ts_ms,
            *client_seq,
        ),
    };
    let stroke_id = parse_stroke_id(&stroke_id_str)?;

    // Build the typed payload + (for Begin) the signed
    // sender.
    let (payload_value, sender) = match &input {
        DrawingSendInput::Begin {
            tool,
            color,
            width,
            x,
            y,
            pressure,
            ..
        } => {
            let tool = parse_tool(tool)?;
            // Only BEGIN is signed, so only BEGIN touches the
            // keyring. The Ed25519 key never leaves Rust; the
            // `Keypair` is dropped at the end of this arm.
            let kp = identity
                .load_keypair()
                .await
                .map_err(|e| err(format!("load_keypair: {e}")))?;
            let pubkey: [u8; 32] = kp.signing.verifying_key().to_bytes();
            // The server compares `sender.user_id` with the
            // connection's server-assigned user id (a UUID), NOT
            // with `derive_user_id(pubkey)` (a 64-char sha256
            // hex string that is not even a UUID). The room
            // client learned it from ROOM_CREATED / ROOM_JOINED.
            let user_id = room
                .local_user_id()
                .await
                .ok_or_else(|| err("no server-assigned user id (not in a room)"))?;
            let begin_payload = locast_protocol::room::StrokeBeginPayload {
                stroke_id,
                tool,
                color: color.clone(),
                width: *width,
                x: *x,
                y: *y,
                pressure: *pressure,
                ts_ms,
            };
            // Sign the canonical bytes (domain tag +
            // msgpack) and attach them to the sender.
            let signed = locast_crypto::drawing_signed_bytes(&begin_payload)
                .map_err(|e| err(format!("serialize drawing payload: {e}")))?;
            let sig = kp.signing.sign(&signed);
            let sender = locast_protocol::envelope::Sender {
                user_id,
                pubkey: pubkey.to_vec(),
                sig: sig.to_bytes().to_vec(),
            };
            (
                serde_json::to_value(&begin_payload)
                    .map_err(|e| err(format!("serialize begin payload: {e}")))?,
                Some(sender),
            )
        }
        DrawingSendInput::Point { x, y, pressure, .. } => {
            let point_payload = locast_protocol::room::StrokePointPayload {
                stroke_id,
                x: *x,
                y: *y,
                pressure: *pressure,
                ts_ms,
            };
            (
                serde_json::to_value(&point_payload)
                    .map_err(|e| err(format!("serialize point payload: {e}")))?,
                None,
            )
        }
        DrawingSendInput::End { .. } => {
            let end_payload = locast_protocol::room::StrokeEndPayload { stroke_id, ts_ms };
            (
                serde_json::to_value(&end_payload)
                    .map_err(|e| err(format!("serialize end payload: {e}")))?,
                None,
            )
        }
    };

    let envelope_id = uuid::Uuid::now_v7();
    let env = locast_protocol::envelope::Envelope {
        v: 1,
        r#type: kind,
        id: envelope_id,
        room_id: Some(room_id),
        sender,
        ts_ms,
        seq: client_seq,
        payload: payload_value,
    };

    signaling
        .send_envelope(env.clone())
        .await
        .map_err(|e| err(format!("send_envelope: {e}")))?;

    Ok(DrawingSendResult {
        envelope_id: env.id.to_string(),
        stroke_id: stroke_id.to_string(),
    })
}
