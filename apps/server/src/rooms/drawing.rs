//! P5-T02: server-side dispatch for the DRAW_BEGIN /
//! DRAW_POINT / DRAW_END protocol.
//!
//! Wire shape: `//shared/protocol/src/room.rs` defines the
//! three payload structs (`StrokeBeginPayload`,
//! `StrokePointPayload`, `StrokeEndPayload`); `//shared/protocol/src/envelope.rs`
//! adds the matching `MessageKind` variants.
//!
//! Signing model (architecture §15.4, §18.9):
//!
//! - DRAW_BEGIN is signed by the originating user. The
//!   canonical signed bytes are produced by
//!   `locast_crypto::drawing_signed_bytes(&payload)`
//!   (domain tag `"DRAW_START"` + canonical msgpack of
//!   the payload). The envelope's `sender.sig` is verified
//!   against the payload before the stroke is admitted.
//! - DRAW_POINT and DRAW_END are NOT individually signed.
//!   The server binds `stroke_id -> (sender_id,
//!   sender_pubkey)` at BEGIN time and rejects any
//!   subsequent POINT/END whose bearer identity does not
//!   match. This matches the roadmap's "200 points in 1
//!   s / <=120 DRAW_POINT" requirement: per-point
//!   signing would inflate the wire size without
//!   strengthening the threat model (the BEGIN signature
//!   already binds the stroke's originator).
//!
//! Attribution:
//!
//! - Every rebroadcast DRAW_BEGIN / DRAW_POINT / DRAW_END
//!   carries the stroke owner's server-assigned `user_id` as
//!   `Envelope::sender.user_id` (stamped by the WS forwarder
//!   from `BroadcastItem::sender`; `pubkey` / `sig` are
//!   empty). The id is the one recorded in the pending-stroke
//!   binding at BEGIN, so it is identical for the whole
//!   stroke. Inbound `envelope.sender` is only read on BEGIN
//!   (and must match the connection); on POINT / END it is
//!   ignored.
//!
//! Authorization:
//!
//! - The capability gate in `super::caps` ensures the
//!   caller is a current member of the room named by
//!   `envelope.room_id` and holds DRAW there (the host has
//!   every bit; other participants need a PERMISSION_SET
//!   grant).
//! - Cross-room injection is rejected: the gate and the
//!   per-type dispatcher both check membership of the
//!   envelope's room, never "the user's current room"; see
//!   `dispatch.rs`.
//! - Replays of the same BEGIN signature are bound to the
//!   `stroke_id` field: a second BEGIN with the same id
//!   is rejected (the existing pending map already has
//!   the entry).
//! - Stroke abandonment: the pending map is wiped on
//!   `RoomState::new` (room teardown); individual stroke
//!   GC is a future task.

#![forbid(unsafe_code)]

#[cfg(test)]
use ed25519_dalek::Signer;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use uuid::Uuid;

use locast_protocol::envelope::{Envelope, MessageKind, Sender};
use locast_protocol::room::{StrokeBeginPayload, StrokeEndPayload, StrokePointPayload};

use super::dispatch::RoomDispatchOutcome;
use super::state::RoomState;
use super::validation::validate_unit_range;

/// Sentinel error reason returned to the caller as a
/// `ROOM_ERROR` envelope. Kept short to keep the wire
/// surface stable.
fn reason(code: DrawingError) -> &'static str {
    match code {
        DrawingError::NotSigned => "drawing_not_signed",
        DrawingError::BadSignature => "drawing_bad_signature",
        DrawingError::StrokeIdMismatch => "drawing_sender_mismatch",
        DrawingError::UnknownStroke => "drawing_unknown_stroke",
        DrawingError::OutOfRange => "drawing_out_of_range",
    }
}

#[derive(Debug, Clone, Copy)]
enum DrawingError {
    NotSigned,
    BadSignature,
    /// DRAW_POINT or DRAW_END envelope's bearer identity
    /// does not match the BEGIN's bound sender.
    StrokeIdMismatch,
    /// DRAW_POINT or DRAW_END with a `stroke_id` that
    /// has no active BEGIN.
    UnknownStroke,
    /// Coordinates or pressure outside `[0, 1]`.
    OutOfRange,
}

/// Verify the Ed25519 signature over
/// `locast_crypto::drawing_signed_bytes(&payload)`. Used
/// by `handle_stroke_begin` to validate the per-stroke
/// sender before admitting the stroke. Returns
/// `DrawingError::NotSigned` if `envelope.sender` is
/// `None` (the v1 path) or
/// `DrawingError::BadSignature` if the cryptographic
/// verification fails.
fn verify_stroke_begin_signature(
    envelope: &Envelope,
    payload: &StrokeBeginPayload,
    expected_sender: Uuid,
    expected_pubkey: [u8; 32],
) -> Result<(), DrawingError> {
    let sender: &Sender = envelope.sender.as_ref().ok_or(DrawingError::NotSigned)?;
    if sender.user_id != expected_sender {
        return Err(DrawingError::NotSigned);
    }
    if sender.pubkey.as_slice() != expected_pubkey.as_slice() {
        return Err(DrawingError::NotSigned);
    }
    let sig_bytes: [u8; 64] = sender
        .sig
        .as_slice()
        .try_into()
        .map_err(|_| DrawingError::BadSignature)?;
    let verifying_key =
        VerifyingKey::from_bytes(&expected_pubkey).map_err(|_| DrawingError::BadSignature)?;
    let signature = Signature::from_bytes(&sig_bytes);
    let signed_bytes =
        locast_crypto::drawing_signed_bytes(payload).map_err(|_| DrawingError::BadSignature)?;
    verifying_key
        .verify(&signed_bytes, &signature)
        .map_err(|_| DrawingError::BadSignature)
}

/// P5-T02: validate DRAW_BEGIN.
///
/// Verifies the signature, registers the stroke in the
/// room's pending map, and returns a `RoomDispatchOutcome`
/// whose `events` carries the rebroadcastable
/// `RoomEvent::StrokeBegin`.
pub async fn handle_stroke_begin(
    envelope: Envelope,
    state: &mut RoomState,
    user_id: Uuid,
    pubkey: [u8; 32],
    now_ms: i64,
) -> RoomDispatchOutcome {
    let payload: StrokeBeginPayload = match serde_json::from_value(envelope.payload.clone()) {
        Ok(p) => p,
        Err(e) => return err_outcome(&envelope, format!("bad DRAW_BEGIN payload: {e}")),
    };
    // Validate normalized coordinates + width (must be
    // finite, in [0, 1] for x/ y/ pressure, > 0 for
    // width). Out-of-range is rejected with a
    // single-caller ROOM_ERROR.
    if !validate_unit_range(payload.x)
        || !validate_unit_range(payload.y)
        || !validate_unit_range(payload.pressure)
        || !(payload.width.is_finite() && payload.width > 0.0)
    {
        return err_outcome(&envelope, reason(DrawingError::OutOfRange).to_string());
    }
    // Verify the signature.
    if let Err(e) = verify_stroke_begin_signature(&envelope, &payload, user_id, pubkey) {
        return err_outcome(&envelope, reason(e).to_string());
    }
    // Reject a second BEGIN for the same stroke id (a
    // replay or a collision).
    if state.drawing.pending.contains_key(&payload.stroke_id) {
        return err_outcome(
            &envelope,
            reason(DrawingError::StrokeIdMismatch).to_string(),
        );
    }
    // Bind the stroke to this sender.
    state.drawing.pending.insert(
        payload.stroke_id,
        super::state::PendingStroke {
            sender_id: user_id,
            sender_pubkey: pubkey,
            started_ms: now_ms,
        },
    );
    let evt = super::registry::RoomEvent::StrokeBegin {
        room_id: envelope.room_id.unwrap_or(state.id),
        sender_id: user_id,
        payload,
    };
    RoomDispatchOutcome {
        to_caller: Vec::new(),
        events: vec![evt],
        close_caller: false,
    }
}

/// P5-T02: validate DRAW_POINT.
///
/// Looks up the stroke in the pending map, rejects
/// cross-sender injections, validates the coordinate /
/// pressure ranges, and emits the rebroadcastable event.
pub async fn handle_stroke_point(
    envelope: Envelope,
    state: &mut RoomState,
    user_id: Uuid,
) -> RoomDispatchOutcome {
    let payload: StrokePointPayload = match serde_json::from_value(envelope.payload.clone()) {
        Ok(p) => p,
        Err(e) => return err_outcome(&envelope, format!("bad DRAW_POINT payload: {e}")),
    };
    if !validate_unit_range(payload.x)
        || !validate_unit_range(payload.y)
        || !validate_unit_range(payload.pressure)
    {
        return err_outcome(&envelope, reason(DrawingError::OutOfRange).to_string());
    }
    let binding = match state.drawing.pending.get(&payload.stroke_id) {
        Some(b) => *b,
        None => {
            return err_outcome(&envelope, reason(DrawingError::UnknownStroke).to_string());
        }
    };
    if binding.sender_id != user_id {
        return err_outcome(
            &envelope,
            reason(DrawingError::StrokeIdMismatch).to_string(),
        );
    }
    // Attribute the point to the stroke's recorded owner (equal to
    // `user_id` after the check above); `envelope.sender` is never
    // read for POINT.
    let evt = super::registry::RoomEvent::StrokePoint {
        room_id: envelope.room_id.unwrap_or(state.id),
        sender_id: binding.sender_id,
        payload,
    };
    RoomDispatchOutcome {
        to_caller: Vec::new(),
        events: vec![evt],
        close_caller: false,
    }
}

/// P5-T02: validate DRAW_END.
///
/// Looks up the stroke, rejects cross-sender / unknown
/// stroke ids, removes the binding from the pending
/// map, and emits the rebroadcastable event.
pub async fn handle_stroke_end(
    envelope: Envelope,
    state: &mut RoomState,
    user_id: Uuid,
) -> RoomDispatchOutcome {
    let payload: StrokeEndPayload = match serde_json::from_value(envelope.payload.clone()) {
        Ok(p) => p,
        Err(e) => return err_outcome(&envelope, format!("bad DRAW_END payload: {e}")),
    };
    // Check ownership BEFORE removing: another participant must
    // not be able to end (and so cancel) someone else's stroke.
    let owner = match state.drawing.pending.get(&payload.stroke_id) {
        Some(b) if b.sender_id == user_id => b.sender_id,
        Some(_) => {
            return err_outcome(
                &envelope,
                reason(DrawingError::StrokeIdMismatch).to_string(),
            );
        }
        None => {
            return err_outcome(&envelope, reason(DrawingError::UnknownStroke).to_string());
        }
    };
    state.drawing.pending.remove(&payload.stroke_id);
    // Attribute the end to the stroke's recorded owner; `envelope.sender`
    // is never read for END.
    let evt = super::registry::RoomEvent::StrokeEnd {
        room_id: envelope.room_id.unwrap_or(state.id),
        sender_id: owner,
        payload,
    };
    RoomDispatchOutcome {
        to_caller: Vec::new(),
        events: vec![evt],
        close_caller: false,
    }
}

/// Build a `ROOM_ERROR` envelope for the caller. The WS
/// layer applies `to_caller` to the originating connection
/// only (no echo to other participants).
fn err_outcome(envelope: &Envelope, message: String) -> RoomDispatchOutcome {
    let payload = locast_protocol::room::RoomErrorPayload {
        code: locast_protocol::room::RoomErrorCode::InvalidState,
        message,
    };
    let mut to_caller = Vec::new();
    if let Ok(env) = envelope_with_payload(
        MessageKind::RoomError,
        envelope.room_id,
        envelope.sender.as_ref().map(|s| s.user_id),
        &payload,
    ) {
        to_caller.push(env);
    }
    RoomDispatchOutcome {
        to_caller,
        events: Vec::new(),
        close_caller: false,
    }
}

/// Thin wrapper that mirrors `dispatch.rs::envelope_with_payload`
/// (kept local to keep this module module's dependency surface
/// small). Build a fresh envelope with the given payload.
fn envelope_with_payload<T: serde::Serialize>(
    kind: MessageKind,
    room_id: Option<Uuid>,
    sender_user: Option<Uuid>,
    payload: &T,
) -> Result<Envelope, String> {
    Ok(Envelope {
        v: 1,
        r#type: kind,
        id: Uuid::now_v7(),
        room_id,
        sender: sender_user.map(|u| Sender {
            user_id: u,
            pubkey: Vec::new(),
            sig: Vec::new(),
        }),
        ts_ms: 0,
        seq: 0,
        payload: serde_json::to_value(payload).map_err(|e| e.to_string())?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;
    use rand::RngCore;

    fn fresh_keypair() -> (SigningKey, [u8; 32]) {
        let mut seed = [0u8; 32];
        OsRng.fill_bytes(&mut seed);
        let sk = SigningKey::from_bytes(&seed);
        let pk = sk.verifying_key().to_bytes();
        (sk, pk)
    }

    fn sign_begin(sk: &SigningKey, payload: &StrokeBeginPayload) -> [u8; 64] {
        let signed = locast_crypto::drawing_signed_bytes(payload).expect("encode");
        sk.sign(&signed).to_bytes()
    }

    fn begin_envelope(
        sender_uid: Uuid,
        sender_pk: [u8; 32],
        sig: [u8; 64],
        room_id: Uuid,
        payload: StrokeBeginPayload,
    ) -> Envelope {
        Envelope {
            v: 1,
            r#type: MessageKind::StrokeBegin,
            id: Uuid::now_v7(),
            room_id: Some(room_id),
            sender: Some(Sender {
                user_id: sender_uid,
                pubkey: sender_pk.to_vec(),
                sig: sig.to_vec(),
            }),
            ts_ms: 0,
            seq: 0,
            payload: serde_json::to_value(payload).expect("payload"),
        }
    }

    fn sample_state(room_id: Uuid, host_uid: Uuid, host_pk: [u8; 32]) -> RoomState {
        RoomState::new(
            room_id,
            "AAAAAA".into(),
            "T".into(),
            host_uid,
            host_pk,
            true,
            0,
            0,
        )
    }

    #[tokio::test]
    async fn begin_with_valid_signature_is_accepted_and_binds_stroke() {
        let room_id = Uuid::now_v7();
        let (sk, pk) = fresh_keypair();
        let host_uid = Uuid::now_v7();
        let stroke_id = Uuid::now_v7();
        let payload = StrokeBeginPayload {
            stroke_id,
            tool: locast_protocol::room::StrokeTool::Pen,
            color: "#000000".into(),
            width: 2.0,
            x: 0.1,
            y: 0.2,
            pressure: 0.5,
            ts_ms: 1000,
        };
        let sig = sign_begin(&sk, &payload);
        let env = begin_envelope(host_uid, pk, sig, room_id, payload.clone());
        let mut state = sample_state(room_id, host_uid, pk);
        let out = handle_stroke_begin(env, &mut state, host_uid, pk, 1000).await;
        assert!(out.to_caller.is_empty());
        assert_eq!(out.events.len(), 1);
        assert!(state.drawing.pending.contains_key(&stroke_id));
    }

    #[tokio::test]
    async fn begin_with_bad_signature_is_rejected() {
        let room_id = Uuid::now_v7();
        let (sk, pk) = fresh_keypair();
        let host_uid = Uuid::now_v7();
        let payload = StrokeBeginPayload {
            stroke_id: Uuid::now_v7(),
            tool: locast_protocol::room::StrokeTool::Pen,
            color: "#000000".into(),
            width: 2.0,
            x: 0.1,
            y: 0.2,
            pressure: 0.5,
            ts_ms: 1000,
        };
        let _ = sign_begin(&sk, &payload);
        let bad_sig = [0xAAu8; 64];
        let env = begin_envelope(host_uid, pk, bad_sig, room_id, payload);
        let mut state = sample_state(room_id, host_uid, pk);
        let out = handle_stroke_begin(env, &mut state, host_uid, pk, 1000).await;
        assert_eq!(out.events.len(), 0);
        assert_eq!(out.to_caller.len(), 1);
        assert_eq!(out.to_caller[0].r#type, MessageKind::RoomError);
    }

    #[tokio::test]
    async fn begin_with_unsigned_envelope_is_rejected() {
        let room_id = Uuid::now_v7();
        let (_, pk) = fresh_keypair();
        let host_uid = Uuid::now_v7();
        let payload = StrokeBeginPayload {
            stroke_id: Uuid::now_v7(),
            tool: locast_protocol::room::StrokeTool::Pen,
            color: "#000000".into(),
            width: 2.0,
            x: 0.1,
            y: 0.2,
            pressure: 0.5,
            ts_ms: 1000,
        };
        let env = Envelope {
            v: 1,
            r#type: MessageKind::StrokeBegin,
            id: Uuid::now_v7(),
            room_id: Some(room_id),
            sender: None,
            ts_ms: 0,
            seq: 0,
            payload: serde_json::to_value(payload).expect("payload"),
        };
        let mut state = sample_state(room_id, host_uid, pk);
        let out = handle_stroke_begin(env, &mut state, host_uid, pk, 1000).await;
        assert_eq!(out.events.len(), 0);
        assert_eq!(out.to_caller.len(), 1);
    }

    #[tokio::test]
    async fn begin_with_out_of_range_coords_is_rejected() {
        let room_id = Uuid::now_v7();
        let (sk, pk) = fresh_keypair();
        let host_uid = Uuid::now_v7();
        let payload = StrokeBeginPayload {
            stroke_id: Uuid::now_v7(),
            tool: locast_protocol::room::StrokeTool::Pen,
            color: "#000000".into(),
            width: 2.0,
            x: 1.5, // out of range
            y: 0.5,
            pressure: 0.5,
            ts_ms: 1000,
        };
        let sig = sign_begin(&sk, &payload);
        let env = begin_envelope(host_uid, pk, sig, room_id, payload);
        let mut state = sample_state(room_id, host_uid, pk);
        let out = handle_stroke_begin(env, &mut state, host_uid, pk, 1000).await;
        assert_eq!(out.events.len(), 0);
        assert_eq!(out.to_caller.len(), 1);
    }

    #[tokio::test]
    async fn point_for_unknown_stroke_is_rejected() {
        let room_id = Uuid::now_v7();
        let (_, pk) = fresh_keypair();
        let host_uid = Uuid::now_v7();
        let mut state = sample_state(room_id, host_uid, pk);
        let payload = StrokePointPayload {
            stroke_id: Uuid::now_v7(),
            x: 0.5,
            y: 0.5,
            pressure: 0.5,
            ts_ms: 1000,
        };
        let env = Envelope {
            v: 1,
            r#type: MessageKind::StrokePoint,
            id: Uuid::now_v7(),
            room_id: Some(room_id),
            sender: None,
            ts_ms: 0,
            seq: 0,
            payload: serde_json::to_value(payload).expect("payload"),
        };
        let out = handle_stroke_point(env, &mut state, host_uid).await;
        assert_eq!(out.events.len(), 0);
        assert_eq!(out.to_caller.len(), 1);
    }

    #[tokio::test]
    async fn point_from_cross_sender_is_rejected() {
        let room_id = Uuid::now_v7();
        let (sk, pk) = fresh_keypair();
        let host_uid = Uuid::now_v7();
        let other_uid = Uuid::now_v7();
        let mut state = sample_state(room_id, host_uid, pk);
        let stroke_id = Uuid::now_v7();
        let begin_payload = StrokeBeginPayload {
            stroke_id,
            tool: locast_protocol::room::StrokeTool::Pen,
            color: "#000000".into(),
            width: 2.0,
            x: 0.1,
            y: 0.2,
            pressure: 0.5,
            ts_ms: 1000,
        };
        let sig = sign_begin(&sk, &begin_payload);
        let begin_env = begin_envelope(host_uid, pk, sig, room_id, begin_payload);
        let _ = handle_stroke_begin(begin_env, &mut state, host_uid, pk, 1000).await;

        // Now another user tries to append a point to that stroke.
        let point_payload = StrokePointPayload {
            stroke_id,
            x: 0.5,
            y: 0.5,
            pressure: 0.5,
            ts_ms: 1100,
        };
        let env = Envelope {
            v: 1,
            r#type: MessageKind::StrokePoint,
            id: Uuid::now_v7(),
            room_id: Some(room_id),
            sender: None,
            ts_ms: 0,
            seq: 0,
            payload: serde_json::to_value(point_payload).expect("payload"),
        };
        let out = handle_stroke_point(env, &mut state, other_uid).await;
        assert_eq!(out.events.len(), 0);
        assert_eq!(out.to_caller.len(), 1);
        // The stroke binding is unchanged.
        let binding = state.drawing.pending.get(&stroke_id).expect("bound");
        assert_eq!(binding.sender_id, host_uid);
    }

    /// POINT / END attribute the event to the stroke's recorded owner
    /// and never read `envelope.sender`, even when it names another user.
    #[tokio::test]
    async fn point_and_end_events_use_the_bound_owner_not_envelope_sender() {
        let room_id = Uuid::now_v7();
        let (sk, pk) = fresh_keypair();
        let host_uid = Uuid::now_v7();
        let spoofed_uid = Uuid::now_v7();
        let stroke_id = Uuid::now_v7();
        let mut state = sample_state(room_id, host_uid, pk);
        let begin_payload = StrokeBeginPayload {
            stroke_id,
            tool: locast_protocol::room::StrokeTool::Pen,
            color: "#000000".into(),
            width: 2.0,
            x: 0.1,
            y: 0.2,
            pressure: 0.5,
            ts_ms: 1000,
        };
        let sig = sign_begin(&sk, &begin_payload);
        let begin_env = begin_envelope(host_uid, pk, sig, room_id, begin_payload);
        let out = handle_stroke_begin(begin_env, &mut state, host_uid, pk, 1000).await;
        match &out.events[..] {
            [super::super::registry::RoomEvent::StrokeBegin { sender_id, .. }] => {
                assert_eq!(*sender_id, host_uid);
            }
            other => panic!("expected one StrokeBegin, got {other:?}"),
        }

        let spoof = Some(Sender {
            user_id: spoofed_uid,
            pubkey: vec![0x11; 32],
            sig: vec![0x22; 64],
        });
        let point_env = Envelope {
            v: 1,
            r#type: MessageKind::StrokePoint,
            id: Uuid::now_v7(),
            room_id: Some(room_id),
            sender: spoof.clone(),
            ts_ms: 0,
            seq: 0,
            payload: serde_json::to_value(StrokePointPayload {
                stroke_id,
                x: 0.5,
                y: 0.5,
                pressure: 0.5,
                ts_ms: 1100,
            })
            .expect("payload"),
        };
        let out = handle_stroke_point(point_env, &mut state, host_uid).await;
        match &out.events[..] {
            [super::super::registry::RoomEvent::StrokePoint { sender_id, .. }] => {
                assert_eq!(*sender_id, host_uid, "spoofed envelope sender ignored");
            }
            other => panic!("expected one StrokePoint, got {other:?}"),
        }

        let end_env = Envelope {
            v: 1,
            r#type: MessageKind::StrokeEnd,
            id: Uuid::now_v7(),
            room_id: Some(room_id),
            sender: spoof,
            ts_ms: 0,
            seq: 0,
            payload: serde_json::to_value(StrokeEndPayload {
                stroke_id,
                ts_ms: 1200,
            })
            .expect("payload"),
        };
        let out = handle_stroke_end(end_env, &mut state, host_uid).await;
        match &out.events[..] {
            [super::super::registry::RoomEvent::StrokeEnd { sender_id, .. }] => {
                assert_eq!(*sender_id, host_uid, "spoofed envelope sender ignored");
            }
            other => panic!("expected one StrokeEnd, got {other:?}"),
        }
    }

    /// A BEGIN whose signed `sender` names another user, or carries
    /// another user's key, is refused and binds nothing.
    #[tokio::test]
    async fn begin_with_a_spoofed_sender_is_rejected_and_binds_nothing() {
        let room_id = Uuid::now_v7();
        let (sk, pk) = fresh_keypair();
        let (_, other_pk) = fresh_keypair();
        let host_uid = Uuid::now_v7();
        let victim_uid = Uuid::now_v7();
        let mut state = sample_state(room_id, host_uid, pk);
        for (claimed_uid, claimed_pk) in [(victim_uid, pk), (host_uid, other_pk)] {
            let stroke_id = Uuid::now_v7();
            let payload = StrokeBeginPayload {
                stroke_id,
                tool: locast_protocol::room::StrokeTool::Pen,
                color: "#000000".into(),
                width: 2.0,
                x: 0.1,
                y: 0.2,
                pressure: 0.5,
                ts_ms: 1000,
            };
            let sig = sign_begin(&sk, &payload);
            let env = begin_envelope(claimed_uid, claimed_pk, sig, room_id, payload);
            let out = handle_stroke_begin(env, &mut state, host_uid, pk, 1000).await;
            assert!(out.events.is_empty());
            assert_eq!(out.to_caller.len(), 1);
            assert!(!state.drawing.pending.contains_key(&stroke_id));
        }
    }

    #[tokio::test]
    async fn end_removes_binding_and_emits_event() {
        let room_id = Uuid::now_v7();
        let (sk, pk) = fresh_keypair();
        let host_uid = Uuid::now_v7();
        let stroke_id = Uuid::now_v7();
        let mut state = sample_state(room_id, host_uid, pk);
        let begin_payload = StrokeBeginPayload {
            stroke_id,
            tool: locast_protocol::room::StrokeTool::Pen,
            color: "#000000".into(),
            width: 2.0,
            x: 0.1,
            y: 0.2,
            pressure: 0.5,
            ts_ms: 1000,
        };
        let sig = sign_begin(&sk, &begin_payload);
        let begin_env = begin_envelope(host_uid, pk, sig, room_id, begin_payload);
        let _ = handle_stroke_begin(begin_env, &mut state, host_uid, pk, 1000).await;
        assert!(state.drawing.pending.contains_key(&stroke_id));
        let end_payload = StrokeEndPayload {
            stroke_id,
            ts_ms: 1500,
        };
        let env = Envelope {
            v: 1,
            r#type: MessageKind::StrokeEnd,
            id: Uuid::now_v7(),
            room_id: Some(room_id),
            sender: None,
            ts_ms: 0,
            seq: 0,
            payload: serde_json::to_value(end_payload).expect("payload"),
        };
        let out = handle_stroke_end(env, &mut state, host_uid).await;
        assert_eq!(out.events.len(), 1);
        assert!(out.to_caller.is_empty());
        assert!(!state.drawing.pending.contains_key(&stroke_id));
    }
}
