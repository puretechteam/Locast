//! WebRTC SDP/ICE relay plumbing (P3-T05).
//!
//! The server is a pure relay for WebRTC SDP offers/answers and
//! ICE candidates. The room dispatcher hands the validated
//! SIGNAL envelope to [`handle_signal`], which looks up the
//! recipient's outbound channel in [`SignalRelay`] and pushes
//! the envelope into it. The receiving WS connection's idle
//! loop pops from its channel and writes the frame to the
//! socket.
//!
//! Registration happens at WS connect / AUTH_OK (the
//! connection's per-task `outbound_tx`). Unregistration
//! happens when the connection's `connection_loop` exits.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;
use uuid::Uuid;

use ed25519_dalek::{Signature, VerifyingKey};
use locast_protocol::envelope::Envelope;
use locast_protocol::room::SignalPayload;

use super::registry::RoomRegistry;
use crate::time::Clock;

/// Per-user outbound channel registry. Keyed by `user_id`
/// because each authenticated user owns at most one active
/// connection in v1 (a single desktop client). The channel is
/// unbounded because the WS layer has its own per-connection
/// rate limiter (`apps/server/src/ratelimit`) that throttles
/// the producer; a slow consumer causes the WS writer to
/// stall until the queue drains (same model as the existing
/// `room_bcast_forwarder` in `apps/server/src/ws/mod.rs:1144`).
#[derive(Default, Clone)]
pub struct SignalRelay {
    inner: Arc<tokio::sync::RwLock<HashMap<Uuid, mpsc::UnboundedSender<Envelope>>>>,
}

impl SignalRelay {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a connection's outbound channel under `user_id`.
    /// If a previous sender is registered (reconnect), it is
    /// replaced; the old sender is dropped, which causes the
    /// old WS task's recv to fail and exit cleanly.
    pub async fn register(&self, user_id: Uuid, tx: mpsc::UnboundedSender<Envelope>) {
        let mut g = self.inner.write().await;
        g.insert(user_id, tx);
    }

    /// Unregister a connection. Called from the WS task's
    /// shutdown path. No-op if `user_id` is not registered.
    pub async fn unregister(&self, user_id: Uuid) {
        let mut g = self.inner.write().await;
        g.remove(&user_id);
    }

    /// Send a SIGNAL envelope to `target_user_id`. Returns
    /// `Ok(())` if the envelope was enqueued; `Err(Envelope)`
    /// if no connection is registered for the target (caller
    /// should map this to `ROOM_ERROR(NotJoined)` or similar).
    pub async fn send(&self, target_user_id: Uuid, envelope: Envelope) -> Result<(), Envelope> {
        let g = self.inner.read().await;
        match g.get(&target_user_id) {
            Some(tx) => tx.send(envelope).map_err(|e| e.0),
            None => Err(envelope),
        }
    }

    /// Test/debug helper.
    pub async fn is_registered(&self, user_id: Uuid) -> bool {
        let g = self.inner.read().await;
        g.contains_key(&user_id)
    }
}

pub const SIGNAL_MAX_BYTES: usize = 64 * 1024;

/// Result of dispatching a SIGNAL envelope.
#[derive(Debug, Default)]
pub struct SignalOutcome {
    /// The envelope to deliver to the recipient (server
    /// forwards unchanged; the server does NOT inspect or
    /// rewrite SDP/ICE bodies).
    pub to_recipient: Option<Envelope>,
    /// Optional ROOM_ERROR envelope to send back to the caller.
    pub to_caller: Option<Envelope>,
}

/// The reason a SIGNAL was rejected. Maps to a wire-level
/// ROOM_ERROR code by `dispatch_room_message`.
#[derive(Debug, thiserror::Error)]
pub enum SignalError {
    #[error("missing sender signature")]
    MissingSender,
    #[error("sender identity does not match bearer")]
    IdentityMismatch,
    #[error("bad signature")]
    BadSignature,
    #[error("payload decode failed: {0}")]
    BadPayload(String),
    #[error("oversized signal envelope: {0} bytes (cap {SIGNAL_MAX_BYTES})")]
    Oversized(usize),
    #[error("sender is not a participant of the room")]
    SenderNotInRoom,
    #[error("recipient is not a participant of the room")]
    RecipientNotInRoom,
    #[error("cannot signal self")]
    SelfSignal,
}

/// Dispatch one SIGNAL envelope. The caller (the room
/// dispatcher) has already verified the bearer. This function
/// adds the per-envelope Ed25519 signature check, the
/// room-membership checks for both sender and recipient, the
/// 64 KiB app-layer size cap, and returns the envelope to
/// forward (or an error envelope to send back to the caller).
pub async fn handle_signal(
    envelope: Envelope,
    registry: &RoomRegistry,
    relay: &SignalRelay,
    clock: &dyn Clock,
    bearer_user_id: Uuid,
    bearer_pubkey: [u8; 32],
) -> SignalOutcome {
    let now_ms = clock.now_ms();

    // 1. App-layer size cap. The WS transport cap is 1 MiB
    //    (§18.5.1); SIGNAL must additionally fit within 64 KiB.
    let serialized_len = serde_json::to_vec(&envelope).map(|v| v.len()).unwrap_or(0);
    if serialized_len > SIGNAL_MAX_BYTES {
        return error_outcome(SignalError::Oversized(serialized_len), now_ms);
    }

    // 2. Decode the typed payload.
    let payload: SignalPayload = match serde_json::from_value(envelope.payload.clone()) {
        Ok(p) => p,
        Err(e) => return error_outcome(SignalError::BadPayload(e.to_string()), now_ms),
    };

    // 3. Sender identity (envelope.sender) must match the
    //    bearer. The bearer is authoritative; we never trust
    //    a client-supplied identity claim.
    let sender = match envelope.sender.as_ref() {
        Some(s) => s,
        None => return error_outcome(SignalError::MissingSender, now_ms),
    };
    if sender.user_id != bearer_user_id {
        return error_outcome(SignalError::IdentityMismatch, now_ms);
    }
    if sender.pubkey.as_slice() != bearer_pubkey.as_slice() {
        return error_outcome(SignalError::IdentityMismatch, now_ms);
    }

    // 4. Self-signal is nonsensical; reject.
    if payload.to_user_id == bearer_user_id {
        return error_outcome(SignalError::SelfSignal, now_ms);
    }

    // 5. Room-membership check. The caller's current room
    //    must equal envelope.room_id, and the recipient must
    //    be a current participant of the same room.
    let caller_room = match registry.get_user_room(bearer_user_id).await {
        Some(r) => r,
        None => return error_outcome(SignalError::SenderNotInRoom, now_ms),
    };
    let signal_room = match envelope.room_id {
        Some(r) => r,
        None => return error_outcome(SignalError::SenderNotInRoom, now_ms),
    };
    if caller_room != signal_room {
        return error_outcome(SignalError::SenderNotInRoom, now_ms);
    }
    if !registry.is_user_in_room(payload.to_user_id, signal_room).await {
        return error_outcome(SignalError::RecipientNotInRoom, now_ms);
    }

    // 6. Verify the Ed25519 signature over the
    //    domain-separated canonicalized payload bytes. The
    //    signature is on:
    //      domain_tag("SIGNAL") (16 bytes) || rmp_serde::to_vec_named(payload)
    //    Architecture §18.9; the shared helper
    //    `locast_crypto::signal_signed_bytes` is the single
    //    source of truth shared with the client.
    let signed_bytes = match locast_crypto::signal_signed_bytes(&payload) {
        Ok(b) => b,
        Err(_) => return error_outcome(SignalError::BadSignature, now_ms), // encoding should never fail
    };
    let pk_bytes: [u8; 32] = match sender.pubkey.as_slice().try_into() {
        Ok(b) => b,
        Err(_) => return error_outcome(SignalError::BadSignature, now_ms),
    };
    let sig_bytes: [u8; 64] = match sender.sig.as_slice().try_into() {
        Ok(s) => s,
        Err(_) => return error_outcome(SignalError::BadSignature, now_ms),
    };
    let vk = match VerifyingKey::from_bytes(&pk_bytes) {
        Ok(v) => v,
        Err(_) => return error_outcome(SignalError::BadSignature, now_ms),
    };
    let sig = Signature::from_bytes(&sig_bytes);
    if vk.verify_strict(&signed_bytes, &sig).is_err() {
        return error_outcome(SignalError::BadSignature, now_ms);
    }

    // 7. Forward the original envelope to the recipient via
    //    the SignalRelay. We do NOT inspect or rewrite the
    //    SDP / ICE bodies; we forward verbatim.
    match relay.send(payload.to_user_id, envelope).await {
        Ok(()) => SignalOutcome::default(),
        Err(_env) => {
            // Recipient has no active connection (disconnected
            // between our membership check and the relay send).
            // Surface as RecipientNotInRoom because the recipient
            // cannot receive right now.
            error_outcome(SignalError::RecipientNotInRoom, now_ms)
        }
    }
}

fn error_outcome(err: SignalError, now_ms: i64) -> SignalOutcome {
    let code = match &err {
        SignalError::MissingSender
        | SignalError::IdentityMismatch
        | SignalError::BadSignature
        | SignalError::BadPayload(_)
        | SignalError::Oversized(_)
        | SignalError::SelfSignal => locast_protocol::room::RoomErrorCode::InvalidState,
        SignalError::SenderNotInRoom | SignalError::RecipientNotInRoom => {
            locast_protocol::room::RoomErrorCode::NotJoined
        }
    };
    let env = locast_protocol::envelope::Envelope {
        v: 1,
        r#type: locast_protocol::envelope::MessageKind::RoomError,
        id: Uuid::now_v7(),
        room_id: None,
        sender: None,
        ts_ms: now_ms,
        seq: 0,
        payload: serde_json::to_value(locast_protocol::room::RoomErrorPayload {
            code,
            message: err.to_string(),
        })
        .unwrap_or(serde_json::json!({})),
    };
    SignalOutcome {
        to_recipient: None,
        to_caller: Some(env),
    }
}

// Canonicalized signed bytes for SIGNAL envelopes live in
// `locast_crypto::signal_signed_bytes` (shared with the
// client). See `shared/crypto/src/lib.rs`.