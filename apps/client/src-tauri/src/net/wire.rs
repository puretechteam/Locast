//! `net::wire` - the MessagePack wire decoder for inbound
//! signaling frames (P8-T02).
//!
//! Every binary WebSocket frame carrying a v1 [`Envelope`] is
//! decoded through this module so the decoder under fuzz is the
//! decoder that production uses. There is exactly one decode
//! entry point ([`decode_envelope`]) and exactly one accept /
//! reject gate ([`decode_and_validate`]); the `Err` half of the
//! latter is the client-side counterpart of the server's
//! `ERROR("bad_msg")` close (architecture §21.8, §20.4.1):
//!
//! - [`WireReject::Decode`] - the frame is not a valid
//!   MessagePack encoding of an [`Envelope`].
//! - [`WireReject::Version`] - the frame decodes but carries
//!   `v != 1` (§18.11: receivers reject anything else).
//!
//! Unknown `type` strings intentionally decode (as
//! `MessageKind::Other`, the §18.11 forward-compat rule); the
//! per-type dispatch, not the decoder, rejects them. The fuzz
//! target `wire_decode` (see `apps/client/src-tauri/fuzz/`) feeds
//! arbitrary bytes through [`decode_and_validate`] and only
//! asserts that no input panics; rejected inputs are expected.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

use locast_protocol::envelope::Envelope;

/// The only protocol version this build accepts (§18.11).
pub const PROTOCOL_VERSION: u8 = 1;

/// Why an inbound frame was rejected before dispatch. The
/// `Display` impl names the failure class so logs line up with
/// the server's `bad_msg` close reason.
#[derive(Debug, thiserror::Error)]
pub enum WireReject {
    /// The bytes are not a valid MessagePack [`Envelope`].
    #[error("bad_msg: msgpack decode failed: {0}")]
    Decode(#[from] rmp_serde::decode::Error),
    /// The bytes decode but carry a protocol version other
    /// than [`PROTOCOL_VERSION`].
    #[error("bad_msg: envelope version {0} != {PROTOCOL_VERSION}")]
    Version(u8),
}

/// Decode a binary WebSocket frame into an [`Envelope`].
///
/// This is the raw MessagePack decode; callers that also need
/// the version gate should use [`decode_and_validate`]. The
/// error type is the decoder's own so the signaling connection
/// loop can keep wrapping it in `FrameError::Decode` unchanged.
pub fn decode_envelope(bytes: &[u8]) -> Result<Envelope, rmp_serde::decode::Error> {
    rmp_serde::from_slice(bytes)
}

/// Decode a frame and apply the v1 version gate in one step.
///
/// `Err(_)` from this function is a `bad_msg` in the §20.4.1
/// sense: the server would count it toward the malformed-message
/// window and the client maps it to `DisconnectReason::
/// ProtocolError`. It never panics on hostile input; the fuzz
/// target depends on that property.
pub fn decode_and_validate(bytes: &[u8]) -> Result<Envelope, WireReject> {
    let env = decode_envelope(bytes)?;
    if env.v != PROTOCOL_VERSION {
        return Err(WireReject::Version(env.v));
    }
    Ok(env)
}

#[cfg(test)]
mod tests {
    use super::*;
    use locast_protocol::envelope::MessageKind;
    use uuid::Uuid;

    /// A structurally valid MessagePack envelope with `v = 99`,
    /// in the exact encoding production uses
    /// (`rmp_serde::to_vec_named`: a fixmap(8) with named keys,
    /// `Uuid` as bin8(16)). It is [`v99_envelope`] encoded; the
    /// test `v99_seed_is_the_real_encoding` pins that equality so
    /// the constant cannot drift from the real encoder. This exact
    /// byte vector is also committed as the fuzz corpus seed
    /// `fuzz/corpus/wire_decode/bad_msg_v99.msgpack`;
    /// `committed_corpus_matches_the_test_vectors` keeps the
    /// committed bytes honest.
    const BAD_MSG_V99_SEED: [u8; 74] = [
        0x88, // fixmap(8): the eight envelope fields
        0xa1, 0x76, // "v"
        0x63, // 99
        0xa4, 0x74, 0x79, 0x70, 0x65, // "type"
        0xa5, 0x48, 0x45, 0x4c, 0x4c, 0x4f, // "HELLO"
        0xa2, 0x69, 0x64, // "id"
        0xc4, 0x10, // bin8(16)
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, // Uuid::nil()
        0xa7, 0x72, 0x6f, 0x6f, 0x6d, 0x5f, 0x69, 0x64, // "room_id"
        0xc0, // nil
        0xa6, 0x73, 0x65, 0x6e, 0x64, 0x65, 0x72, // "sender"
        0xc0, // nil
        0xa5, 0x74, 0x73, 0x5f, 0x6d, 0x73, // "ts_ms"
        0x01, // 1
        0xa3, 0x73, 0x65, 0x71, // "seq"
        0x01, // 1
        0xa7, 0x70, 0x61, 0x79, 0x6c, 0x6f, 0x61, 0x64, // "payload"
        0x80, // fixmap(0)
    ];

    /// Truncated frame: a fixmap(1) whose key is present but
    /// whose value (and everything after) is missing. Also
    /// committed as `bad_msg_truncated.msgpack`.
    const BAD_MSG_TRUNCATED_SEED: [u8; 3] = [0x81, 0xa1, 0x76];

    /// The pre-fix v99 seed: `id` hand-encoded as a str8(36)
    /// UUID string instead of the bin8(16) the real encoder
    /// emits. Structurally wrong for this decoder, so it must be
    /// a decode failure, never a version failure.
    const BAD_MSG_STR_UUID: [u8; 94] = [
        0x88, // fixmap(8)
        0xa1, 0x76, // "v"
        0x63, // 99
        0xa4, 0x74, 0x79, 0x70, 0x65, // "type"
        0xa5, 0x48, 0x45, 0x4c, 0x4c, 0x4f, // "HELLO"
        0xa2, 0x69, 0x64, // "id"
        0xd9, 0x24, // str8(36)
        0x30, 0x30, 0x30, 0x30, 0x30, 0x30, 0x30, 0x30, 0x2d, 0x30, 0x30, 0x30, 0x30, 0x2d, 0x30,
        0x30, 0x30, 0x30, 0x2d, 0x30, 0x30, 0x30, 0x30, 0x2d, 0x30, 0x30, 0x30, 0x30, 0x30, 0x30,
        0x30, 0x30, 0x30, 0x30, 0x30, 0x30, // "00000000-0000-0000-0000-000000000000"
        0xa7, 0x72, 0x6f, 0x6f, 0x6d, 0x5f, 0x69, 0x64, // "room_id"
        0xc0, // nil
        0xa6, 0x73, 0x65, 0x6e, 0x64, 0x65, 0x72, // "sender"
        0xc0, // nil
        0xa5, 0x74, 0x73, 0x5f, 0x6d, 0x73, // "ts_ms"
        0x01, // 1
        0xa3, 0x73, 0x65, 0x71, // "seq"
        0x01, // 1
        0xa7, 0x70, 0x61, 0x79, 0x6c, 0x6f, 0x61, 0x64, // "payload"
        0x80, // fixmap(0)
    ];

    fn envelope(v: u8, kind: MessageKind, ts_ms: i64) -> Envelope {
        Envelope {
            v,
            r#type: kind,
            id: Uuid::nil(),
            room_id: None,
            sender: None,
            ts_ms,
            seq: 1,
            payload: serde_json::json!({}),
        }
    }

    /// The envelope [`BAD_MSG_V99_SEED`] encodes: deterministic
    /// fields, valid in every respect except `v = 99`.
    fn v99_envelope() -> Envelope {
        envelope(99, MessageKind::Hello, 1)
    }

    fn valid_hello_bytes() -> Vec<u8> {
        let env = envelope(PROTOCOL_VERSION, MessageKind::Hello, 1_700_000_000_000);
        rmp_serde::to_vec_named(&env).expect("encode hello")
    }

    #[test]
    fn valid_envelope_round_trips() {
        let bytes = valid_hello_bytes();
        let env = decode_and_validate(&bytes).expect("valid envelope must decode");
        assert_eq!(
            env,
            envelope(PROTOCOL_VERSION, MessageKind::Hello, 1_700_000_000_000)
        );
        assert_eq!(
            rmp_serde::to_vec_named(&env).expect("re-encode hello"),
            bytes,
            "decode then encode must reproduce the original bytes"
        );
    }

    #[test]
    fn empty_input_is_rejected_as_decode() {
        let err = decode_and_validate(&[]).unwrap_err();
        assert!(matches!(err, WireReject::Decode(_)), "got {err:?}");
    }

    #[test]
    fn every_proper_prefix_of_a_valid_frame_is_rejected_as_decode() {
        let bytes = valid_hello_bytes();
        for len in 0..bytes.len() {
            let err = decode_and_validate(&bytes[..len]).unwrap_err();
            assert!(
                matches!(err, WireReject::Decode(_)),
                "prefix of length {len} gave {err:?}"
            );
        }
    }

    #[test]
    fn reserved_marker_0xc1_is_rejected_as_decode() {
        // 0xc1 is the one MessagePack marker that is never used.
        let err = decode_and_validate(&[0xc1]).unwrap_err();
        assert!(matches!(err, WireReject::Decode(_)), "got {err:?}");
    }

    #[test]
    fn versions_zero_and_two_are_rejected_as_version() {
        for v in [0u8, 2] {
            let bytes = rmp_serde::to_vec_named(&envelope(v, MessageKind::Hello, 1))
                .expect("encode envelope");
            let err = decode_and_validate(&bytes).unwrap_err();
            assert!(
                matches!(err, WireReject::Version(got) if got == v),
                "v={v} gave {err:?}"
            );
        }
    }

    #[test]
    fn version_256_is_rejected_as_decode() {
        // `v` is a u8, so 256 cannot come from the encoder. Encode
        // a v1 envelope, then swap the positive fixint value of
        // the leading `"v"` key (fixmap(8), fixstr "v", 0x01) for
        // a uint16 256 (cd 01 00). The map stays well formed; only
        // the integer range is wrong for the field type.
        let bytes = valid_hello_bytes();
        assert_eq!(&bytes[..4], &[0x88, 0xa1, 0x76, 0x01], "unexpected layout");
        let mut patched = bytes[..3].to_vec();
        patched.extend_from_slice(&[0xcd, 0x01, 0x00]);
        patched.extend_from_slice(&bytes[4..]);
        let err = decode_and_validate(&patched).unwrap_err();
        assert!(matches!(err, WireReject::Decode(_)), "got {err:?}");
    }

    #[test]
    fn malformed_bytes_are_rejected_as_decode() {
        let err = decode_and_validate(&BAD_MSG_TRUNCATED_SEED).unwrap_err();
        assert!(matches!(err, WireReject::Decode(_)), "got {err:?}");
        assert!(err.to_string().starts_with("bad_msg: "), "got {err}");
    }

    #[test]
    fn string_encoded_uuid_is_rejected_as_decode_not_version() {
        // Structurally malformed for this decoder (id is a str,
        // not bin8(16)): the decode error fires before the
        // version gate is ever reached.
        let err = decode_and_validate(&BAD_MSG_STR_UUID).unwrap_err();
        assert!(matches!(err, WireReject::Decode(_)), "got {err:?}");
        assert!(err.to_string().starts_with("bad_msg: "), "got {err}");
    }

    #[test]
    fn v99_seed_is_the_real_encoding() {
        let encoded = rmp_serde::to_vec_named(&v99_envelope()).expect("encode v99");
        assert_eq!(encoded, BAD_MSG_V99_SEED, "BAD_MSG_V99_SEED drifted");
        // The seed is structurally valid: the raw decoder accepts
        // it, and only the version gate turns it away.
        let env = decode_envelope(&BAD_MSG_V99_SEED).expect("v99 seed must decode");
        assert_eq!(env, v99_envelope());
    }

    #[test]
    fn wrong_version_is_rejected_as_bad_msg() {
        let err = decode_and_validate(&BAD_MSG_V99_SEED).unwrap_err();
        assert!(matches!(err, WireReject::Version(99)), "got {err:?}");
        assert!(err.to_string().starts_with("bad_msg: "), "got {err}");
    }

    #[test]
    fn unknown_type_decodes_permissively() {
        // 18.11: the decoder is permissive about unknown type
        // strings; dispatch rejects them, not the decoder.
        let env = envelope(
            PROTOCOL_VERSION,
            MessageKind::Other("FOOBAR".into()),
            1_700_000_000_000,
        );
        let bytes = rmp_serde::to_vec_named(&env).expect("encode unknown type");
        assert!(
            bytes.windows(b"FOOBAR".len()).any(|w| w == b"FOOBAR"),
            "unknown tag must be on the wire as a plain string"
        );
        let decoded = decode_and_validate(&bytes).expect("unknown type still decodes");
        assert_eq!(decoded.v, PROTOCOL_VERSION);
        assert!(matches!(decoded.r#type, MessageKind::Other(ref t) if t == "FOOBAR"));
    }

    #[test]
    fn committed_corpus_matches_the_test_vectors() {
        // The fuzz corpus seeds live at
        // apps/client/src-tauri/fuzz/corpus/wire_decode/. This
        // test fails if the committed files drift from the byte
        // vectors asserted above, so the corpus stays in sync
        // with what the decoder is proven to reject.
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fuzz/corpus/wire_decode");
        let v99 = std::fs::read(dir.join("bad_msg_v99.msgpack")).expect("corpus seed exists");
        assert_eq!(v99, BAD_MSG_V99_SEED, "bad_msg_v99.msgpack drifted");
        let trunc =
            std::fs::read(dir.join("bad_msg_truncated.msgpack")).expect("corpus seed exists");
        assert_eq!(
            trunc, BAD_MSG_TRUNCATED_SEED,
            "bad_msg_truncated.msgpack drifted"
        );
    }

    #[test]
    fn committed_valid_seeds_decode_and_re_encode_identically() {
        // The valid seeds must stay accepted by the production
        // decoder and must be exactly what the real encoder emits
        // for the decoded value, so mutation starts from the true
        // production map layout.
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fuzz/corpus/wire_decode");
        for name in ["valid_hello.msgpack", "valid_chat_message.msgpack"] {
            let seed = std::fs::read(dir.join(name)).expect("corpus seed exists");
            let env = decode_and_validate(&seed)
                .unwrap_or_else(|e| panic!("{name} must decode and validate: {e:?}"));
            let re_encoded = rmp_serde::to_vec_named(&env).expect("re-encode seed");
            assert_eq!(re_encoded, seed, "{name} is not the real encoding");
        }
    }
}
