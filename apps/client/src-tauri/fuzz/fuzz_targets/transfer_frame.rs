//! `transfer_frame` - a cargo-fuzz target for the DataChannel framing.
//!
//! Feeds arbitrary bytes to the production transfer-wire decoder,
//! `locast_client_lib::transfer::wire::codec::{decode, decode_stream}`:
//! the length-prefixed JSON codec every inbound WebRTC DataChannel
//! message from a remote peer goes through. Hostile input must never
//! panic, hang, or allocate without bound; `Err` and `Ok` are both
//! acceptable outcomes.
//!
//! Two inputs are tried per run:
//! - the raw bytes, so the length-prefix checks (short read, zero,
//!   over `MAX_FRAME_BYTES`) are fuzzed directly;
//! - a re-framed copy: the first four bytes are dropped and the rest
//!   is given a correct big-endian length prefix. Without this a
//!   mutated JSON body would almost always carry a stale prefix and
//!   be rejected before `serde_json` and `validate` are reached.
//!
//! Properties checked on `Ok`:
//! - `decode`: the consumed count is `4 + prefix`, at least 4, and at
//!   most the input length;
//! - `decode_stream`: every byte is consumed (the loop only ends at
//!   the end of the buffer) and the first frame equals what `decode`
//!   returns for the same buffer;
//! - an accepted frame re-encodes with `codec::encode` and decodes
//!   back to an equal frame, consuming exactly the encoded length
//!   (`validate` is `pub(crate)`, so this round trip is how the
//!   target re-checks it).
//!
//! The corpus in `corpus/transfer_frame/` is committed: frames built
//! with the real encoder.
#![no_main]

use libfuzzer_sys::fuzz_target;
use locast_client_lib::transfer::wire::{codec, Frame, MAX_FRAME_BYTES};

fn check_decode(data: &[u8]) {
    if let Ok((frame, used)) = codec::decode(data) {
        assert!(used >= 4, "consumed less than the length prefix");
        assert!(used <= data.len(), "consumed more than the input");
        let prefix = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
        assert_eq!(used, 4 + prefix, "consumed count disagrees with prefix");
        check_roundtrip(&frame);
    }
    if let Ok((frames, used)) = codec::decode_stream(data) {
        assert!(used <= data.len(), "stream consumed more than the input");
        assert_eq!(used, data.len(), "stream returned Ok but left bytes");
        assert_eq!(frames.is_empty(), data.is_empty());
        if let Some(first) = frames.first() {
            let (f, n) = codec::decode(data).expect("stream Ok implies first decode Ok");
            assert_eq!(&f, first, "decode and decode_stream disagree");
            assert!(n <= used);
        }
        for frame in &frames {
            check_roundtrip(frame);
        }
    }
}

fn check_roundtrip(frame: &Frame) {
    let mut buf = Vec::new();
    codec::encode(frame, &mut buf).expect("accepted frame must re-encode");
    let (back, used) = codec::decode(&buf).expect("encoded frame must decode");
    assert_eq!(used, buf.len());
    assert_eq!(&back, frame, "encode/decode round trip changed the frame");
}

fuzz_target!(|data: &[u8]| {
    check_decode(data);

    // Re-framed copy: keep the JSON body, fix the prefix.
    let body = data.get(4..).unwrap_or(&[]);
    if body.len() <= MAX_FRAME_BYTES as usize {
        let mut framed = Vec::with_capacity(4 + body.len());
        framed.extend_from_slice(&(body.len() as u32).to_be_bytes());
        framed.extend_from_slice(body);
        check_decode(&framed);
    }
});
