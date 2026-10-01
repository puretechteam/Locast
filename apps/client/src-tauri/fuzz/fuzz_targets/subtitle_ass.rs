//! `subtitle_ass` - a P8-T03 cargo-fuzz target.
//!
//! Feeds arbitrary raw bytes (no lossy conversion, so BOMs,
//! UTF-16 and invalid UTF-8 reach the decoder) to
//! `locast_client_lib::media::subtitles::parse_ass`, the production ASS/SSA
//! parser for untrusted subtitle files. Any input may be rejected;
//! hostile input must never panic, hang, or allocate without
//! bound. On `Err` the fixed `Display` text and the stable
//! `code()` string must be producible. On `Ok` the documented
//! cue invariants must hold: at most `MAX_CUES` cues, sorted by
//! start time, `start_ms <= end_ms <= MAX_TIMESTAMP_MS`, and cue
//! text that is non-empty and at most `MAX_CUE_TEXT_BYTES` long.
//! The corpus in `corpus/subtitle_ass/` is committed: one valid
//! file plus malicious vectors (BOM with a negative timestamp, a
//! 4294967296 / 4GB size string, format-specific edge cases).
#![no_main]

use libfuzzer_sys::fuzz_target;
use locast_client_lib::media::subtitles::{
    parse_ass, MAX_CUES, MAX_CUE_TEXT_BYTES, MAX_TIMESTAMP_MS,
};

fuzz_target!(|data: &[u8]| {
    match parse_ass(data) {
        Err(e) => {
            // Error rendering must not panic and must stay stable.
            let _ = format!("{e}");
            let _ = e.code();
        }
        Ok(cues) => {
            assert!(u32::try_from(cues.len()).is_ok_and(|n| n <= MAX_CUES));
            let mut prev_start = 0u32;
            for cue in &cues {
                assert!(cue.start_ms <= cue.end_ms);
                assert!(cue.end_ms <= MAX_TIMESTAMP_MS);
                assert!(!cue.text.is_empty());
                assert!(cue.text.len() <= MAX_CUE_TEXT_BYTES);
                assert!(cue.start_ms >= prev_start);
                prev_start = cue.start_ms;
            }
        }
    }
});
