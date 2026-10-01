//! `media::subtitles::srt` - SRT (SubRip) parser (P8-T03, architecture
//! sections 17.3 and 21.8).
//!
//! Parses blocks of an optional index line, a
//! `H:MM:SS,mmm --> H:MM:SS,mmm` timing line and text lines. Input
//! arrives already decoded by `common::decode_text`.
//!
//! Rules:
//!
//! - Blocks are separated by blank (whitespace-only) lines. The timing
//!   line is the first line of a block containing `-->`. Anything
//!   before it in the block (the index line, or garbage) is ignored
//!   and never parsed, so an index of `4294967296` is harmless. A block
//!   with no `-->` line is skipped.
//! - A timing line that does not parse is a hard error
//!   (`BadTimestamp`), so negative, non-digit, over-long and
//!   overflowing values reject the whole file. `end < start` is
//!   `TimestampOrder`; a stamp past 100 h is `TimestampTooLarge`.
//! - Stamps are `H{1,3}:M{1,2}:S{1,2}[,.]d{1,3}`. Minutes and seconds
//!   are NOT required to be below 60 (SRT does not demand it); only
//!   the 100 h total cap applies. Coordinates after the end stamp are
//!   ignored.
//! - Text lines are joined with `\n`. Markup in `<...>` and `{...}` is
//!   stripped by one linear scan per line; an unclosed `<` or `{` is
//!   kept as literal text. HTML entities are left as they are.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

use super::common::{
    check_cue_count, check_order, frac_to_ms, is_dropped_char, line_number, parse_digits,
    push_cue_char, push_cue_text, split_lines, timestamp_to_ms,
};
use super::{Cue, SubtitleError};

/// A cue whose timing line has been read and whose text is being
/// collected.
struct Pending {
    start_ms: u32,
    end_ms: u32,
    text: String,
}

/// Parse already-decoded SRT (SubRip) text into cues. Unsorted and
/// unfiltered; `parse_subtitle` sorts, drops empty cues and applies
/// the cue cap.
pub(super) fn parse(text: &str) -> Result<Vec<Cue>, SubtitleError> {
    let lines = split_lines(text)?;
    let mut cues: Vec<Cue> = Vec::new();
    let mut pending: Option<Pending> = None;
    let mut piece = String::new();

    for (index, raw) in lines.iter().enumerate() {
        let line_no = line_number(index);
        if raw.trim().is_empty() {
            flush(&mut pending, &mut cues)?;
            continue;
        }
        match pending.as_mut() {
            Some(cue) => {
                piece.clear();
                strip_tags(raw, &mut piece);
                if !piece.is_empty() {
                    if !cue.text.is_empty() {
                        push_cue_char(&mut cue.text, '\n', line_no)?;
                    }
                    push_cue_text(&mut cue.text, &piece, line_no)?;
                }
            }
            None => {
                if raw.contains("-->") {
                    pending = Some(parse_timing_line(raw, line_no)?);
                }
                // Otherwise: index line or garbage before the timing
                // line; ignored. A block that never gets a timing
                // line is skipped until the next blank line.
            }
        }
    }
    flush(&mut pending, &mut cues)?;
    Ok(cues)
}

/// Move a finished cue into `cues` (skipping empty text), checking
/// the cue cap before the push.
fn flush(pending: &mut Option<Pending>, cues: &mut Vec<Cue>) -> Result<(), SubtitleError> {
    if let Some(p) = pending.take() {
        if !p.text.trim().is_empty() {
            check_cue_count(cues.len().saturating_add(1))?;
            cues.push(Cue {
                start_ms: p.start_ms,
                end_ms: p.end_ms,
                text: p.text,
            });
        }
    }
    Ok(())
}

/// Parse `START --> END [coordinates]` into a pending cue.
fn parse_timing_line(raw: &str, line: u32) -> Result<Pending, SubtitleError> {
    let bad = || SubtitleError::BadTimestamp { line };
    let (left, right) = raw.split_once("-->").ok_or_else(bad)?;
    let end_token = right.split_whitespace().next().ok_or_else(bad)?;
    let start_ms = parse_stamp(left.trim(), line)?;
    let end_ms = parse_stamp(end_token, line)?;
    check_order(line, start_ms, end_ms)?;
    Ok(Pending {
        start_ms,
        end_ms,
        text: String::new(),
    })
}

/// Parse one `H{1,3}:M{1,2}:S{1,2}[,.]d{1,3}` stamp into
/// milliseconds. Shape errors are `BadTimestamp`; a value above the
/// 100 h cap is `TimestampTooLarge`.
fn parse_stamp(s: &str, line: u32) -> Result<u32, SubtitleError> {
    let bad = || SubtitleError::BadTimestamp { line };
    let mut parts = s.split(':');
    let hours = parts.next().ok_or_else(bad)?;
    let minutes = parts.next().ok_or_else(bad)?;
    let rest = parts.next().ok_or_else(bad)?;
    if parts.next().is_some() {
        return Err(bad());
    }
    let (seconds, frac) = rest.split_once([',', '.']).ok_or_else(bad)?;
    let h = field(hours, 3).ok_or_else(bad)?;
    let m = field(minutes, 2).ok_or_else(bad)?;
    let sec = field(seconds, 2).ok_or_else(bad)?;
    let ms = frac_to_ms(frac).ok_or_else(bad)?;
    timestamp_to_ms(line, h, m, sec, ms)
}

/// A digit field of at most `max_digits` ASCII digits.
fn field(s: &str, max_digits: usize) -> Option<u32> {
    if s.len() > max_digits {
        return None;
    }
    parse_digits(s)
}

/// Strip `<...>` and `{...}` markup from one line into `out`
/// (dropping control and bidi characters on the way).
///
/// One linear pass: each opener looks ahead for its closer; on
/// success the scan jumps past the closer, on failure the opener is
/// kept as literal text and that kind of opener never looks ahead
/// again on this line, so `<<<<...` and `{{{{...` stay O(n).
fn strip_tags(line: &str, out: &mut String) {
    let mut skip_until: usize = 0;
    let mut no_angle_close = false;
    let mut no_brace_close = false;
    for (i, c) in line.char_indices() {
        if i < skip_until {
            continue;
        }
        let (closer, gave_up) = match c {
            '<' => ('>', &mut no_angle_close),
            '{' => ('}', &mut no_brace_close),
            _ => {
                if !is_dropped_char(c) {
                    out.push(c);
                }
                continue;
            }
        };
        if !*gave_up {
            let found = line
                .get(i.saturating_add(1)..)
                .and_then(|rest| rest.find(closer));
            match found {
                Some(off) => {
                    // The closer is ASCII: one byte past its offset.
                    skip_until = i.saturating_add(1).saturating_add(off).saturating_add(1);
                    continue;
                }
                None => *gave_up = true,
            }
        }
        out.push(c);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::media::subtitles::{parse_srt, MAX_CUES, MAX_CUE_TEXT_BYTES, MAX_LINE_BYTES};
    use std::time::{Duration, Instant};

    fn p(s: &str) -> Result<Vec<Cue>, SubtitleError> {
        parse_srt(s.as_bytes())
    }

    fn cue(start_ms: u32, end_ms: u32, text: &str) -> Cue {
        Cue {
            start_ms,
            end_ms,
            text: text.to_string(),
        }
    }

    // ----- behavior -----

    #[test]
    fn srt_valid_multi_cue() {
        let got = p(
            "1\n00:00:01,000 --> 00:00:02,500\nHello\n\n2\n00:00:03,000 --> 00:00:04,000\nWorld\n",
        )
        .unwrap();
        assert_eq!(
            got,
            vec![cue(1_000, 2_500, "Hello"), cue(3_000, 4_000, "World")]
        );
    }

    #[test]
    fn srt_multi_line_text_is_joined_with_newline() {
        let got = p("1\n00:00:01,000 --> 00:00:02,000\nline one\nline two\nline three\n").unwrap();
        assert_eq!(
            got,
            vec![cue(1_000, 2_000, "line one\nline two\nline three")]
        );
    }

    #[test]
    fn srt_accepts_dot_and_comma_separators() {
        let got = p("1\n00:00:01.250 --> 00:00:02,750\nx\n").unwrap();
        assert_eq!(got, vec![cue(1_250, 2_750, "x")]);
    }

    #[test]
    fn srt_fraction_digit_count_scales() {
        let got =
            p("1\n00:00:01,5 --> 00:00:02,25\na\n\n2\n00:00:03,007 --> 00:00:04,1\nb\n").unwrap();
        assert_eq!(got[0], cue(1_500, 2_250, "a"));
        assert_eq!(got[1], cue(3_007, 4_100, "b"));
    }

    #[test]
    fn srt_short_hour_minute_second_fields() {
        let got = p("1\n0:0:1,000 --> 1:2:3,004\nx\n").unwrap();
        assert_eq!(got, vec![cue(1_000, 3_723_004, "x")]);
    }

    #[test]
    fn srt_crlf_line_endings() {
        let got = p("1\r\n00:00:01,000 --> 00:00:02,000\r\nHi\r\nthere\r\n\r\n2\r\n00:00:03,000 --> 00:00:04,000\r\nBye\r\n").unwrap();
        assert_eq!(
            got,
            vec![cue(1_000, 2_000, "Hi\nthere"), cue(3_000, 4_000, "Bye")]
        );
    }

    #[test]
    fn srt_cr_only_line_endings() {
        let got =
            p("1\r00:00:01,000 --> 00:00:02,000\rHi\r\r2\r00:00:03,000 --> 00:00:04,000\rBye\r")
                .unwrap();
        assert_eq!(got, vec![cue(1_000, 2_000, "Hi"), cue(3_000, 4_000, "Bye")]);
    }

    #[test]
    fn srt_mixed_line_endings() {
        let got = p("1\n00:00:01,000 --> 00:00:02,000\r\nHi\r\r2\r\n00:00:03,000 --> 00:00:04,000\nBye\r\n\n").unwrap();
        assert_eq!(got, vec![cue(1_000, 2_000, "Hi"), cue(3_000, 4_000, "Bye")]);
    }

    #[test]
    fn srt_bom_plus_valid_content_is_ok() {
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(b"1\r\n00:00:01,000 --> 00:00:02,000\r\nHello\r\n");
        let got = parse_srt(&bytes).unwrap();
        assert_eq!(got, vec![cue(1_000, 2_000, "Hello")]);
    }

    #[test]
    fn srt_missing_index_line_is_fine() {
        let got = p("00:00:01,000 --> 00:00:02,000\nNo index\n").unwrap();
        assert_eq!(got, vec![cue(1_000, 2_000, "No index")]);
    }

    #[test]
    fn srt_garbage_index_line_is_ignored() {
        let got = p("not-a-number\n00:00:01,000 --> 00:00:02,000\nText\n").unwrap();
        assert_eq!(got, vec![cue(1_000, 2_000, "Text")]);
    }

    #[test]
    fn srt_block_without_arrow_is_skipped() {
        let got =
            p("hello there\nno timing here\n\n1\n00:00:01,000 --> 00:00:02,000\nkept\n").unwrap();
        assert_eq!(got, vec![cue(1_000, 2_000, "kept")]);
    }

    #[test]
    fn srt_file_with_only_arrowless_blocks_is_no_cues() {
        assert_eq!(p("just\ntext\n\nmore text\n"), Err(SubtitleError::NoCues));
    }

    #[test]
    fn srt_tags_are_stripped() {
        let got = p("1\n00:00:01,000 --> 00:00:02,000\n<i>Hello</i> {\\an8}<font color=\"red\">there</font>\n").unwrap();
        assert_eq!(got, vec![cue(1_000, 2_000, "Hello there")]);
    }

    #[test]
    fn srt_tag_only_line_does_not_leave_a_blank_line() {
        let got = p("1\n00:00:01,000 --> 00:00:02,000\n{\\an8}\nText\n").unwrap();
        assert_eq!(got, vec![cue(1_000, 2_000, "Text")]);
    }

    #[test]
    fn srt_html_entities_are_left_as_is() {
        let got = p("1\n00:00:01,000 --> 00:00:02,000\nTom &amp; Jerry &lt;3 &nbsp;x\n").unwrap();
        assert_eq!(
            got,
            vec![cue(1_000, 2_000, "Tom &amp; Jerry &lt;3 &nbsp;x")]
        );
    }

    #[test]
    fn srt_end_equal_to_start_is_allowed() {
        let got = p("1\n00:00:05,000 --> 00:00:05,000\nx\n").unwrap();
        assert_eq!(got, vec![cue(5_000, 5_000, "x")]);
    }

    #[test]
    fn srt_end_before_start_is_timestamp_order() {
        assert_eq!(
            p("1\n00:00:05,000 --> 00:00:04,999\nx\n"),
            Err(SubtitleError::TimestampOrder { line: 2 })
        );
    }

    #[test]
    fn srt_trailing_coordinates_are_ignored() {
        let got = p("1\n00:00:01,000 --> 00:00:02,000 X1:63 X2:223 Y1:43 Y2:58\nx\n").unwrap();
        assert_eq!(got, vec![cue(1_000, 2_000, "x")]);
    }

    #[test]
    fn srt_arrow_without_surrounding_spaces_is_accepted() {
        let got = p("1\n00:00:01,000-->00:00:02,000\nx\n").unwrap();
        assert_eq!(got, vec![cue(1_000, 2_000, "x")]);
    }

    #[test]
    fn srt_empty_text_cues_are_skipped() {
        let got = p("1\n00:00:01,000 --> 00:00:02,000\n\n2\n00:00:03,000 --> 00:00:04,000\n<i></i>\n\n3\n00:00:05,000 --> 00:00:06,000\nreal\n").unwrap();
        assert_eq!(got, vec![cue(5_000, 6_000, "real")]);
    }

    #[test]
    fn srt_output_is_sorted_by_start() {
        let got = p("1\n00:00:09,000 --> 00:00:10,000\nb\n\n2\n00:00:01,000 --> 00:00:02,000\na\n")
            .unwrap();
        assert_eq!(got[0].text, "a");
        assert_eq!(got[1].text, "b");
    }

    #[test]
    fn srt_control_and_bidi_characters_are_dropped() {
        let got = p("1\n00:00:01,000 --> 00:00:02,000\na\u{1}b\u{202E}c\tz\n").unwrap();
        assert_eq!(got, vec![cue(1_000, 2_000, "abcz")]);
    }

    #[test]
    fn srt_multi_byte_text_around_tag_boundaries() {
        let got = p("1\n00:00:01,000 --> 00:00:02,000\n\u{e9}<b>\u{fc}</b>\u{20ac}{\\an8}\u{65e5}\u{672c}\u{1f600}\n").unwrap();
        assert_eq!(
            got,
            vec![cue(
                1_000,
                2_000,
                "\u{e9}\u{fc}\u{20ac}\u{65e5}\u{672c}\u{1f600}"
            )]
        );
    }

    #[test]
    fn srt_unclosed_tags_are_kept_as_text() {
        let got = p("1\n00:00:01,000 --> 00:00:02,000\na < b { c <\u{e9} {\u{65e5}\n").unwrap();
        assert_eq!(got, vec![cue(1_000, 2_000, "a < b { c <\u{e9} {\u{65e5}")]);
    }

    #[test]
    fn srt_multi_byte_text_inside_tags_is_removed() {
        let got =
            p("1\n00:00:01,000 --> 00:00:02,000\nx<\u{65e5}\u{672c}>y{\u{1f600}}z\n").unwrap();
        assert_eq!(got, vec![cue(1_000, 2_000, "xyz")]);
    }

    #[test]
    fn srt_exactly_100_hours_is_allowed() {
        let got = p("1\n100:00:00,000 --> 100:00:00,000\nx\n").unwrap();
        assert_eq!(got, vec![cue(360_000_000, 360_000_000, "x")]);
    }

    #[test]
    fn srt_cue_text_at_the_cap_is_ok() {
        let body = "a".repeat(MAX_CUE_TEXT_BYTES);
        let got = p(&format!("1\n00:00:01,000 --> 00:00:02,000\n{body}\n")).unwrap();
        assert_eq!(got[0].text.len(), MAX_CUE_TEXT_BYTES);
    }

    // ----- malicious -----

    #[test]
    fn mal_srt_bom_negative_timestamp_and_4gb_string_is_rejected() {
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(
            b"4294967296\r\n-00:00:01,000 --> 00:00:02,000\r\n4294967296\r\n\r\n",
        );
        assert_eq!(
            parse_srt(&bytes),
            Err(SubtitleError::BadTimestamp { line: 2 })
        );
    }

    #[test]
    fn mal_srt_4gb_index_alone_is_harmless() {
        let got = p("4294967296\n00:00:01,000 --> 00:00:02,000\nx\n").unwrap();
        assert_eq!(got, vec![cue(1_000, 2_000, "x")]);
    }

    #[test]
    fn mal_srt_negative_timestamp_is_rejected() {
        for t in [
            "-00:00:01,000 --> 00:00:02,000",
            "00:00:01,000 --> -00:00:02,000",
            "00:-1:01,000 --> 00:00:02,000",
            "+0:00:01,000 --> 00:00:02,000",
        ] {
            assert_eq!(
                p(&format!("1\n{t}\nx\n")),
                Err(SubtitleError::BadTimestamp { line: 2 }),
                "{t}"
            );
        }
    }

    #[test]
    fn mal_srt_hour_4294967296_is_rejected() {
        assert_eq!(
            p("1\n4294967296:00:00,000 --> 4294967296:00:01,000\nx\n"),
            Err(SubtitleError::BadTimestamp { line: 2 })
        );
        assert_eq!(
            p("1\n00:00:01,000 --> 4294967296:00:01,000\nx\n"),
            Err(SubtitleError::BadTimestamp { line: 2 })
        );
    }

    #[test]
    fn mal_srt_twenty_digit_millisecond_field_is_rejected() {
        assert_eq!(
            p("1\n00:00:01,18446744073709551615 --> 00:00:02,000\nx\n"),
            Err(SubtitleError::BadTimestamp { line: 2 })
        );
        assert_eq!(
            p("1\n00:00:01,00000000000000000000 --> 00:00:02,000\nx\n"),
            Err(SubtitleError::BadTimestamp { line: 2 })
        );
    }

    #[test]
    fn mal_srt_hex_and_non_digit_fields_are_rejected() {
        for t in [
            "ff:ff:ff,fff --> 00:00:01,000",
            "00:00:01,000 --> ffffffffffffffff",
            "00:00:01;000 --> 00:00:02,000",
            "00:00 --> 00:00:02,000",
            "00:00:01,000:5 --> 00:00:02,000",
            "00:00:01 --> 00:00:02,000",
            "00:00:01, --> 00:00:02,000",
            "00:00:01,0000 --> 00:00:02,000",
            "00:00:001,000 --> 00:00:02,000",
            "00:000:01,000 --> 00:00:02,000",
            "0000:00:01,000 --> 00:00:02,000",
            "\u{661}:00:01,000 --> 00:00:02,000",
            "00:00:01,000 -->",
            "-->",
        ] {
            assert_eq!(
                p(&format!("1\n{t}\nx\n")),
                Err(SubtitleError::BadTimestamp { line: 2 }),
                "{t}"
            );
        }
    }

    #[test]
    fn mal_srt_minutes_and_seconds_above_59_follow_the_total_cap() {
        // Decision: SRT does not require min/sec < 60, so 99:99:99,999
        // is not a shape error. It is 362_439_999 ms, past the 100 h
        // cap, hence TimestampTooLarge. A value with large min/sec but
        // under the cap is accepted.
        assert_eq!(
            p("1\n99:99:99,999 --> 99:99:99,999\nx\n"),
            Err(SubtitleError::TimestampTooLarge { line: 2 })
        );
        let got = p("1\n00:99:99,999 --> 00:99:99,999\nx\n").unwrap();
        assert_eq!(got, vec![cue(6_039_999, 6_039_999, "x")]);
    }

    #[test]
    fn mal_srt_timestamp_beyond_100_hours_is_too_large() {
        assert_eq!(
            p("1\n100:00:00,001 --> 100:00:00,001\nx\n"),
            Err(SubtitleError::TimestampTooLarge { line: 2 })
        );
        assert_eq!(
            p("1\n00:00:01,000 --> 999:59:59,999\nx\n"),
            Err(SubtitleError::TimestampTooLarge { line: 2 })
        );
    }

    #[test]
    fn mal_srt_bad_timing_line_after_good_cues_still_rejects_the_file() {
        assert_eq!(
            p("1\n00:00:01,000 --> 00:00:02,000\nok\n\n2\nnonsense --> 00:00:04,000\nx\n"),
            Err(SubtitleError::BadTimestamp { line: 6 })
        );
    }

    #[test]
    fn mal_srt_nul_byte_is_rejected_at_decode() {
        assert_eq!(
            parse_srt(b"1\n00:00:01,000 --> 00:00:02,000\nab\0cd\n"),
            Err(SubtitleError::InvalidEncoding)
        );
    }

    #[test]
    fn mal_srt_100k_open_brace_lines_complete_without_panic() {
        let mut s = String::from("1\n00:00:01,000 --> 00:00:02,000\nx\n\n");
        for _ in 0..100_000 {
            s.push_str("{\n");
        }
        let started = Instant::now();
        // The arrowless trailing block is skipped; the file stays valid.
        let got = p(&s).unwrap();
        assert_eq!(got, vec![cue(1_000, 2_000, "x")]);
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn mal_srt_100k_open_angle_lines_complete_without_panic() {
        let mut s = String::from("1\n00:00:01,000 --> 00:00:02,000\nx\n\n");
        for _ in 0..100_000 {
            s.push_str("<\n");
        }
        let started = Instant::now();
        let got = p(&s).unwrap();
        assert_eq!(got, vec![cue(1_000, 2_000, "x")]);
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn mal_srt_100k_open_tag_lines_inside_a_cue_are_too_long_not_a_panic() {
        let mut s = String::from("1\n00:00:01,000 --> 00:00:02,000\n");
        for _ in 0..100_000 {
            s.push_str("{<\n");
        }
        let started = Instant::now();
        assert!(matches!(p(&s), Err(SubtitleError::CueTooLong { .. })));
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn mal_srt_strip_tags_is_linear_on_unclosed_openers() {
        let started = Instant::now();
        for opener in ["<", "{", "<{", "{<"] {
            let line = opener.repeat(MAX_LINE_BYTES / opener.len());
            let mut out = String::new();
            strip_tags(&line, &mut out);
            assert_eq!(out, line, "{opener}");
        }
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn mal_srt_strip_tags_handles_nested_and_adjacent_markup() {
        let mut out = String::new();
        strip_tags("<<b>>x<i>{y}</i>}{", &mut out);
        // "<<b>" is one tag (the first '<' pairs with the first '>'),
        // leaving ">x"; then "{y}" is removed, the lone '}' and the
        // unclosed '{' stay.
        assert_eq!(out, ">x}{");
    }

    #[test]
    fn mal_srt_one_mebibyte_of_newlines_is_an_error_not_a_panic() {
        let bytes = vec![b'\n'; 1024 * 1024];
        assert_eq!(parse_srt(&bytes), Err(SubtitleError::EmptyInput));
        let mut with_text = vec![b'a'];
        with_text.extend(std::iter::repeat(b'\n').take(1024 * 1024));
        assert_eq!(parse_srt(&with_text), Err(SubtitleError::TooManyLines));
    }

    #[test]
    fn mal_srt_one_mebibyte_of_carriage_returns_is_an_error_not_a_panic() {
        let bytes = vec![b'\r'; 1024 * 1024];
        assert_eq!(parse_srt(&bytes), Err(SubtitleError::EmptyInput));
    }

    #[test]
    fn mal_srt_overlong_line_is_rejected() {
        let long = "a".repeat(MAX_LINE_BYTES + 1);
        assert_eq!(
            p(&format!("1\n00:00:01,000 --> 00:00:02,000\n{long}\n")),
            Err(SubtitleError::LineTooLong { line: 3 })
        );
    }

    #[test]
    fn mal_srt_max_cues_plus_one_is_too_many_cues() {
        let max = usize::try_from(MAX_CUES).unwrap();
        let mut s = String::new();
        for i in 0..=max {
            s.push_str(&format!("{i}\n00:00:00,000 --> 00:00:01,000\nx\n\n"));
        }
        assert_eq!(p(&s), Err(SubtitleError::TooManyCues { limit: MAX_CUES }));
        // Exactly MAX_CUES is fine.
        let mut ok = String::new();
        for i in 0..max {
            ok.push_str(&format!("{i}\n00:00:00,000 --> 00:00:01,000\nx\n\n"));
        }
        assert_eq!(p(&ok).unwrap().len(), max);
    }

    #[test]
    fn mal_srt_cue_text_over_the_cap_is_cue_too_long() {
        let body = "a".repeat(MAX_CUE_TEXT_BYTES + 1);
        assert_eq!(
            p(&format!("1\n00:00:01,000 --> 00:00:02,000\n{body}\n")),
            Err(SubtitleError::CueTooLong { line: 3 })
        );
        // Over the cap across several lines.
        let half = "b".repeat(MAX_CUE_TEXT_BYTES / 2);
        assert!(matches!(
            p(&format!(
                "1\n00:00:01,000 --> 00:00:02,000\n{half}\n{half}\n{half}\n"
            )),
            Err(SubtitleError::CueTooLong { .. })
        ));
    }

    #[test]
    fn mal_srt_invalid_utf8_and_lone_surrogate_are_rejected_at_decode() {
        assert_eq!(
            parse_srt(b"1\n00:00:01,000 --> 00:00:02,000\n\xff\xfe\n"),
            Err(SubtitleError::InvalidEncoding)
        );
        assert_eq!(
            parse_srt(&[0xFF, 0xFE, 0x00, 0xD8]),
            Err(SubtitleError::InvalidEncoding)
        );
    }

    #[test]
    fn mal_srt_utf16_bom_valid_content_is_ok() {
        let text = "1\n00:00:01,000 --> 00:00:02,000\nHi\n";
        let mut bytes = vec![0xFF, 0xFE];
        for u in text.encode_utf16() {
            bytes.extend_from_slice(&u.to_le_bytes());
        }
        assert_eq!(parse_srt(&bytes).unwrap(), vec![cue(1_000, 2_000, "Hi")]);
    }
}
