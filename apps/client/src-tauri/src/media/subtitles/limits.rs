//! `media::subtitles::limits` - hard caps for subtitle parsing
//! (P8-T03, architecture sections 17.3 and 21.8).
//!
//! Every cap REJECTS the whole file when exceeded; nothing is ever
//! silently truncated. The numbers are chosen so that a realistic
//! 10 MB SRT (architecture Risk 8) is admitted while a hostile file
//! cannot force unbounded memory or CPU use.
//!
//! The input-size cap is a recommendation that informs the open
//! size question in architecture appendix A.8 (which asks for a
//! Rust-pre-parse routing threshold); it does not close that
//! question and is not an edit to the architecture document.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

/// Maximum accepted input size in bytes (16 MiB). Checked before
/// any allocation. Also re-checked against the decoded UTF-8 length
/// of UTF-16 input, which can grow up to 1.5x.
pub const MAX_INPUT_BYTES: u64 = 16 * 1024 * 1024;

/// Maximum number of lines in a file (1,000,000).
pub const MAX_LINES: usize = 1_000_000;

/// Maximum length of one line in bytes, excluding the terminator
/// (65,536).
pub const MAX_LINE_BYTES: usize = 65_536;

/// Maximum number of cues a parser may emit (100,000).
pub const MAX_CUES: u32 = 100_000;

/// Maximum size of one cue's text in UTF-8 bytes (8,192).
pub const MAX_CUE_TEXT_BYTES: usize = 8_192;

/// Maximum accepted timestamp in milliseconds (100 hours). Fits in
/// a `u32` with room to spare.
pub const MAX_TIMESTAMP_MS: u32 = 360_000_000;

/// Maximum number of digits accepted in any numeric field of a
/// timestamp. Nine digits always fit in a `u32`, so longer fields
/// are rejected before any integer parse.
pub const MAX_TIMESTAMP_DIGITS: usize = 9;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn limits_have_the_documented_values() {
        assert_eq!(MAX_INPUT_BYTES, 16_777_216);
        assert_eq!(MAX_LINES, 1_000_000);
        assert_eq!(MAX_LINE_BYTES, 65_536);
        assert_eq!(MAX_CUES, 100_000);
        assert_eq!(MAX_CUE_TEXT_BYTES, 8_192);
        assert_eq!(MAX_TIMESTAMP_MS, 100 * 60 * 60 * 1000);
        assert_eq!(MAX_TIMESTAMP_DIGITS, 9);
    }

    #[test]
    fn nine_digit_fields_always_fit_in_u32() {
        assert!(999_999_999_u64 <= u64::from(u32::MAX));
        assert!(
            10_u64.pow(u32::try_from(MAX_TIMESTAMP_DIGITS).unwrap()) - 1 <= u64::from(u32::MAX)
        );
    }
}
