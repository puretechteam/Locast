//! Handshake payload structs (HELLO, WELCOME, CHALLENGE, AUTH,
//! AUTH_OK, AUTH_FAIL) per `docs/ARCHITECTURE.md` section 18.4.1.
//!
//! These are the per-type payloads. They are not used directly by
//! the [`crate::envelope::Envelope`] type (which holds a
//! `serde_json::Value` payload for schema-agnostic transport);
//! they are deserialized out of the envelope's `payload` field
//! when the recipient knows the expected type.

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};
use ts_rs::TS;
use uuid::Uuid;

/// HELLO (C -> S).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "ts/index.ts")]
pub struct HelloPayload {
    pub client_version: String,
    #[ts(inline)]
    pub platform: Platform,
    pub device_id: String,
}

/// The client OS. v1 supports win, mac, linux.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "ts/index.ts")]
pub enum Platform {
    #[serde(rename = "win")]
    Win,
    #[serde(rename = "mac")]
    Mac,
    #[serde(rename = "linux")]
    Linux,
}

/// WELCOME (S -> C).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "ts/index.ts")]
pub struct WelcomePayload {
    pub session_id: Uuid,
    pub server_ts_ms: i64,
    pub config: WelcomeConfig,
}

/// Server-published session configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "ts/index.ts")]
pub struct WelcomeConfig {
    pub max_room_size: u8,
    pub rate: WelcomeRate,
}

/// Per-connection rate limits announced at WELCOME.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "ts/index.ts")]
pub struct WelcomeRate {
    pub msgs_per_sec: u16,
    pub bytes_per_sec: u32,
}

/// CHALLENGE (S -> C).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "ts/index.ts")]
pub struct ChallengePayload {
    /// 32-byte random nonce. The client signs this raw (no
    /// domain tag, §20.4.4) and returns the signature in
    /// [`AuthPayload::sig`].
    pub nonce: Vec<u8>,
    /// Server-side absolute expiry (unix ms).
    pub expires_ms: i64,
}

/// AUTH (C -> S).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "ts/index.ts")]
pub struct AuthPayload {
    /// 32-byte Ed25519 public key. The server uses this as the
    /// canonical identifier for the user (§21.3).
    pub pubkey: Vec<u8>,
    /// 64-byte Ed25519 signature over the raw 32-byte nonce
    /// from the preceding CHALLENGE.
    pub sig: Vec<u8>,
}

/// AUTH_OK (S -> C).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "ts/index.ts")]
pub struct AuthOkPayload {
    /// Server-assigned UUID v7 (§20.4.4). NOT the client's
    /// sha256-hex `user_id`.
    pub user_id: Uuid,
    pub bearer: AuthBearer,
    /// The 32-byte public key the server associated with the
    /// user. Echoes the value the client sent in AUTH so the
    /// client can confirm the binding.
    pub pubkey: Vec<u8>,
}

/// A bearer token issued by the server on successful AUTH.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "ts/index.ts")]
pub struct AuthBearer {
    /// 32-byte bearer token. The client holds the plaintext
    /// only in memory; the server stores sha256(token) in
    /// SQLite (§21.3).
    pub token: Vec<u8>,
    /// Absolute expiry (unix ms). 15 minutes by default (§20.4.4).
    pub expires_ms: i64,
}

/// AUTH_FAIL (S -> C).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "ts/index.ts")]
pub struct AuthFailPayload {
    #[ts(inline)]
    pub reason: AuthFailReason,
}

/// Failure reason for an AUTH_FAIL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "ts/index.ts")]
pub enum AuthFailReason {
    #[serde(rename = "bad_sig")]
    BadSig,
    #[serde(rename = "expired")]
    Expired,
    #[serde(rename = "banned")]
    Banned,
    #[serde(rename = "rate")]
    Rate,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn hello_json_roundtrip() {
        let h = HelloPayload {
            client_version: "0.0.0".to_string(),
            platform: Platform::Win,
            device_id: "abc-123".to_string(),
        };
        let s = serde_json::to_string(&h).expect("serialize");
        assert!(s.contains("\"platform\":\"win\""));
        let back: HelloPayload = serde_json::from_str(&s).expect("deserialize");
        assert_eq!(back, h);
    }

    #[test]
    fn auth_fail_reason_serde_strings() {
        for (reason, wire) in [
            (AuthFailReason::BadSig, "\"bad_sig\""),
            (AuthFailReason::Expired, "\"expired\""),
            (AuthFailReason::Banned, "\"banned\""),
            (AuthFailReason::Rate, "\"rate\""),
        ] {
            let s = serde_json::to_string(&reason).expect("serialize");
            assert_eq!(s, wire);
            let back: AuthFailReason = serde_json::from_str(&s).expect("deserialize");
            assert_eq!(back, reason);
        }
    }

    #[test]
    fn auth_payload_carries_32_64_byte_fields() {
        let p = AuthPayload {
            pubkey: vec![1u8; 32],
            sig: vec![2u8; 64],
        };
        let s = serde_json::to_string(&p).expect("serialize");
        let back: AuthPayload = serde_json::from_str(&s).expect("deserialize");
        assert_eq!(back, p);
    }

    #[test]
    fn welcome_payload_has_nested_config() {
        let w = WelcomePayload {
            session_id: Uuid::nil(),
            server_ts_ms: 1_700_000_000_000,
            config: WelcomeConfig {
                max_room_size: 8,
                rate: WelcomeRate {
                    msgs_per_sec: 100,
                    bytes_per_sec: 1_000_000,
                },
            },
        };
        let v: serde_json::Value = serde_json::to_value(&w).expect("to_value");
        assert_eq!(v["config"]["max_room_size"], json!(8));
        let back: WelcomePayload = serde_json::from_value(v).expect("from_value");
        assert_eq!(back, w);
    }
}
