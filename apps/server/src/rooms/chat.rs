//! P6-T03: server-relayed chat handler.
//!
//! Wire shape: `shared/protocol/src/envelope.rs` defines
//! `MessageKind::ChatMessage`; `shared/protocol/src/room.rs`
//! defines the `ChatPayload` struct.
//!
//! Authorization flow:
//!
//! 1. The dispatcher calls `check_capability(..., Command::ChatMessage)`
//!    which verifies the caller is a joined participant of the room.
//! 2. This handler then validates:
//!    - `text.len() <= 2048` (2 KiB schema-level rejection)
//!    - `reply_to` is `None` or refers to a valid participant in the room
//! 3. A `RoomEvent::ChatMessage` is broadcast to all participants.

#![forbid(unsafe_code)]

use locast_protocol::envelope::Envelope;
use locast_protocol::room::ChatPayload;
use serde::de::DeserializeOwned;
use uuid::Uuid;

use super::caps::{can, Action, Scope};
use super::error::RoomError;
use super::registry::{ChatMessage, RoomEvent, RoomRegistry};
use super::store::RoomStore;

const MAX_TEXT_LEN: usize = 2048;

fn decode_payload<T: DeserializeOwned>(value: &serde_json::Value) -> Result<T, RoomError> {
    serde_json::from_value(value.clone())
        .map_err(|e| RoomError::Internal(format!("chat payload: {}", e)))
}

/// Handle a CHAT_MESSAGE envelope from a room participant.
///
/// The dispatcher has already verified the caller is a joined
/// participant via `check_capability(..., Command::ChatMessage)`.
/// This handler validates the 2 KiB text limit and broadcasts
/// the chat message to all room participants.
pub async fn handle_chat_message(
    envelope: Envelope,
    registry: &RoomRegistry,
    _store: &dyn RoomStore,
    user_id: Uuid,
    now_ms: i64,
) -> Result<Vec<RoomEvent>, RoomError> {
    let room_id = envelope
        .room_id
        .ok_or_else(|| RoomError::Internal("CHAT_MESSAGE missing room_id".into()))?;

    let payload: ChatPayload = decode_payload(&envelope.payload)?;

    handle_chat_message_payload(room_id, payload, registry, user_id, now_ms).await
}

async fn handle_chat_message_payload(
    room_id: Uuid,
    payload: ChatPayload,
    registry: &RoomRegistry,
    user_id: Uuid,
    now_ms: i64,
) -> Result<Vec<RoomEvent>, RoomError> {
    if payload.text.len() > MAX_TEXT_LEN {
        return Err(RoomError::Internal(format!(
            "chat message text exceeds {} byte limit (got {})",
            MAX_TEXT_LEN,
            payload.text.len()
        )));
    }

    if !can(registry, user_id, room_id, Scope::Chat, Action::SendChat).await {
        return Err(RoomError::Internal("chat: not authorized to send".into()));
    }

    let handle = registry
        .get_by_id(room_id)
        .await
        .ok_or(RoomError::RoomNotFound)?;

    let state = handle.read().await;
    if !state
        .participants
        .iter()
        .any(|p| p.user_id == user_id && p.status != locast_protocol::room::ParticipantStatus::Left)
    {
        return Err(RoomError::NotJoined);
    }

    if let Some(reply_to) = payload.reply_to {
        if !state.participants.iter().any(|p| p.user_id == reply_to) {
            return Err(RoomError::Internal(
                "chat: reply_to references unknown participant".into(),
            ));
        }
    }

    tracing::debug!(user_id = %user_id, room_id = %room_id, text_len = payload.text.len(), "chat message accepted");

    Ok(vec![RoomEvent::ChatMessage(ChatMessage {
        room_id,
        sender_id: user_id,
        text: payload.text,
        reply_to: payload.reply_to,
        sent_ms: now_ms,
    })])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rooms::registry::RoomRegistryConfig;
    use crate::time::{Clock, MockClock};
    use locast_protocol::envelope::Envelope;

    fn fresh_registry() -> (RoomRegistry, MockClock) {
        let clock = MockClock::new(1_000_000);
        let cfg = RoomRegistryConfig {
            max_participants: 8,
            host_disconnect_grace_ms: 200,
            participant_stale_after_ms: 300_000,
            participant_disconnect_after_ms: 15_000,
        };
        (RoomRegistry::new(cfg), clock)
    }

    fn uid(i: u8) -> Uuid {
        let mut b = [0u8; 16];
        b[0] = i;
        b[15] = i;
        Uuid::from_bytes(b)
    }

    async fn setup_room_with_host_and_viewer(
        reg: &RoomRegistry,
        clock: &MockClock,
    ) -> (Uuid, Uuid, Uuid) {
        use super::super::store::NoopRoomStore;
        let s = NoopRoomStore;
        let (room, _self_view) = reg
            .create(&s, "T".into(), uid(1), [1u8; 32], true, clock.now_ms())
            .await
            .expect("create room");
        reg.join(
            &s,
            &room.code,
            uid(2),
            [2u8; 32],
            "viewer".into(),
            clock.now_ms(),
        )
        .await
        .expect("viewer joins");
        (room.id, uid(1), uid(2))
    }

    fn envelope(room_id: Uuid, _user_id: Uuid, payload: ChatPayload) -> Envelope {
        Envelope {
            v: 1,
            r#type: locast_protocol::envelope::MessageKind::ChatMessage,
            id: Uuid::now_v7(),
            room_id: Some(room_id),
            sender: None,
            ts_ms: 1_000_000,
            seq: 1,
            payload: serde_json::to_value(payload).unwrap(),
        }
    }

    #[tokio::test]
    async fn member_sends_chat_broadcasts_to_all() {
        let (reg, clock) = fresh_registry();
        let s = super::super::store::NoopRoomStore;
        let (room_id, _host_id, viewer_id) = setup_room_with_host_and_viewer(&reg, &clock).await;

        let payload = ChatPayload {
            sender_id: viewer_id,
            text: "Hello everyone!".to_string(),
            reply_to: None,
            sent_ms: clock.now_ms(),
        };
        let env = envelope(room_id, viewer_id, payload);

        let events = handle_chat_message(env, &reg, &s, viewer_id, clock.now_ms())
            .await
            .expect("handle_chat_message should succeed");

        assert_eq!(events.len(), 1);
        match &events[0] {
            RoomEvent::ChatMessage(msg) => {
                assert_eq!(msg.room_id, room_id);
                assert_eq!(msg.sender_id, viewer_id);
                assert_eq!(msg.text, "Hello everyone!");
                assert!(msg.reply_to.is_none());
            }
            _ => panic!("expected ChatMessage event"),
        }
    }

    #[tokio::test]
    async fn text_exceeds_2kib_rejected() {
        let (reg, clock) = fresh_registry();
        let s = super::super::store::NoopRoomStore;
        let (room_id, _host_id, viewer_id) = setup_room_with_host_and_viewer(&reg, &clock).await;

        let oversized_text = "x".repeat(2049);
        let payload = ChatPayload {
            sender_id: viewer_id,
            text: oversized_text,
            reply_to: None,
            sent_ms: clock.now_ms(),
        };
        let env = envelope(room_id, viewer_id, payload);

        let err = handle_chat_message(env, &reg, &s, viewer_id, clock.now_ms())
            .await
            .expect_err("should reject oversized message");
        assert!(matches!(err, RoomError::Internal(ref msg) if msg.contains("exceeds")));
    }

    #[tokio::test]
    async fn text_at_2kib_boundary_accepted() {
        let (reg, clock) = fresh_registry();
        let s = super::super::store::NoopRoomStore;
        let (room_id, _host_id, viewer_id) = setup_room_with_host_and_viewer(&reg, &clock).await;

        let exact_text = "x".repeat(2048);
        let payload = ChatPayload {
            sender_id: viewer_id,
            text: exact_text,
            reply_to: None,
            sent_ms: clock.now_ms(),
        };
        let env = envelope(room_id, viewer_id, payload);

        let events = handle_chat_message(env, &reg, &s, viewer_id, clock.now_ms())
            .await
            .expect("2 KiB message should be accepted");
        assert_eq!(events.len(), 1);
    }

    #[tokio::test]
    async fn without_chat_cap_rejected_via_can() {
        let (reg, clock) = fresh_registry();
        let s = super::super::store::NoopRoomStore;
        let (room_id, _host_id, viewer_id) = setup_room_with_host_and_viewer(&reg, &clock).await;

        {
            let handle = reg.get_by_id(room_id).await.expect("room");
            let mut state = handle.write().await;
            if let Some(p) = state
                .participants
                .iter_mut()
                .find(|p| p.user_id == viewer_id)
            {
                p.cap_set = 0;
            }
        }

        let payload = ChatPayload {
            sender_id: viewer_id,
            text: "Hello".to_string(),
            reply_to: None,
            sent_ms: clock.now_ms(),
        };
        let env = envelope(room_id, viewer_id, payload);

        let err = handle_chat_message(env, &reg, &s, viewer_id, clock.now_ms())
            .await
            .expect_err("should reject without CHAT cap");
        assert!(matches!(err, RoomError::Internal(ref msg) if msg.contains("not authorized")));
    }

    #[tokio::test]
    async fn host_with_chat_cap_can_send() {
        let (reg, clock) = fresh_registry();
        let s = super::super::store::NoopRoomStore;
        let (room_id, host_id, _viewer_id) = setup_room_with_host_and_viewer(&reg, &clock).await;

        let payload = ChatPayload {
            sender_id: host_id,
            text: "Host greeting".to_string(),
            reply_to: None,
            sent_ms: clock.now_ms(),
        };
        let env = envelope(room_id, host_id, payload);

        let events = handle_chat_message(env, &reg, &s, host_id, clock.now_ms())
            .await
            .expect("host should be able to send chat");
        assert_eq!(events.len(), 1);
        match &events[0] {
            RoomEvent::ChatMessage(msg) => {
                assert_eq!(msg.sender_id, host_id);
                assert_eq!(msg.text, "Host greeting");
            }
            _ => panic!("expected ChatMessage event"),
        }
    }

    #[tokio::test]
    async fn with_reply_to_preserved_in_broadcast() {
        let (reg, clock) = fresh_registry();
        let s = super::super::store::NoopRoomStore;
        let (room_id, _host_id, viewer_id) = setup_room_with_host_and_viewer(&reg, &clock).await;

        let payload = ChatPayload {
            sender_id: viewer_id,
            text: "Replying to host".to_string(),
            reply_to: Some(uid(1)),
            sent_ms: clock.now_ms(),
        };
        let env = envelope(room_id, viewer_id, payload);

        let events = handle_chat_message(env, &reg, &s, viewer_id, clock.now_ms())
            .await
            .expect("handle_chat_message should succeed");

        assert_eq!(events.len(), 1);
        match &events[0] {
            RoomEvent::ChatMessage(msg) => {
                assert_eq!(msg.reply_to, Some(uid(1)));
            }
            _ => panic!("expected ChatMessage event"),
        }
    }

    #[tokio::test]
    async fn reply_to_unknown_user_rejected() {
        let (reg, clock) = fresh_registry();
        let s = super::super::store::NoopRoomStore;
        let (room_id, _host_id, viewer_id) = setup_room_with_host_and_viewer(&reg, &clock).await;

        let payload = ChatPayload {
            sender_id: viewer_id,
            text: "Reply to ghost".to_string(),
            reply_to: Some(uid(99)),
            sent_ms: clock.now_ms(),
        };
        let env = envelope(room_id, viewer_id, payload);

        let err = handle_chat_message(env, &reg, &s, viewer_id, clock.now_ms())
            .await
            .expect_err("should reject reply_to unknown user");
        assert!(matches!(err, RoomError::Internal(ref msg) if msg.contains("unknown participant")));
    }

    #[tokio::test]
    async fn host_and_viewer_both_see_same_chat_history() {
        let (reg, clock) = fresh_registry();
        let s = super::super::store::NoopRoomStore;
        let (room_id, host_id, viewer_id) = setup_room_with_host_and_viewer(&reg, &clock).await;

        let mut all_events: Vec<RoomEvent> = Vec::new();

        for i in 0..10 {
            let sender = if i % 2 == 0 { host_id } else { viewer_id };
            let payload = ChatPayload {
                sender_id: sender,
                text: format!("Message {}", i),
                reply_to: None,
                sent_ms: clock.now_ms() + (i as i64 * 100),
            };
            let env = envelope(room_id, sender, payload);

            let events =
                handle_chat_message(env, &reg, &s, sender, clock.now_ms() + (i as i64 * 100))
                    .await
                    .expect("message should be accepted");
            all_events.extend(events);
        }

        assert_eq!(all_events.len(), 10);
        for (i, event) in all_events.iter().enumerate() {
            match event {
                RoomEvent::ChatMessage(msg) => {
                    assert_eq!(msg.text, format!("Message {}", i));
                }
                _ => panic!("expected ChatMessage event"),
            }
        }
    }

    #[tokio::test]
    async fn chat_cap_can_be_revoked() {
        let (reg, clock) = fresh_registry();
        let s = super::super::store::NoopRoomStore;
        let (room_id, _host_id, viewer_id) = setup_room_with_host_and_viewer(&reg, &clock).await;

        {
            let handle = reg.get_by_id(room_id).await.expect("room");
            let mut state = handle.write().await;
            if let Some(p) = state
                .participants
                .iter_mut()
                .find(|p| p.user_id == viewer_id)
            {
                p.cap_set = 0;
            }
        }

        let payload = ChatPayload {
            sender_id: viewer_id,
            text: "Should fail".to_string(),
            reply_to: None,
            sent_ms: clock.now_ms(),
        };
        let env = envelope(room_id, viewer_id, payload);

        let err = handle_chat_message(env, &reg, &s, viewer_id, clock.now_ms())
            .await
            .expect_err("chat should be rejected after cap revoked");
        assert!(matches!(err, RoomError::Internal(ref msg) if msg.contains("not authorized")));
    }
}
