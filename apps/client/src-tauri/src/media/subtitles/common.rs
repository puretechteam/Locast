//! `media::subtitles::common` - helpers shared by the SRT, VTT and
//! ASS parsers (P8-T03, architecture sections 17.3 and 21.8).
//!
//! Three groups of helpers live here:
//!
//! - Decoding: [`decode_text`] turns untrusted bytes into text
//!   (size cap first, BOM handling, strict UTF-8 / UTF-16, NUL
//!   rejection, empty-input rejection) and [`split_lines`] splits it
//!   into lines with the line-count and line-length caps enforced.
//! - Numbers: [`parse_digits`], [`frac_to_ms`], [`timestamp_to_ms`]
//!   and [`check_order`] parse timestamp fields with checked
//!   arithmetic. No integer is ever parsed from a field of more than
//!   [`MAX_TIMESTAMP_DIGITS`] digits, and nothing is ever allocated
//!   with a size taken from file content.
//! - Text: [`is_dropped_char`], [`sanitize_text`], [`push_cue_text`]
//!   and [`push_cue_char`] build cue text with control and bidi
//!   characters removed and the per-cue size cap enforced as the text
//!   grows.
//!
//! The functions are `pub` only so the whole subtitle module tree
//! (and its tests) can reach them; they are internal helpers, not a
//! stable API.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

use std::borrow::Cow;

use super::error::SubtitleError;
use super::limits::{
    MAX_CUES, MAX_CUE_TEXT_BYTES, MAX_INPUT_BYTES, MAX_LINES, MAX_LINE_BYTES, MAX_TIMESTAMP_DIGITS,
    MAX_TIMESTAMP_MS,
};

/// Milliseconds per hour / minute / second.
const MS_PER_HOUR: u64 = 3_600_000;
const MS_PER_MINUTE: u64 = 60_000;
const MS_PER_SECOND: u64 = 1_000;

/// True when `len` is above the input cap. Never panics, never
/// truncates.
fn exceeds_input_cap(len: usize) -> bool {
    match u64::try_from(len) {
        Ok(n) => n > MAX_INPUT_BYTES,
        Err(_) => true,
    }
}

/// Decode untrusted bytes into text.
///
/// Rules, in order:
///
/// 1. More than [`MAX_INPUT_BYTES`] bytes -> `TooLarge`, before any
///    allocation.
/// 2. `EF BB BF` -> strip it, then strict UTF-8.
/// 3. `FF FE` / `FE FF` -> strip it, then strict UTF-16 LE / BE. An
///    odd byte count or an unpaired surrogate -> `InvalidEncoding`;
///    the decoded UTF-8 size is re-checked against the cap.
/// 4. Otherwise strict UTF-8 (never lossy). Non-BOM, non-UTF-8
///    legacy encodings such as cp1252 are rejected.
/// 5. Any NUL character -> `InvalidEncoding`.
/// 6. Empty or whitespace-only text (including BOM-only input) ->
///    `EmptyInput`.
///
/// A U+FEFF anywhere except the very start is ordinary text.
pub fn decode_text(bytes: &[u8]) -> Result<Cow<'_, str>, SubtitleError> {
    if exceeds_input_cap(bytes.len()) {
        return Err(SubtitleError::TooLarge {
            limit: MAX_INPUT_BYTES,
        });
    }
    let text: Cow<'_, str> = match bytes {
        [0xFF, 0xFE, rest @ ..] => Cow::Owned(decode_utf16(rest, u16::from_le_bytes)?),
        [0xFE, 0xFF, rest @ ..] => Cow::Owned(decode_utf16(rest, u16::from_be_bytes)?),
        [0xEF, 0xBB, 0xBF, rest @ ..] => Cow::Borrowed(decode_utf8(rest)?),
        _ => Cow::Borrowed(decode_utf8(bytes)?),
    };
    if text.contains('\0') {
        return Err(SubtitleError::InvalidEncoding);
    }
    if text.trim().is_empty() {
        return Err(SubtitleError::EmptyInput);
    }
    Ok(text)
}

/// Strict UTF-8 decode; any invalid sequence is `InvalidEncoding`.
fn decode_utf8(bytes: &[u8]) -> Result<&str, SubtitleError> {
    std::str::from_utf8(bytes).map_err(|_| SubtitleError::InvalidEncoding)
}

/// Strict UTF-16 decode of BOM-less `bytes` using `to_unit` to turn a
/// byte pair into a code unit. Not `String::from_utf16le` (MSRV 1.75).
fn decode_utf16(bytes: &[u8], to_unit: fn([u8; 2]) -> u16) -> Result<String, SubtitleError> {
    let chunks = bytes.chunks_exact(2);
    if !chunks.remainder().is_empty() {
        return Err(SubtitleError::InvalidEncoding);
    }
    // Grown, never pre-sized: the length is bounded by the input
    // cap, but the rule is that no allocation is sized up front.
    let mut units: Vec<u16> = Vec::new();
    for chunk in chunks {
        let pair = <[u8; 2]>::try_from(chunk).map_err(|_| SubtitleError::InvalidEncoding)?;
        units.push(to_unit(pair));
    }
    let decoded = String::from_utf16(&units).map_err(|_| SubtitleError::InvalidEncoding)?;
    if exceeds_input_cap(decoded.len()) {
        return Err(SubtitleError::TooLarge {
            limit: MAX_INPUT_BYTES,
        });
    }
    Ok(decoded)
}

/// 1-based line number for the 0-based line index `index`,
/// saturating at `u32::MAX`.
pub fn line_number(index: usize) -> u32 {
    u32::try_from(index).unwrap_or(u32::MAX).saturating_add(1)
}

/// Split `text` into lines on `\r\n`, `\r` or `\n` with one byte
/// scan (no intermediate copy).
///
/// A final terminator does not create an extra empty line, so
/// `"a\n"` is `["a"]` and `"a\n\n"` is `["a", ""]`. More than
/// [`MAX_LINES`] lines -> `TooManyLines`; a line longer than
/// [`MAX_LINE_BYTES`] bytes (terminator excluded) -> `LineTooLong`
/// with its 1-based line number.
pub fn split_lines(text: &str) -> Result<Vec<&str>, SubtitleError> {
    let bytes = text.as_bytes();
    let mut lines: Vec<&str> = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while let Some(&b) = bytes.get(i) {
        i += 1;
        if b == b'\n' || b == b'\r' {
            push_line(text, &mut lines, start, i - 1)?;
            if b == b'\r' && bytes.get(i) == Some(&b'\n') {
                i += 1;
            }
            start = i;
        }
    }
    if start < bytes.len() {
        push_line(text, &mut lines, start, bytes.len())?;
    }
    Ok(lines)
}

/// Append `text[start..end]` to `lines`, enforcing the line caps.
fn push_line<'a>(
    text: &'a str,
    lines: &mut Vec<&'a str>,
    start: usize,
    end: usize,
) -> Result<(), SubtitleError> {
    if lines.len() >= MAX_LINES {
        return Err(SubtitleError::TooManyLines);
    }
    if end.saturating_sub(start) > MAX_LINE_BYTES {
        return Err(SubtitleError::LineTooLong {
            line: line_number(lines.len()),
        });
    }
    // The cut points sit next to ASCII line terminators, so they are
    // always char boundaries; `get` keeps this panic-free regardless.
    let line = text.get(start..end).ok_or(SubtitleError::InvalidEncoding)?;
    lines.push(line);
    Ok(())
}

/// Parse a purely numeric field.
///
/// Rejects the empty string, any byte that is not an ASCII digit
/// (so `-`, `+`, spaces and non-ASCII digits all fail), and any field
/// longer than [`MAX_TIMESTAMP_DIGITS`] digits, all BEFORE the
/// integer parse. `"4294967296"` is therefore rejected on length,
/// not by overflow.
pub fn parse_digits(s: &str) -> Option<u32> {
    if s.is_empty() || s.len() > MAX_TIMESTAMP_DIGITS {
        return None;
    }
    if !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse::<u32>().ok()
}

/// Convert a fractional-second digit string to milliseconds by digit
/// count: 1 digit is tenths (x100), 2 digits is hundredths (x10,
/// also the ASS centisecond field), 3 digits is milliseconds (x1).
/// Any other length, or a non-digit, is `None`.
pub fn frac_to_ms(s: &str) -> Option<u32> {
    let value = parse_digits(s)?;
    let scale: u32 = match s.len() {
        1 => 100,
        2 => 10,
        3 => 1,
        _ => return None,
    };
    value.checked_mul(scale)
}

/// Combine clock fields into milliseconds with checked arithmetic.
///
/// `ms` must already be below 1000 (use [`frac_to_ms`]). This does
/// not range-check `minutes` / `seconds` (callers such as VTT enforce
/// `< 60` themselves). Overflow or a total above
/// [`MAX_TIMESTAMP_MS`] (100 hours exactly is allowed) ->
/// `TimestampTooLarge { line }`.
pub fn timestamp_to_ms(
    line: u32,
    hours: u32,
    minutes: u32,
    seconds: u32,
    ms: u32,
) -> Result<u32, SubtitleError> {
    let too_large = SubtitleError::TimestampTooLarge { line };
    let total = u64::from(hours)
        .checked_mul(MS_PER_HOUR)
        .and_then(|h| {
            u64::from(minutes)
                .checked_mul(MS_PER_MINUTE)
                .and_then(|m| h.checked_add(m))
        })
        .and_then(|hm| {
            u64::from(seconds)
                .checked_mul(MS_PER_SECOND)
                .and_then(|s| hm.checked_add(s))
        })
        .and_then(|hms| hms.checked_add(u64::from(ms)));
    match total.and_then(|t| u32::try_from(t).ok()) {
        Some(t) if t <= MAX_TIMESTAMP_MS => Ok(t),
        _ => Err(too_large),
    }
}

/// `TimestampOrder { line }` when `end < start`; equal is allowed.
pub fn check_order(line: u32, start_ms: u32, end_ms: u32) -> Result<(), SubtitleError> {
    if end_ms < start_ms {
        Err(SubtitleError::TimestampOrder { line })
    } else {
        Ok(())
    }
}

/// `TooManyCues` when `count` (the number of cues INCLUDING the one
/// about to be pushed) is above [`MAX_CUES`]. Parsers call this as
/// `check_cue_count(cues.len().saturating_add(1))` before pushing.
pub fn check_cue_count(count: usize) -> Result<(), SubtitleError> {
    let over = match u32::try_from(count) {
        Ok(n) => n > MAX_CUES,
        Err(_) => true,
    };
    if over {
        Err(SubtitleError::TooManyCues { limit: MAX_CUES })
    } else {
        Ok(())
    }
}

/// True for characters removed from cue text: C0 control characters
/// (U+0000..U+001F) other than `\n`, and the bidi control characters
/// U+202A..U+202E and U+2066..U+2069. DEL (U+007F) is kept.
pub fn is_dropped_char(c: char) -> bool {
    (c < '\u{20}' && c != '\n') || matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

/// `s` with every [`is_dropped_char`] character removed. No size
/// cap; use [`push_cue_text`] when building a cue.
pub fn sanitize_text(s: &str) -> String {
    s.chars().filter(|c| !is_dropped_char(*c)).collect()
}

/// Append one character to cue text `buf`, skipping dropped
/// characters and enforcing [`MAX_CUE_TEXT_BYTES`] on the running
/// size. Overflow -> `CueTooLong { line }` (the cue is rejected, not
/// truncated).
pub fn push_cue_char(buf: &mut String, c: char, line: u32) -> Result<(), SubtitleError> {
    if is_dropped_char(c) {
        return Ok(());
    }
    match buf.len().checked_add(c.len_utf8()) {
        Some(n) if n <= MAX_CUE_TEXT_BYTES => {
            buf.push(c);
            Ok(())
        }
        _ => Err(SubtitleError::CueTooLong { line }),
    }
}

/// Append `piece` to cue text `buf` character by character via
/// [`push_cue_char`], so the size cap is checked as the text grows
/// and the buffer never exceeds the cap.
pub fn push_cue_text(buf: &mut String, piece: &str, line: u32) -> Result<(), SubtitleError> {
    for c in piece.chars() {
        push_cue_char(buf, c, line)?;
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn cap() -> usize {
        usize::try_from(MAX_INPUT_BYTES).unwrap()
    }

    fn utf16le(s: &str) -> Vec<u8> {
        let mut out = vec![0xFF, 0xFE];
        for u in s.encode_utf16() {
            out.extend_from_slice(&u.to_le_bytes());
        }
        out
    }

    fn utf16be(s: &str) -> Vec<u8> {
        let mut out = vec![0xFE, 0xFF];
        for u in s.encode_utf16() {
            out.extend_from_slice(&u.to_be_bytes());
        }
        out
    }

    // ----- decode_text -----

    #[test]
    fn decode_plain_utf8_borrows() {
        let text = decode_text(b"hello").unwrap();
        assert_eq!(text, "hello");
        assert!(matches!(text, Cow::Borrowed(_)));
    }

    #[test]
    fn decode_strips_a_utf8_bom() {
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice("caf\u{e9}".as_bytes());
        assert_eq!(decode_text(&bytes).unwrap(), "caf\u{e9}");
    }

    #[test]
    fn decode_strips_only_one_utf8_bom() {
        let bytes = [0xEF, 0xBB, 0xBF, 0xEF, 0xBB, 0xBF, b'a'];
        assert_eq!(decode_text(&bytes).unwrap(), "\u{feff}a");
    }

    #[test]
    fn decode_keeps_a_mid_file_feff_as_text() {
        let bytes = "a\u{feff}b".as_bytes();
        assert_eq!(decode_text(bytes).unwrap(), "a\u{feff}b");
    }

    #[test]
    fn decode_utf16le_with_bom() {
        let bytes = utf16le("a\u{e9}\u{65e5}\u{1F600}");
        assert_eq!(decode_text(&bytes).unwrap(), "a\u{e9}\u{65e5}\u{1F600}");
    }

    #[test]
    fn decode_utf16be_with_bom() {
        let bytes = utf16be("a\u{e9}\u{65e5}\u{1F600}");
        assert_eq!(decode_text(&bytes).unwrap(), "a\u{e9}\u{65e5}\u{1F600}");
    }

    #[test]
    fn decode_odd_length_utf16_is_invalid_encoding() {
        let mut le = utf16le("ab");
        le.push(0x41);
        assert_eq!(decode_text(&le), Err(SubtitleError::InvalidEncoding));
        let mut be = utf16be("ab");
        be.push(0x41);
        assert_eq!(decode_text(&be), Err(SubtitleError::InvalidEncoding));
    }

    #[test]
    fn decode_lone_surrogate_is_invalid_encoding() {
        // 'A' then a lone high surrogate U+D800 (LE).
        let le = [0xFF, 0xFE, 0x41, 0x00, 0x00, 0xD8];
        assert_eq!(decode_text(&le), Err(SubtitleError::InvalidEncoding));
        // A lone low surrogate U+DC00 (BE) followed by 'A'.
        let be = [0xFE, 0xFF, 0xDC, 0x00, 0x00, 0x41];
        assert_eq!(decode_text(&be), Err(SubtitleError::InvalidEncoding));
        // A high surrogate followed by a non-low-surrogate.
        let bad_pair = [0xFF, 0xFE, 0x00, 0xD8, 0x41, 0x00];
        assert_eq!(decode_text(&bad_pair), Err(SubtitleError::InvalidEncoding));
    }

    #[test]
    fn decode_utf16_expansion_past_the_cap_is_too_large() {
        // 8_388_607 units of U+0800 (3 UTF-8 bytes each) from exactly
        // MAX_INPUT_BYTES input bytes decode to about 24 MiB.
        let mut bytes = vec![0xFF, 0xFE];
        let units = (cap() - 2) / 2;
        for _ in 0..units {
            bytes.extend_from_slice(&[0x00, 0x08]);
        }
        assert_eq!(bytes.len(), cap());
        assert_eq!(
            decode_text(&bytes),
            Err(SubtitleError::TooLarge {
                limit: MAX_INPUT_BYTES
            })
        );
    }

    #[test]
    fn decode_invalid_utf8_is_invalid_encoding() {
        assert_eq!(
            decode_text(&[b'a', 0xFF, b'b']),
            Err(SubtitleError::InvalidEncoding)
        );
        // cp1252 e-acute on its own.
        assert_eq!(decode_text(&[0xE9]), Err(SubtitleError::InvalidEncoding));
        // Truncated multi-byte sequence.
        assert_eq!(
            decode_text(&[b'a', 0xE2, 0x82]),
            Err(SubtitleError::InvalidEncoding)
        );
        // Overlong encoding of '/'.
        assert_eq!(
            decode_text(&[0xC0, 0xAF]),
            Err(SubtitleError::InvalidEncoding)
        );
        // UTF-8 encoded surrogate.
        assert_eq!(
            decode_text(&[0xED, 0xA0, 0x80]),
            Err(SubtitleError::InvalidEncoding)
        );
        // Invalid UTF-8 after a BOM.
        assert_eq!(
            decode_text(&[0xEF, 0xBB, 0xBF, 0xFF]),
            Err(SubtitleError::InvalidEncoding)
        );
    }

    #[test]
    fn decode_nul_is_invalid_encoding() {
        assert_eq!(decode_text(b"a\0b"), Err(SubtitleError::InvalidEncoding));
        assert_eq!(decode_text(b"\0"), Err(SubtitleError::InvalidEncoding));
        // NUL decoded out of UTF-16.
        assert_eq!(
            decode_text(&utf16le("a\0b")),
            Err(SubtitleError::InvalidEncoding)
        );
        // A UTF-32LE BOM looks like UTF-16LE then NUL.
        assert_eq!(
            decode_text(&[0xFF, 0xFE, 0x00, 0x00, 0x41, 0x00, 0x00, 0x00]),
            Err(SubtitleError::InvalidEncoding)
        );
    }

    #[test]
    fn decode_empty_and_blank_input_is_empty_input() {
        assert_eq!(decode_text(b""), Err(SubtitleError::EmptyInput));
        assert_eq!(decode_text(b" \t\r\n "), Err(SubtitleError::EmptyInput));
        assert_eq!(
            decode_text(&[0xEF, 0xBB, 0xBF]),
            Err(SubtitleError::EmptyInput)
        );
        assert_eq!(decode_text(&[0xFF, 0xFE]), Err(SubtitleError::EmptyInput));
        assert_eq!(decode_text(&[0xFE, 0xFF]), Err(SubtitleError::EmptyInput));
        assert_eq!(
            decode_text(&utf16le("  \n")),
            Err(SubtitleError::EmptyInput)
        );
    }

    #[test]
    fn decode_bom_plus_content_is_not_rejected() {
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(b"WEBVTT\n");
        assert_eq!(
            decode_text(&bytes).unwrap(),
            "WEBVTT
"
        );
    }

    #[test]
    fn decode_one_byte_below_and_at_the_cap_is_accepted() {
        let at_cap = vec![b'a'; cap()];
        assert_eq!(decode_text(&at_cap).unwrap().len(), cap());
        let below = vec![b'a'; cap() - 1];
        assert_eq!(decode_text(&below).unwrap().len(), cap() - 1);
    }

    #[test]
    fn decode_one_byte_over_the_cap_is_too_large() {
        let over = vec![b'a'; cap() + 1];
        assert_eq!(
            decode_text(&over),
            Err(SubtitleError::TooLarge {
                limit: MAX_INPUT_BYTES
            })
        );
        // The cap is checked before decoding, so invalid bytes at
        // that size still report TooLarge.
        let over_invalid = vec![0xFFu8; cap() + 1];
        assert_eq!(
            decode_text(&over_invalid),
            Err(SubtitleError::TooLarge {
                limit: MAX_INPUT_BYTES
            })
        );
    }

    // ----- split_lines -----

    #[test]
    fn split_handles_each_terminator() {
        assert_eq!(split_lines("a\nb\nc").unwrap(), ["a", "b", "c"]);
        assert_eq!(split_lines("a\r\nb\r\nc").unwrap(), ["a", "b", "c"]);
        assert_eq!(split_lines("a\rb\rc").unwrap(), ["a", "b", "c"]);
    }

    #[test]
    fn split_handles_mixed_terminators() {
        assert_eq!(split_lines("a\r\nb\rc\nd").unwrap(), ["a", "b", "c", "d"]);
        // \n then \r are two terminators, not one.
        assert_eq!(split_lines("a\n\rb").unwrap(), ["a", "", "b"]);
        // \r\r\n is a lone \r then \r\n.
        assert_eq!(split_lines("a\r\r\nb").unwrap(), ["a", "", "b"]);
    }

    #[test]
    fn split_final_terminator_adds_no_empty_line() {
        assert_eq!(split_lines("a\n").unwrap(), ["a"]);
        assert_eq!(split_lines("a\r\n").unwrap(), ["a"]);
        assert_eq!(split_lines("a\r").unwrap(), ["a"]);
        assert_eq!(split_lines("a\n\n").unwrap(), ["a", ""]);
        assert_eq!(split_lines("\r\n\r\n").unwrap(), ["", ""]);
    }

    #[test]
    fn split_empty_text_has_no_lines() {
        assert!(split_lines("").unwrap().is_empty());
    }

    #[test]
    fn split_keeps_multibyte_text_intact() {
        assert_eq!(
            split_lines("caf\u{e9}\r\n\u{65e5}\u{672c}\u{1F600}\n").unwrap(),
            ["caf\u{e9}", "\u{65e5}\u{672c}\u{1F600}"]
        );
    }

    #[test]
    fn split_accepts_exactly_max_lines_and_rejects_one_more() {
        let ok = "\n".repeat(MAX_LINES);
        assert_eq!(split_lines(&ok).unwrap().len(), MAX_LINES);
        let over = "\n".repeat(MAX_LINES + 1);
        assert_eq!(split_lines(&over), Err(SubtitleError::TooManyLines));
        let over_crlf = "\r\n".repeat(MAX_LINES + 1);
        assert_eq!(split_lines(&over_crlf), Err(SubtitleError::TooManyLines));
    }

    #[test]
    fn split_counts_an_unterminated_last_line_against_max_lines() {
        let mut text = "\n".repeat(MAX_LINES);
        text.push('x');
        assert_eq!(split_lines(&text), Err(SubtitleError::TooManyLines));
    }

    #[test]
    fn split_accepts_exactly_max_line_bytes_and_rejects_one_more() {
        let ok = "a".repeat(MAX_LINE_BYTES);
        assert_eq!(split_lines(&ok).unwrap().len(), 1);
        let over = "a".repeat(MAX_LINE_BYTES + 1);
        assert_eq!(
            split_lines(&over),
            Err(SubtitleError::LineTooLong { line: 1 })
        );
    }

    #[test]
    fn split_line_too_long_reports_its_line_number() {
        let text = format!("x\ny\n{}\nz", "a".repeat(MAX_LINE_BYTES + 1));
        assert_eq!(
            split_lines(&text),
            Err(SubtitleError::LineTooLong { line: 3 })
        );
        let text_cr = format!("x\ry\r{}", "a".repeat(MAX_LINE_BYTES + 1));
        assert_eq!(
            split_lines(&text_cr),
            Err(SubtitleError::LineTooLong { line: 3 })
        );
    }

    #[test]
    fn split_line_length_excludes_the_terminator() {
        let text = format!(
            "{}\r\n{}\r\n",
            "a".repeat(MAX_LINE_BYTES),
            "b".repeat(MAX_LINE_BYTES)
        );
        assert_eq!(split_lines(&text).unwrap().len(), 2);
    }

    #[test]
    fn line_number_is_one_based_and_saturates() {
        assert_eq!(line_number(0), 1);
        assert_eq!(line_number(11), 12);
        assert_eq!(line_number(usize::MAX), u32::MAX);
    }

    // ----- parse_digits / frac_to_ms -----

    #[test]
    fn digits_accepts_plain_ascii_numbers() {
        assert_eq!(parse_digits("0"), Some(0));
        assert_eq!(parse_digits("007"), Some(7));
        assert_eq!(parse_digits("123456789"), Some(123_456_789));
        assert_eq!(parse_digits("999999999"), Some(999_999_999));
    }

    #[test]
    fn digits_rejects_empty_signs_and_non_digits() {
        assert_eq!(parse_digits(""), None);
        assert_eq!(parse_digits("-"), None);
        assert_eq!(parse_digits("+"), None);
        assert_eq!(parse_digits("-1"), None);
        assert_eq!(parse_digits("+1"), None);
        assert_eq!(parse_digits(" 1"), None);
        assert_eq!(parse_digits("1 "), None);
        assert_eq!(parse_digits("1a"), None);
        assert_eq!(parse_digits("0x10"), None);
        assert_eq!(parse_digits("1_0"), None);
        assert_eq!(parse_digits("ffffffff"), None);
    }

    #[test]
    fn digits_rejects_non_ascii_digits() {
        // Arabic-indic one, fullwidth one, superscript two.
        assert_eq!(parse_digits("\u{661}"), None);
        assert_eq!(parse_digits("\u{ff11}"), None);
        assert_eq!(parse_digits("\u{b2}"), None);
        assert_eq!(parse_digits("1\u{661}"), None);
    }

    #[test]
    fn digits_rejects_more_than_nine_digits_before_parsing() {
        assert_eq!(parse_digits("1000000000"), None);
        assert_eq!(parse_digits("4294967296"), None);
        assert_eq!(parse_digits("4294967295"), None);
        assert_eq!(parse_digits("0000000000"), None);
        assert_eq!(parse_digits("18446744073709551616"), None);
        assert_eq!(parse_digits(&"9".repeat(100_000)), None);
    }

    #[test]
    fn frac_scales_by_digit_count() {
        assert_eq!(frac_to_ms("5"), Some(500));
        assert_eq!(frac_to_ms("05"), Some(50));
        assert_eq!(frac_to_ms("12"), Some(120));
        assert_eq!(frac_to_ms("005"), Some(5));
        assert_eq!(frac_to_ms("123"), Some(123));
        assert_eq!(frac_to_ms("999"), Some(999));
        assert_eq!(frac_to_ms("000"), Some(0));
    }

    #[test]
    fn frac_rejects_bad_lengths_and_characters() {
        assert_eq!(frac_to_ms(""), None);
        assert_eq!(frac_to_ms("1234"), None);
        assert_eq!(frac_to_ms("-1"), None);
        assert_eq!(frac_to_ms("1a"), None);
        assert_eq!(frac_to_ms("\u{661}"), None);
    }

    // ----- timestamp_to_ms / check_order / check_cue_count -----

    #[test]
    fn timestamp_combines_fields() {
        assert_eq!(timestamp_to_ms(1, 0, 0, 0, 0), Ok(0));
        assert_eq!(timestamp_to_ms(1, 0, 0, 1, 0), Ok(1_000));
        assert_eq!(timestamp_to_ms(1, 1, 2, 3, 4), Ok(3_723_004));
        assert_eq!(timestamp_to_ms(1, 0, 59, 59, 999), Ok(3_599_999));
    }

    #[test]
    fn timestamp_allows_exactly_one_hundred_hours() {
        assert_eq!(timestamp_to_ms(1, 100, 0, 0, 0), Ok(MAX_TIMESTAMP_MS));
        assert_eq!(
            timestamp_to_ms(1, 99, 59, 59, 999),
            Ok(MAX_TIMESTAMP_MS - 1)
        );
    }

    #[test]
    fn timestamp_above_one_hundred_hours_is_too_large() {
        let err = SubtitleError::TimestampTooLarge { line: 7 };
        assert_eq!(timestamp_to_ms(7, 100, 0, 0, 1), Err(err.clone()));
        assert_eq!(timestamp_to_ms(7, 101, 0, 0, 0), Err(err.clone()));
        assert_eq!(timestamp_to_ms(7, 0, 6_000, 1, 0), Err(err.clone()));
        assert_eq!(timestamp_to_ms(7, 0, 0, 360_001, 0), Err(err));
    }

    #[test]
    fn timestamp_extreme_fields_never_overflow_or_panic() {
        let err = SubtitleError::TimestampTooLarge { line: 1 };
        assert_eq!(
            timestamp_to_ms(1, u32::MAX, u32::MAX, u32::MAX, u32::MAX),
            Err(err.clone())
        );
        assert_eq!(
            timestamp_to_ms(1, 999_999_999, 59, 59, 999),
            Err(err.clone())
        );
        assert_eq!(timestamp_to_ms(1, 0, u32::MAX, 0, 0), Err(err.clone()));
        assert_eq!(timestamp_to_ms(1, 0, 0, u32::MAX, 0), Err(err));
    }

    #[test]
    fn order_allows_equal_and_rejects_end_before_start() {
        assert_eq!(check_order(1, 5, 5), Ok(()));
        assert_eq!(check_order(1, 5, 6), Ok(()));
        assert_eq!(
            check_order(9, 6, 5),
            Err(SubtitleError::TimestampOrder { line: 9 })
        );
    }

    #[test]
    fn cue_count_allows_up_to_max_and_rejects_more() {
        let max = usize::try_from(MAX_CUES).unwrap();
        assert_eq!(check_cue_count(0), Ok(()));
        assert_eq!(check_cue_count(max), Ok(()));
        assert_eq!(
            check_cue_count(max + 1),
            Err(SubtitleError::TooManyCues { limit: MAX_CUES })
        );
        assert_eq!(
            check_cue_count(usize::MAX),
            Err(SubtitleError::TooManyCues { limit: MAX_CUES })
        );
    }

    // ----- text sanitizer -----

    #[test]
    fn sanitize_drops_c0_controls_except_newline() {
        assert_eq!(sanitize_text("a\u{1}b\u{8}c\u{1b}d"), "abcd");
        assert_eq!(sanitize_text("a\tb\rc\u{b}d\u{c}e"), "abcde");
        assert_eq!(sanitize_text("a\nb"), "a\nb");
        assert_eq!(sanitize_text("\u{0}"), "");
    }

    #[test]
    fn sanitize_drops_bidi_controls() {
        for c in ['\u{202A}', '\u{202B}', '\u{202C}', '\u{202D}', '\u{202E}'] {
            assert_eq!(sanitize_text(&format!("a{c}b")), "ab", "{c:?}");
        }
        for c in ['\u{2066}', '\u{2067}', '\u{2068}', '\u{2069}'] {
            assert_eq!(sanitize_text(&format!("a{c}b")), "ab", "{c:?}");
        }
    }

    #[test]
    fn sanitize_keeps_neighbours_of_the_dropped_ranges_and_normal_text() {
        assert_eq!(
            sanitize_text("\u{2029}\u{202F}\u{2065}\u{206A}"),
            "\u{2029}\u{202F}\u{2065}\u{206A}"
        );
        assert_eq!(
            sanitize_text("caf\u{e9} \u{65e5}\u{672c} \u{1F600} \u{7f}"),
            "caf\u{e9} \u{65e5}\u{672c} \u{1F600} \u{7f}"
        );
        assert_eq!(sanitize_text("\u{feff}x"), "\u{feff}x");
        assert!(is_dropped_char('\0'));
        assert!(!is_dropped_char('\n'));
        assert!(!is_dropped_char(' '));
    }

    #[test]
    fn push_cue_text_sanitizes_while_appending() {
        let mut buf = String::new();
        push_cue_text(&mut buf, "a\u{1}b", 1).unwrap();
        push_cue_char(&mut buf, '\u{202E}', 1).unwrap();
        push_cue_char(&mut buf, '\n', 1).unwrap();
        push_cue_text(&mut buf, "c", 1).unwrap();
        assert_eq!(buf, "ab\nc");
    }

    #[test]
    fn push_cue_text_accepts_exactly_the_cap_and_rejects_one_more() {
        let mut buf = String::new();
        push_cue_text(&mut buf, &"a".repeat(MAX_CUE_TEXT_BYTES), 4).unwrap();
        assert_eq!(buf.len(), MAX_CUE_TEXT_BYTES);
        assert_eq!(
            push_cue_char(&mut buf, 'b', 4),
            Err(SubtitleError::CueTooLong { line: 4 })
        );
        assert_eq!(buf.len(), MAX_CUE_TEXT_BYTES);
    }

    #[test]
    fn push_cue_text_counts_utf8_bytes_not_chars() {
        let mut buf = "a".repeat(MAX_CUE_TEXT_BYTES - 1);
        // A 2-byte char does not fit in the last byte.
        assert_eq!(
            push_cue_char(&mut buf, '\u{e9}', 8),
            Err(SubtitleError::CueTooLong { line: 8 })
        );
        assert_eq!(buf.len(), MAX_CUE_TEXT_BYTES - 1);
        push_cue_char(&mut buf, 'z', 8).unwrap();
        assert_eq!(buf.len(), MAX_CUE_TEXT_BYTES);
    }

    #[test]
    fn push_cue_text_stops_growing_at_the_cap_on_a_huge_piece() {
        let mut buf = String::new();
        let piece = "x".repeat(MAX_CUE_TEXT_BYTES * 4);
        assert_eq!(
            push_cue_text(&mut buf, &piece, 2),
            Err(SubtitleError::CueTooLong { line: 2 })
        );
        assert_eq!(buf.len(), MAX_CUE_TEXT_BYTES);
    }

    #[test]
    fn dropped_characters_do_not_count_against_the_cap() {
        let mut buf = String::new();
        push_cue_text(&mut buf, &"\u{1}".repeat(MAX_CUE_TEXT_BYTES * 2), 1).unwrap();
        assert!(buf.is_empty());
    }
}
