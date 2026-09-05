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
//! This command does NOT enforce the 120 Hz limit (that
// is the React layer's responsibility, in
//! `services/drawing.ts`). The server enforces a per-
// connection message rate (P2-T04) which is the second
// line of defense.

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
/// `client_seq` is the per-sender monotonic counter for
/// the DRAW stream; the server uses it to drop duplicate
/// / out-of-order DRAW_POINT / DRAW_END envelopes for a
/// given `stroke_id`. P5-T02's coalescing (in the React
/// layer) is "last-point-wins" so duplicate client_seq
/// values are common; the server simply drops them.
///
/// `client_seq` for `begin` MUST be `1` and `stroke_id`
/// MUST be UUID v7. The React layer stamps both.
#[tauri::command]
#[specta::specta]
pub async fn drawing_send(
    input: DrawingSendInput,
    room: TauriState<'_, std::sync::Arc<RoomClient>>,
    signaling: TauriState<'_, std::sync::Arc<SignalingClient>>,
    identity: TauriState<'_, std::sync::Arc<IdentityService>>,
) -> Result<DrawingSendResult, AppError> {
    let summary = room
        .state()
        .await
        .ok_or_else(|| err("not in a room"))?;
    let room_id = uuid::Uuid::parse_str(&summary.id)
        .map_err(|e| err(format!("bad cached room id: {e}")))?;

    let (kind, stroke_id_str, ts_ms, client_seq) = match &input {
        DrawingSendInput::Begin { stroke_id, ts_ms, client_seq, .. } => (
            locast_protocol::envelope::MessageKind::StrokeBegin,
            stroke_id.clone(),
            *ts_ms,
            *client_seq,
        ),
        DrawingSendInput::Point { stroke_id, ts_ms, client_seq, .. } => (
            locast_protocol::envelope::MessageKind::StrokePoint,
            stroke_id.clone(),
            *ts_ms,
            *client_seq,
        ),
        DrawingSendInput::End { stroke_id, ts_ms, client_seq } => (
            locast_protocol::envelope::MessageKind::StrokeEnd,
            stroke_id.clone(),
            *ts_ms,
            *client_seq,
        ),
    };
    let stroke_id = uuid::Uuid::parse_str(&stroke_id_str)
        .map_err(|e| err(format!("bad stroke id: {e}")))?;

    // Resolve the identity (signing key + pubkey + user_id).
    // The identity service is the single TauriState that
    // owns the Ed25519 key; the React layer never sees it.
    // P5-T02 establishes the canonical signing path for
    // DRAW_BEGIN: the Rust side reads the keypair once
    // per call, signs the canonical bytes (domain tag +
    // msgpack), and attaches the signature to the
    // envelope's `sender` field. The `Keypair` is dropped
    // at the end of the scope (the architecture's
    // guidance: "the seed is dropped when the binding
    // goes out of scope"; see keystore.rs::sign_manifest).
    let kp = identity
        .load_keypair()
        .await
        .map_err(|e| err(format!("load_keypair: {e}")))?;
    let pubkey: [u8; 32] = kp.signing.verifying_key().to_bytes();
    let user_id_str = crate::identity::derive_user_id(pubkey);
    let user_id = uuid::Uuid::parse_str(&user_id_str)
        .map_err(|e| err(format!("derive user_id: {e}")))?;

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
            let tool = match tool.as_str() {
                "pen" => locast_protocol::room::StrokeTool::Pen,
                other => return Err(err(format!("unsupported drawing tool: {other}"))),
            };
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
        DrawingSendInput::Point {
            x,
            y,
            pressure,
            ..
        } => {
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