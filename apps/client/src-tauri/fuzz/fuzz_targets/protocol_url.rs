//! `protocol_url` - a cargo-fuzz target for the `locast://` handler's parsers.
//!
//! Feeds attacker-controlled strings to two production functions in
//! `locast_client_lib::library::protocol`: `LocastUrl::parse`, the URL
//! parser every webview request goes through before the database is
//! touched, and `parse_single_range`, the HTTP `Range` header parser.
//!
//! Input layout: the first 8 bytes are a little-endian u64, right
//! shifted by the low 6 bits of the last of those bytes, giving the
//! file `total_size` (the shift makes 0, 1, small, and near-`u64::MAX`
//! sizes all common). The rest is decoded with
//! `String::from_utf8_lossy`: lossy rather than strict so that a
//! mutation that breaks one byte of UTF-8 still reaches the parsers
//! as a mostly-intact string instead of being thrown away. The same
//! string is used as both the URL and the `Range` header.
//!
//! Properties checked (exactly what the source documents):
//! - nothing panics;
//! - `parse_single_range` `Ok((start, end))` satisfies
//!   `start <= end < total_size`;
//! - `LocastUrl::parse` `Ok` carries only non-empty segments with no
//!   `..`, `/`, `\` or NUL (`decode_segment` contract), and the
//!   segments survive an `encode_segment` / `parse` round trip.
//!
//! The corpus in `corpus/protocol_url/` is committed: a few valid and
//! hostile URL and Range seeds, each with its 8-byte size header.
#![no_main]

use libfuzzer_sys::fuzz_target;
use locast_client_lib::library::protocol::{encode_segment, parse_single_range, LocastUrl};

fn segments(url: &LocastUrl) -> (&'static str, &str, &str) {
    match url {
        LocastUrl::Media {
            sha_prefix,
            filename,
        } => ("media", sha_prefix, filename),
        LocastUrl::Subtitle { sub_id, filename } => ("subtitles", sub_id, filename),
        LocastUrl::Meta { media_id, name } => ("meta", media_id, name),
    }
}

fuzz_target!(|data: &[u8]| {
    if data.len() < 8 {
        return;
    }
    let raw = u64::from_le_bytes([
        data[0], data[1], data[2], data[3], data[4], data[5], data[6], data[7],
    ]);
    let total_size = raw >> (data[7] & 63);
    let text = String::from_utf8_lossy(&data[8..]);

    if let Ok((start, end)) = parse_single_range(&text, total_size) {
        assert!(start <= end, "range start after end: {text:?}");
        assert!(end < total_size, "range end past EOF: {text:?}");
    }

    if let Ok(url) = LocastUrl::parse(&text) {
        let (host, a, b) = segments(&url);
        for seg in [a, b] {
            assert!(!seg.is_empty(), "empty segment accepted: {text:?}");
            assert!(!seg.contains(".."), "traversal accepted: {text:?}");
            assert!(
                !seg.contains(['/', '\\', '\0']),
                "separator or NUL accepted: {text:?}"
            );
        }
        let rebuilt = format!(
            "locast://{host}/{}/{}",
            encode_segment(a),
            encode_segment(b)
        );
        assert_eq!(
            LocastUrl::parse(&rebuilt).ok().as_ref(),
            Some(&url),
            "encode_segment round trip changed the URL: {text:?}"
        );
    }
});
