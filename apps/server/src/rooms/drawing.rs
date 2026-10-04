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
//!
//! P5-T03 adds DRAW_UNDO and DRAW_CLEAR:
//!
//! - Committed strokes. DRAW_END moves a stroke from the
//!   pending map into the room's committed record
//!   (`stroke_id -> owner user_id`, bounded by
//!   `MAX_COMMITTED_STROKES`, oldest forgotten first). DRAW_UNDO
//!   is authorized against THAT record, looked up in the room
//!   the envelope names.
//! - DRAW_UNDO carries only a stroke id. The actor is the
//!   authenticated connection; the owner is the stored one.
//!   Owner == actor needs UNDO_OWN (or UNDO_ANY), anyone else's
//!   stroke needs UNDO_ANY. The capability is re-checked under
//!   the room lock. A stroke that is unknown, still in progress,
//!   already undone, cleared or forgotten is an idempotent
//!   no-op (nothing is mutated or broadcast, and no ROOM_ERROR
//!   is sent: the room client treats every unsolicited
//!   ROOM_ERROR as the end of the room, and these are expected
//!   races). A refusal (missing capability, or another user's
//!   stroke with only UNDO_OWN) is likewise silent and logged:
//!   the real client gates its UI on the same bits, so a
//!   refusal only happens after a revoke race.
//! - DRAW_CLEAR needs CLEAR_ALL (re-checked under the lock),
//!   empties the committed record and marks every in-progress
//!   stroke `cleared`: the drawer's remaining POINT / END are
//!   accepted silently (no ROOM_ERROR) but not rebroadcast or
//!   committed, so a cleared stroke cannot reappear.
//! - Accepted undo / clear events are rebroadcast to EVERY
//!   participant, the actor included, with the authenticated
//!   actor as `Envelope::sender`. The dispatcher publishes them
//!   (and all other drawing events) while still holding the
//!   room lock, so every client sees them in the order the
//!   server applied them.

#![forbid(unsafe_code)]

#[cfg(test)]
use ed25519_dalek::Signer;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use uuid::Uuid;

use locast_protocol::envelope::{Envelope, MessageKind, Sender};
use locast_protocol::room::{
    ParticipantStatus, StrokeBeginPayload, StrokeEndPayload, StrokePointPayload, StrokeUndoPayload,
};

use super::caps::{participant_can, Action, Scope};
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
    //
    // Also refuse an id that is already a committed stroke:
    // otherwise another participant could re-BEGIN and END a
    // stroke id the room already holds and overwrite its recorded
    // owner (and so become allowed to undo it as "their own").
    if state.drawing.pending.contains_key(&payload.stroke_id)
        || state.drawing.owner_of(&payload.stroke_id).is_some()
    {
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
            cleared: false,
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
    // A DRAW_CLEAR landed while this stroke was open: accept the
    // point (so the drawer is not answered with a ROOM_ERROR) but do
    // not rebroadcast it, or the cleared stroke would reappear.
    if binding.cleared {
        return RoomDispatchOutcome::default();
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
    let binding = state.drawing.pending.remove(&payload.stroke_id);
    if binding.is_some_and(|b| b.cleared) {
        // Cleared while open: closed silently, never committed (so it
        // cannot be undone later) and not rebroadcast.
        return RoomDispatchOutcome::default();
    }
    // The stroke is now committed: remember its owner so DRAW_UNDO
    // can be authorized against it.
    state.drawing.commit(payload.stroke_id, owner);
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

/// `true` if `user_id` has a live participant record in `state`
/// (connected or reconnecting) that may perform `action`.
fn actor_can(state: &RoomState, user_id: Uuid, action: Action) -> bool {
    state
        .participants
        .iter()
        .find(|p| p.user_id == user_id && p.status != ParticipantStatus::Left)
        .is_some_and(|p| {
            matches!(
                p.status,
                ParticipantStatus::Connected | ParticipantStatus::Reconnecting
            ) && participant_can(p, Scope::Drawing, action)
        })
}

/// P5-T03: validate DRAW_UNDO.
///
/// `user_id` is the authenticated connection (never read from the
/// envelope or payload). The stroke is looked up only in `state`,
/// the room the envelope names. See the module docs for the no-op
/// and refusal rules.
pub async fn handle_stroke_undo(
    envelope: Envelope,
    state: &mut RoomState,
    user_id: Uuid,
) -> RoomDispatchOutcome {
    let payload: StrokeUndoPayload = match serde_json::from_value(envelope.payload.clone()) {
        Ok(p) => p,
        Err(e) => return err_outcome(&envelope, format!("bad DRAW_UNDO payload: {e}")),
    };
    let Some(owner) = state.drawing.owner_of(&payload.stroke_id) else {
        // Unknown, in progress, already undone, cleared, forgotten, or
        // a stroke of another room: idempotent no-op.
        tracing::debug!(
            room_id = %state.id,
            user_id = %user_id,
            stroke_id = %payload.stroke_id,
            "DRAW_UNDO for a stroke the room does not hold; ignored"
        );
        return RoomDispatchOutcome::default();
    };
    let allowed = if owner == user_id {
        actor_can(state, user_id, Action::UndoOwnStroke)
            || actor_can(state, user_id, Action::UndoAnyStroke)
    } else {
        actor_can(state, user_id, Action::UndoAnyStroke)
    };
    if !allowed {
        tracing::debug!(
            room_id = %state.id,
            user_id = %user_id,
            stroke_id = %payload.stroke_id,
            own_stroke = owner == user_id,
            "DRAW_UNDO refused: capability not held; room state unchanged"
        );
        return RoomDispatchOutcome::default();
    }
    state.drawing.remove_committed(&payload.stroke_id);
    RoomDispatchOutcome {
        to_caller: Vec::new(),
        events: vec![super::registry::RoomEvent::StrokeUndo {
            room_id: state.id,
            actor_id: user_id,
            payload,
        }],
        close_caller: false,
    }
}

/// P5-T03: validate DRAW_CLEAR. Requires CLEAR_ALL (re-checked here
/// under the room lock); a caller without it is refused silently and
/// the room is not touched.
pub async fn handle_stroke_clear(state: &mut RoomState, user_id: Uuid) -> RoomDispatchOutcome {
    if !actor_can(state, user_id, Action::ClearAll) {
        tracing::debug!(
            room_id = %state.id,
            user_id = %user_id,
            "DRAW_CLEAR refused: capability not held; room state unchanged"
        );
        return RoomDispatchOutcome::default();
    }
    let cleared = state.drawing.clear_all();
    tracing::debug!(room_id = %state.id, cleared, "DRAW_CLEAR applied");
    RoomDispatchOutcome {
        to_caller: Vec::new(),
        events: vec![super::registry::RoomEvent::StrokeClear {
            room_id: state.id,
            actor_id: user_id,
        }],
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

    // ------------------------------------------------------------
    // P5-T03: DRAW_UNDO / DRAW_CLEAR
    // ------------------------------------------------------------

    use super::super::registry::RoomEvent;
    use super::super::state::ParticipantRecord;
    use locast_protocol::room::cap;

    struct Room {
        state: RoomState,
        host: Uuid,
        host_sk: SigningKey,
        host_pk: [u8; 32],
    }

    fn room() -> Room {
        let (host_sk, host_pk) = fresh_keypair();
        let host = Uuid::now_v7();
        Room {
            state: sample_state(Uuid::now_v7(), host, host_pk),
            host,
            host_sk,
            host_pk,
        }
    }

    fn add_member(state: &mut RoomState, caps: u32) -> Uuid {
        let uid = Uuid::now_v7();
        state.participants.push(ParticipantRecord {
            user_id: uid,
            pubkey: [9; 32],
            display_name: "m".into(),
            joined_ms: 0,
            status: ParticipantStatus::Connected,
            last_seen_ms: 0,
            is_host: false,
            cap_set: caps,
        });
        uid
    }

    fn set_caps(state: &mut RoomState, uid: Uuid, caps: u32) {
        state
            .participants
            .iter_mut()
            .find(|p| p.user_id == uid)
            .expect("member")
            .cap_set = caps;
    }

    fn begin_payload(stroke_id: Uuid) -> StrokeBeginPayload {
        StrokeBeginPayload {
            stroke_id,
            tool: locast_protocol::room::StrokeTool::Pen,
            color: "#000000".into(),
            width: 2.0,
            x: 0.1,
            y: 0.2,
            pressure: 0.5,
            ts_ms: 1000,
        }
    }

    fn plain(kind: MessageKind, room_id: Uuid, payload: serde_json::Value) -> Envelope {
        Envelope {
            v: 1,
            r#type: kind,
            id: Uuid::now_v7(),
            room_id: Some(room_id),
            sender: None,
            ts_ms: 0,
            seq: 0,
            payload,
        }
    }

    /// Draw one complete stroke as the host (BEGIN then END).
    async fn host_stroke(r: &mut Room) -> Uuid {
        let stroke_id = Uuid::now_v7();
        let payload = begin_payload(stroke_id);
        let sig = sign_begin(&r.host_sk, &payload);
        let env = begin_envelope(r.host, r.host_pk, sig, r.state.id, payload);
        let out = handle_stroke_begin(env, &mut r.state, r.host, r.host_pk, 1).await;
        assert_eq!(out.events.len(), 1);
        end_stroke(&mut r.state, r.host, stroke_id).await;
        stroke_id
    }

    async fn end_stroke(state: &mut RoomState, uid: Uuid, stroke_id: Uuid) -> RoomDispatchOutcome {
        let env = plain(
            MessageKind::StrokeEnd,
            state.id,
            serde_json::to_value(StrokeEndPayload {
                stroke_id,
                ts_ms: 2,
            })
            .unwrap(),
        );
        handle_stroke_end(env, state, uid).await
    }

    fn undo_env(room_id: Uuid, stroke_id: Uuid) -> Envelope {
        plain(
            MessageKind::StrokeUndo,
            room_id,
            serde_json::to_value(StrokeUndoPayload { stroke_id }).unwrap(),
        )
    }

    #[tokio::test]
    async fn ended_strokes_are_recorded_with_their_owner_and_pending_ones_are_not() {
        let mut r = room();
        let stroke_id = Uuid::now_v7();
        let payload = begin_payload(stroke_id);
        let sig = sign_begin(&r.host_sk, &payload);
        let env = begin_envelope(r.host, r.host_pk, sig, r.state.id, payload);
        handle_stroke_begin(env, &mut r.state, r.host, r.host_pk, 1).await;
        assert_eq!(
            r.state.drawing.owner_of(&stroke_id),
            None,
            "still in progress"
        );
        end_stroke(&mut r.state, r.host, stroke_id).await;
        assert_eq!(r.state.drawing.owner_of(&stroke_id), Some(r.host));
        assert!(!r.state.drawing.pending.contains_key(&stroke_id));
    }

    #[tokio::test]
    async fn own_undo_needs_undo_own_and_removes_the_stroke_with_the_actor_stamped() {
        let mut r = room();
        let member = add_member(&mut r.state, cap::CHAT | cap::DRAW);
        // The member draws a stroke (BEGIN needs a real signature).
        let (sk, pk) = fresh_keypair();
        let stroke_id = Uuid::now_v7();
        let payload = begin_payload(stroke_id);
        let sig = sign_begin(&sk, &payload);
        let env = begin_envelope(member, pk, sig, r.state.id, payload);
        handle_stroke_begin(env, &mut r.state, member, pk, 1).await;
        end_stroke(&mut r.state, member, stroke_id).await;

        // No undo bit: refused, state unchanged.
        let out = handle_stroke_undo(undo_env(r.state.id, stroke_id), &mut r.state, member).await;
        assert!(out.events.is_empty() && out.to_caller.is_empty());
        assert_eq!(r.state.drawing.owner_of(&stroke_id), Some(member));

        // undo_own: allowed; the event names the AUTHENTICATED actor.
        set_caps(&mut r.state, member, cap::CHAT | cap::DRAW | cap::UNDO_OWN);
        let out = handle_stroke_undo(undo_env(r.state.id, stroke_id), &mut r.state, member).await;
        match &out.events[..] {
            [RoomEvent::StrokeUndo {
                room_id,
                actor_id,
                payload,
            }] => {
                assert_eq!(*room_id, r.state.id);
                assert_eq!(*actor_id, member);
                assert_eq!(payload.stroke_id, stroke_id);
            }
            other => panic!("expected one StrokeUndo, got {other:?}"),
        }
        assert_eq!(r.state.drawing.owner_of(&stroke_id), None);

        // A replayed undo is a harmless no-op.
        let out = handle_stroke_undo(undo_env(r.state.id, stroke_id), &mut r.state, member).await;
        assert!(out.events.is_empty() && out.to_caller.is_empty());
    }

    #[tokio::test]
    async fn undoing_another_users_stroke_needs_undo_any() {
        let mut r = room();
        let stroke_id = host_stroke(&mut r).await;
        let other = add_member(&mut r.state, cap::CHAT | cap::UNDO_OWN);
        // undo_own is not enough for someone else's stroke.
        let out = handle_stroke_undo(undo_env(r.state.id, stroke_id), &mut r.state, other).await;
        assert!(out.events.is_empty() && out.to_caller.is_empty());
        assert_eq!(
            r.state.drawing.owner_of(&stroke_id),
            Some(r.host),
            "unchanged"
        );
        // clear_all is not enough either.
        set_caps(&mut r.state, other, cap::CHAT | cap::CLEAR_ALL);
        let out = handle_stroke_undo(undo_env(r.state.id, stroke_id), &mut r.state, other).await;
        assert!(out.events.is_empty());
        assert_eq!(r.state.drawing.owner_of(&stroke_id), Some(r.host));
        // undo_any is.
        set_caps(&mut r.state, other, cap::CHAT | cap::UNDO_ANY);
        let out = handle_stroke_undo(undo_env(r.state.id, stroke_id), &mut r.state, other).await;
        assert_eq!(out.events.len(), 1);
        assert_eq!(r.state.drawing.owner_of(&stroke_id), None);
    }

    #[tokio::test]
    async fn undo_any_alone_may_also_undo_the_actors_own_stroke() {
        let mut r = room();
        let stroke_id = host_stroke(&mut r).await;
        // Hand the host's stroke to a member by recording it directly.
        let member = add_member(&mut r.state, cap::CHAT | cap::UNDO_ANY);
        let mine = Uuid::now_v7();
        r.state.drawing.commit(mine, member);
        let out = handle_stroke_undo(undo_env(r.state.id, mine), &mut r.state, member).await;
        assert_eq!(out.events.len(), 1);
        assert_eq!(r.state.drawing.owner_of(&stroke_id), Some(r.host));
    }

    #[tokio::test]
    async fn unknown_in_progress_and_removed_strokes_are_silent_no_ops() {
        let mut r = room();
        // Unknown id.
        let out =
            handle_stroke_undo(undo_env(r.state.id, Uuid::now_v7()), &mut r.state, r.host).await;
        assert!(out.events.is_empty() && out.to_caller.is_empty());
        // In progress.
        let open = Uuid::now_v7();
        let payload = begin_payload(open);
        let sig = sign_begin(&r.host_sk, &payload);
        let env = begin_envelope(r.host, r.host_pk, sig, r.state.id, payload);
        handle_stroke_begin(env, &mut r.state, r.host, r.host_pk, 1).await;
        let out = handle_stroke_undo(undo_env(r.state.id, open), &mut r.state, r.host).await;
        assert!(out.events.is_empty() && out.to_caller.is_empty());
        assert!(
            r.state.drawing.pending.contains_key(&open),
            "pending untouched"
        );
        // After END it can be undone, exactly once.
        end_stroke(&mut r.state, r.host, open).await;
        assert_eq!(
            handle_stroke_undo(undo_env(r.state.id, open), &mut r.state, r.host)
                .await
                .events
                .len(),
            1
        );
        assert!(
            handle_stroke_undo(undo_env(r.state.id, open), &mut r.state, r.host)
                .await
                .events
                .is_empty()
        );
    }

    #[tokio::test]
    async fn undo_only_sees_strokes_of_the_room_it_is_dispatched_in() {
        let mut room_a = room();
        let mut room_b = room();
        let stroke_in_a = host_stroke(&mut room_a).await;
        // Same user id, same stroke id, but room B's state: nothing there.
        let out = handle_stroke_undo(
            undo_env(room_b.state.id, stroke_in_a),
            &mut room_b.state,
            room_b.host,
        )
        .await;
        assert!(out.events.is_empty() && out.to_caller.is_empty());
        assert_eq!(
            room_a.state.drawing.owner_of(&stroke_in_a),
            Some(room_a.host)
        );
    }

    #[tokio::test]
    async fn undo_payload_identity_fields_are_ignored() {
        let mut r = room();
        let stroke_id = host_stroke(&mut r).await;
        let member = add_member(&mut r.state, cap::CHAT | cap::UNDO_OWN);
        let spoof = plain(
            MessageKind::StrokeUndo,
            r.state.id,
            serde_json::json!({
                "stroke_id": stroke_id,
                "user_id": r.host,
                "owner": member,
                "actor_id": r.host,
            }),
        );
        // `member` is the connection; claiming to be the host or the
        // owner in the payload changes nothing: still refused.
        let out = handle_stroke_undo(spoof, &mut r.state, member).await;
        assert!(out.events.is_empty());
        assert_eq!(r.state.drawing.owner_of(&stroke_id), Some(r.host));
    }

    #[tokio::test]
    async fn a_malformed_undo_payload_is_an_error_not_a_panic() {
        let mut r = room();
        let env = plain(MessageKind::StrokeUndo, r.state.id, serde_json::json!({}));
        let out = handle_stroke_undo(env, &mut r.state, r.host).await;
        assert!(out.events.is_empty());
        assert_eq!(out.to_caller.len(), 1);
    }

    #[tokio::test]
    async fn begin_cannot_take_over_a_committed_stroke_id() {
        let mut r = room();
        let victim_stroke = host_stroke(&mut r).await;
        let (sk, pk) = fresh_keypair();
        let attacker = add_member(&mut r.state, cap::CHAT | cap::DRAW | cap::UNDO_OWN);
        let payload = begin_payload(victim_stroke);
        let sig = sign_begin(&sk, &payload);
        let env = begin_envelope(attacker, pk, sig, r.state.id, payload);
        let out = handle_stroke_begin(env, &mut r.state, attacker, pk, 5).await;
        assert!(out.events.is_empty());
        assert_eq!(
            out.to_caller.len(),
            1,
            "re-BEGIN of a committed id is refused"
        );
        assert_eq!(r.state.drawing.owner_of(&victim_stroke), Some(r.host));
        // So the attacker cannot undo it as "their own".
        let out =
            handle_stroke_undo(undo_env(r.state.id, victim_stroke), &mut r.state, attacker).await;
        assert!(out.events.is_empty());
        assert_eq!(r.state.drawing.owner_of(&victim_stroke), Some(r.host));
    }

    #[tokio::test]
    async fn clear_requires_clear_all_and_never_mutates_without_it() {
        let mut r = room();
        let s1 = host_stroke(&mut r).await;
        let s2 = host_stroke(&mut r).await;
        let member = add_member(
            &mut r.state,
            cap::CHAT | cap::DRAW | cap::UNDO_ANY | cap::UNDO_OWN,
        );
        let out = handle_stroke_clear(&mut r.state, member).await;
        assert!(out.events.is_empty() && out.to_caller.is_empty());
        assert_eq!(r.state.drawing.committed_len(), 2, "state intact");

        set_caps(&mut r.state, member, cap::CHAT | cap::CLEAR_ALL);
        let out = handle_stroke_clear(&mut r.state, member).await;
        match &out.events[..] {
            [RoomEvent::StrokeClear { room_id, actor_id }] => {
                assert_eq!(*room_id, r.state.id);
                assert_eq!(*actor_id, member);
            }
            other => panic!("expected one StrokeClear, got {other:?}"),
        }
        assert_eq!(r.state.drawing.committed_len(), 0);
        // A later undo of a cleared stroke is a harmless no-op.
        for s in [s1, s2] {
            let out = handle_stroke_undo(undo_env(r.state.id, s), &mut r.state, r.host).await;
            assert!(out.events.is_empty() && out.to_caller.is_empty());
        }
        // Replayed clear is idempotent (still broadcast, nothing to remove).
        assert_eq!(
            handle_stroke_clear(&mut r.state, r.host).await.events.len(),
            1
        );
    }

    #[tokio::test]
    async fn a_stroke_open_during_clear_never_reappears_and_does_not_error() {
        let mut r = room();
        let open = Uuid::now_v7();
        let payload = begin_payload(open);
        let sig = sign_begin(&r.host_sk, &payload);
        let env = begin_envelope(r.host, r.host_pk, sig, r.state.id, payload);
        handle_stroke_begin(env, &mut r.state, r.host, r.host_pk, 1).await;
        assert_eq!(
            handle_stroke_clear(&mut r.state, r.host).await.events.len(),
            1
        );

        // The drawer's remaining POINT and END are accepted silently:
        // no error (which would evict the drawer's client), no
        // rebroadcast, nothing committed.
        let point = plain(
            MessageKind::StrokePoint,
            r.state.id,
            serde_json::to_value(StrokePointPayload {
                stroke_id: open,
                x: 0.5,
                y: 0.5,
                pressure: 0.5,
                ts_ms: 3,
            })
            .unwrap(),
        );
        let out = handle_stroke_point(point, &mut r.state, r.host).await;
        assert!(out.events.is_empty() && out.to_caller.is_empty());
        let out = end_stroke(&mut r.state, r.host, open).await;
        assert!(out.events.is_empty() && out.to_caller.is_empty());
        assert!(!r.state.drawing.pending.contains_key(&open));
        assert_eq!(r.state.drawing.owner_of(&open), None, "not undoable");
        assert_eq!(r.state.drawing.committed_len(), 0);
        // A stroke begun AFTER the clear is normal again.
        let after = host_stroke(&mut r).await;
        assert_eq!(r.state.drawing.owner_of(&after), Some(r.host));
    }

    #[tokio::test]
    async fn a_revoked_or_departed_actor_is_refused_under_the_lock() {
        let mut r = room();
        let stroke_id = host_stroke(&mut r).await;
        let member = add_member(&mut r.state, cap::CHAT | cap::UNDO_ANY | cap::CLEAR_ALL);
        // Revoked between the gate and the lock.
        set_caps(&mut r.state, member, cap::CHAT);
        assert!(
            handle_stroke_undo(undo_env(r.state.id, stroke_id), &mut r.state, member)
                .await
                .events
                .is_empty()
        );
        assert!(handle_stroke_clear(&mut r.state, member)
            .await
            .events
            .is_empty());
        // Left the room: capabilities no longer count.
        set_caps(
            &mut r.state,
            member,
            cap::CHAT | cap::UNDO_ANY | cap::CLEAR_ALL,
        );
        r.state
            .participants
            .iter_mut()
            .find(|p| p.user_id == member)
            .unwrap()
            .status = ParticipantStatus::Left;
        assert!(
            handle_stroke_undo(undo_env(r.state.id, stroke_id), &mut r.state, member)
                .await
                .events
                .is_empty()
        );
        assert!(handle_stroke_clear(&mut r.state, member)
            .await
            .events
            .is_empty());
        assert_eq!(r.state.drawing.owner_of(&stroke_id), Some(r.host));
    }
}
