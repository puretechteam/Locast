//! Per-message dispatch for the ROOM_* and PRESENCE envelopes.
//!
//! The WS layer validates the bearer (P2-T02) and then calls
//! into this module. The result is a list of envelopes to
//! send to the caller and a list of `RoomEvent`s to broadcast
//! to other participants.

#![forbid(unsafe_code)]

use locast_protocol::envelope::{Envelope, MessageKind};
use locast_protocol::room::{
    ParticipantJoinedPayload, ParticipantLeftPayload, PresencePayload, RoomCreatePayload,
    RoomCreatedPayload, RoomErrorCode, RoomErrorPayload, RoomJoinRequestPayload, RoomJoinedPayload,
    RoomLeavePayload, RoomStatePayload,
};
use uuid::Uuid;

use super::codes;
use super::error::RoomError;
use super::registry::{RoomEvent, RoomRegistry};
use crate::time::Clock;
/// The result of dispatching a single ROOM_* message. The
/// WS layer applies the `to_caller` envelopes first (in
/// order) and then broadcasts the `events` to the room
/// (excluding the caller, where appropriate).
#[derive(Debug, Default)]
pub struct RoomDispatchOutcome {
    /// Envelopes to send back to the caller only.
    pub to_caller: Vec<Envelope>,
    /// Room-wide events to broadcast.
    pub events: Vec<RoomEvent>,
    /// When `true`, the WS layer should close the caller's
    /// connection.
    pub close_caller: bool,
}

/// Dispatch one envelope. `user_id` and `pubkey` come from
/// the validated bearer (P2-T02). `now_ms` is read from the
/// injected `clock` so tests can drive the registry's
/// grace / stale paths deterministically.
pub async fn dispatch_room_message(
    envelope: Envelope,
    registry: &RoomRegistry,
    clock: &dyn Clock,
    user_id: Uuid,
    pubkey: [u8; 32],
) -> RoomDispatchOutcome {
    let now_ms = clock.now_ms();
    match envelope.r#type {
        MessageKind::RoomCreate => {
            handle_room_create(envelope, registry, user_id, pubkey, now_ms).await
        }
        MessageKind::RoomJoinRequest => {
            handle_room_join_request(envelope, registry, user_id, pubkey, now_ms).await
        }
        MessageKind::RoomLeave => handle_room_leave(registry, user_id, now_ms).await,
        MessageKind::Presence => handle_presence(registry, user_id, now_ms).await,
        _ => RoomDispatchOutcome::default(),
    }
}

async fn handle_room_create(
    envelope: Envelope,
    registry: &RoomRegistry,
    user_id: Uuid,
    pubkey: [u8; 32],
    now_ms: i64,
) -> RoomDispatchOutcome {
    let payload_value = strip_bearer(envelope.payload);
    let payload: RoomCreatePayload = match serde_json::from_value(payload_value) {
        Ok(p) => p,
        Err(e) => {
            return RoomDispatchOutcome::from_room_error(
                RoomError::InvalidState,
                format!("bad ROOM_CREATE payload: {e}"),
            );
        }
    };
    let mut out = RoomDispatchOutcome::default();
    match registry
        .create(
            payload.title,
            user_id,
            pubkey,
            payload.migration_enabled,
            now_ms,
        )
        .await
    {
        Ok((summary, self_view)) => {
            let env = envelope_with_payload(
                MessageKind::RoomCreated,
                Some(summary.id),
                &RoomCreatedPayload {
                    room: summary,
                    you: self_view,
                },
                now_ms,
            );
            out.to_caller.push(env);
        }
        Err(e) => {
            let msg = e.to_string();
            out.to_caller
                .push(err_envelope(MessageKind::RoomError, e.into(), msg, now_ms));
        }
    }
    out
}

async fn handle_room_join_request(
    envelope: Envelope,
    registry: &RoomRegistry,
    user_id: Uuid,
    pubkey: [u8; 32],
    now_ms: i64,
) -> RoomDispatchOutcome {
    let payload_value = strip_bearer(envelope.payload);
    let payload: RoomJoinRequestPayload = match serde_json::from_value(payload_value) {
        Ok(p) => p,
        Err(e) => {
            return RoomDispatchOutcome::from_room_error(
                RoomError::InvalidState,
                format!("bad ROOM_JOIN_REQUEST payload: {e}"),
            );
        }
    };
    let mut out = RoomDispatchOutcome::default();
    let code = match codes::normalize(&payload.code) {
        Some(c) => c,
        None => {
            out.to_caller.push(err_envelope(
                MessageKind::RoomError,
                RoomErrorCode::InvalidCode,
                "invalid room code".to_string(),
                now_ms,
            ));
            return out;
        }
    };
    if payload.display_name.is_empty() || payload.display_name.len() > 32 {
        out.to_caller.push(err_envelope(
            MessageKind::RoomError,
            RoomErrorCode::InvalidState,
            "display_name must be 1-32 chars".to_string(),
            now_ms,
        ));
        return out;
    }
    match registry
        .join(&code, user_id, pubkey, payload.display_name, now_ms)
        .await
    {
        Ok((joined, evt)) => {
            let env = envelope_with_payload(
                MessageKind::RoomJoined,
                Some(joined.room.id),
                &joined,
                now_ms,
            );
            out.to_caller.push(env);
            out.events.push(evt);
        }
        Err(e) => {
            let msg = e.to_string();
            out.to_caller
                .push(err_envelope(MessageKind::RoomError, e.into(), msg, now_ms));
        }
    }
    out
}

/// Strip the `bearer` field from a payload before
/// deserializing it as a typed struct. The bearer has
/// already been validated by the WS layer; the per-type
/// payload structs don't (and shouldn't) know about it.
fn strip_bearer(mut v: serde_json::Value) -> serde_json::Value {
    if let Some(obj) = v.as_object_mut() {
        obj.remove("bearer");
    }
    v
}

async fn handle_room_leave(
    registry: &RoomRegistry,
    user_id: Uuid,
    now_ms: i64,
) -> RoomDispatchOutcome {
    let mut out = RoomDispatchOutcome::default();
    match registry.leave(user_id, true, now_ms).await {
        Ok((events, _summary)) => {
            out.events.extend(events);
        }
        Err(e) => {
            let msg = e.to_string();
            out.to_caller
                .push(err_envelope(MessageKind::RoomError, e.into(), msg, now_ms));
        }
    }
    out
}

async fn handle_presence(
    registry: &RoomRegistry,
    user_id: Uuid,
    now_ms: i64,
) -> RoomDispatchOutcome {
    registry.touch(user_id, now_ms).await;
    RoomDispatchOutcome::default()
}

impl RoomDispatchOutcome {
    fn from_room_error(code: RoomError, message: String) -> Self {
        let mut out = Self::default();
        out.to_caller.push(err_envelope(
            MessageKind::RoomError,
            code.into(),
            message,
            now_ms_i64(),
        ));
        out
    }
}

fn err_envelope(kind: MessageKind, code: RoomErrorCode, message: String, now_ms: i64) -> Envelope {
    envelope_with_payload(kind, None, &RoomErrorPayload { code, message }, now_ms)
}

fn envelope_with_payload<T: serde::Serialize>(
    kind: MessageKind,
    room_id: Option<Uuid>,
    payload: &T,
    now_ms: i64,
) -> Envelope {
    Envelope {
        v: 1,
        r#type: kind,
        id: Uuid::now_v7(),
        room_id,
        sender: None,
        ts_ms: now_ms,
        seq: 0,
        payload: serde_json::to_value(payload).unwrap_or(serde_json::json!({})),
    }
}

fn now_ms_i64() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[allow(dead_code)]
fn _ensure_types_used(
    _p: ParticipantJoinedPayload,
    _l: ParticipantLeftPayload,
    _pr: PresencePayload,
    _rl: RoomLeavePayload,
    _rj: RoomJoinedPayload,
    _rs: RoomStatePayload,
) {
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::MockClock;
    use locast_protocol::room::cap;

    fn fresh_registry() -> (RoomRegistry, MockClock) {
        let clock = MockClock::new(1_000_000);
        let cfg = super::super::registry::RoomRegistryConfig {
            max_participants: 8,
            host_disconnect_grace_ms: 200,
            participant_stale_after_ms: 300_000,
        };
        (RoomRegistry::new(cfg), clock)
    }

    fn pubkey() -> [u8; 32] {
        [7u8; 32]
    }

    fn uid(i: u8) -> Uuid {
        let mut b = [0u8; 16];
        b[0] = i;
        b[15] = i;
        Uuid::from_bytes(b)
    }

    #[tokio::test]
    async fn dispatch_room_create_sends_room_created() {
        let (reg, clock) = fresh_registry();
        let env = Envelope {
            v: 1,
            r#type: MessageKind::RoomCreate,
            id: Uuid::now_v7(),
            room_id: None,
            sender: None,
            ts_ms: clock.now_ms(),
            seq: 1,
            payload: serde_json::to_value(RoomCreatePayload {
                title: "T".into(),
                migration_enabled: true,
            })
            .unwrap(),
        };
        let out = dispatch_room_message(env, &reg, &clock, uid(1), pubkey()).await;
        assert_eq!(out.to_caller.len(), 1);
        assert_eq!(out.to_caller[0].r#type, MessageKind::RoomCreated);
    }

    #[tokio::test]
    async fn dispatch_invalid_code_yields_invalid_code_error() {
        let (reg, clock) = fresh_registry();
        let env = Envelope {
            v: 1,
            r#type: MessageKind::RoomJoinRequest,
            id: Uuid::now_v7(),
            room_id: None,
            sender: None,
            ts_ms: clock.now_ms(),
            seq: 1,
            payload: serde_json::to_value(RoomJoinRequestPayload {
                code: "BAD0".into(),
                display_name: "B".into(),
            })
            .unwrap(),
        };
        let out = dispatch_room_message(env, &reg, &clock, uid(1), pubkey()).await;
        assert_eq!(out.to_caller.len(), 1);
        let p: RoomErrorPayload = serde_json::from_value(out.to_caller[0].payload.clone()).unwrap();
        assert_eq!(p.code, RoomErrorCode::InvalidCode);
    }

    #[tokio::test]
    async fn presence_refreshes_last_seen() {
        let (reg, clock) = fresh_registry();
        let env = Envelope {
            v: 1,
            r#type: MessageKind::RoomCreate,
            id: Uuid::now_v7(),
            room_id: None,
            sender: None,
            ts_ms: clock.now_ms(),
            seq: 1,
            payload: serde_json::to_value(RoomCreatePayload {
                title: "T".into(),
                migration_enabled: true,
            })
            .unwrap(),
        };
        let _ = dispatch_room_message(env, &reg, &clock, uid(1), pubkey()).await;
        clock.advance(5_000);
        let env = Envelope {
            v: 1,
            r#type: MessageKind::Presence,
            id: Uuid::now_v7(),
            room_id: None,
            sender: None,
            ts_ms: clock.now_ms(),
            seq: 2,
            payload: serde_json::to_value(PresencePayload {
                status: "alive".into(),
            })
            .unwrap(),
        };
        let out = dispatch_room_message(env, &reg, &clock, uid(1), pubkey()).await;
        assert!(out.to_caller.is_empty());
        let snap = reg.list_snapshot(uid(1)).await.expect("snap");
        let me = snap
            .room
            .participants
            .iter()
            .find(|p| p.user_id == uid(1))
            .expect("me");
        assert!(me.last_seen_ms >= 1_005_000);
    }

    #[test]
    fn cap_constants_have_expected_bits() {
        assert_eq!(cap::CHAT, 0x80);
        assert_eq!(cap::PLAYBACK_CONTROL, 0x01);
    }
}
