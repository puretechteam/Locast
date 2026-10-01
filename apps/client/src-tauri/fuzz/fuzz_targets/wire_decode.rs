//! `wire_decode` - the P8-T02 cargo-fuzz target.
//!
//! Feeds arbitrary bytes through the production MessagePack
//! decoder (`locast_client_lib::net::wire::decode_and_validate`)
//! - the same entry point every inbound binary WebSocket frame
//! takes (see `src/net/signaling.rs`). The only property under
//! test is that hostile input never panics, never hangs, and
//! never performs unbounded work; `Err` (`bad_msg` semantics)
//! and `Ok` are both acceptable outcomes. The corpus in
//! `corpus/wire_decode/` is committed and contains the known
//! malformed vectors asserted in `src/net/wire.rs` plus valid
//! envelopes produced by `rmp_serde::to_vec_named` (the real
//! encoder), so mutation starts from the production map layout.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Accepted envelopes must also re-encode without panicking:
    // the outbound path serializes the same type, so a value the
    // decoder can build must be one the encoder can emit. The
    // result is discarded; only a panic counts as a finding.
    if let Ok(env) = locast_client_lib::net::wire::decode_and_validate(data) {
        let _ = rmp_serde::to_vec_named(&env);
    }
});
