//! Integration tests for the SIGNAL envelope and SignalPayload types
//! added in P3-T05.

use locast_protocol::envelope::MessageKind;
use locast_protocol::room::{SignalCandidate, SignalKind, SignalPayload};
use serde_json::{json, Value};
use uuid::Uuid;

fn target_id() -> Uuid {
    Uuid::from_u128(0xdead_beef_dead_beef_dead_beef_dead_beef_u128)
}

#[test]
fn signal_payload_roundtrip() {
    let target = target_id();

    let offer = SignalPayload {
        to_user_id: target,
        kind: SignalKind::Offer,
        sdp: Some("v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\n".to_string()),
        candidates: None,
    };
    let offer_v = serde_json::to_value(&offer).expect("serialize offer");
    let offer_back: SignalPayload =
        serde_json::from_value(offer_v.clone()).expect("deserialize offer");
    assert_eq!(offer_back, offer);

    let answer = SignalPayload {
        to_user_id: target,
        kind: SignalKind::Answer,
        sdp: Some("v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\n".to_string()),
        candidates: None,
    };
    let answer_v = serde_json::to_value(&answer).expect("serialize answer");
    let answer_back: SignalPayload =
        serde_json::from_value(answer_v.clone()).expect("deserialize answer");
    assert_eq!(answer_back, answer);

    let ice = SignalPayload {
        to_user_id: target,
        kind: SignalKind::Ice,
        sdp: None,
        candidates: Some(vec![SignalCandidate {
            candidate: "candidate:1 1 udp 2122252543 192.0.2.1 12345 typ host".to_string(),
            sdp_mid: None,
            sdp_m_line_index: None,
        }]),
    };
    let ice_v = serde_json::to_value(&ice).expect("serialize ice");
    let ice_back: SignalPayload = serde_json::from_value(ice_v.clone()).expect("deserialize ice");
    assert_eq!(ice_back, ice);

    assert_eq!(offer_v["kind"], json!("offer"));
    assert_eq!(answer_v["kind"], json!("answer"));
    assert_eq!(ice_v["kind"], json!("ice"));

    assert!(offer_v.get("candidates").is_none());
    assert!(offer_v.get("sdp_mid").is_none());
    assert!(offer_v.get("sdp_m_line_index").is_none());
    assert!(ice_v.get("sdp").is_none());
}

#[test]
fn signal_message_kind_is_signal_lifecycle() {
    assert!(MessageKind::Signal.is_signal_lifecycle());
    assert_eq!(MessageKind::Signal.as_str(), "SIGNAL");

    for other in [
        MessageKind::Hello,
        MessageKind::Auth,
        MessageKind::RoomCreate,
        MessageKind::RoomJoinRequest,
        MessageKind::RoomLeave,
        MessageKind::Presence,
        MessageKind::ManifestPublish,
        MessageKind::ManifestRequest,
        MessageKind::RateLimit,
        MessageKind::Other("WHATEVER".to_string()),
    ] {
        assert!(
            !other.is_signal_lifecycle(),
            "{:?} should not be a signal-lifecycle message",
            other
        );
    }
}

#[test]
fn signal_payload_omits_unset_fields() {
    let payload = SignalPayload {
        to_user_id: target_id(),
        kind: SignalKind::Ice,
        sdp: None,
        candidates: Some(vec![SignalCandidate {
            candidate: "candidate:1 1 udp 2122252543 192.0.2.1 12345 typ host".to_string(),
            sdp_mid: None,
            sdp_m_line_index: None,
        }]),
    };

    let v: Value = serde_json::to_value(&payload).expect("serialize");
    assert!(v.get("sdp").is_none(), "sdp should be omitted, got {}", v);
    let cand = &v["candidates"][0];
    assert!(
        cand.get("sdp_mid").is_none(),
        "sdp_mid should be omitted, got {}",
        cand
    );
    assert!(
        cand.get("sdp_m_line_index").is_none(),
        "sdp_m_line_index should be omitted, got {}",
        cand
    );
}
