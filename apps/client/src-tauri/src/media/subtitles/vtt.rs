//! `media::subtitles::vtt` - WebVTT parser (P8-T03, architecture
//! sections 17.3 and 21.8).
//!
//! Parses a `WEBVTT` header followed by cue blocks with `[H+:]MM:SS.mmm`
//! timings. Input arrives already decoded by `common::decode_text`
//! (BOM stripped).
//!
//! Rules:
//!
//! - The first line must be exactly `WEBVTT`, or `WEBVTT` followed by a
//!   space or tab and free text; anything else (including `WEBVTTX`)
//!   is `MissingHeader`.
//! - Blocks are separated by blank lines. Blocks starting with `NOTE`,
//!   `STYLE` or `REGION` are skipped, as is a block that is only an
//!   identifier with no timing line. A cue block is an optional
//!   identifier line followed by a timing line containing `-->`. The
//!   identifier is never parsed, so an id of `4294967296` is harmless.
//! - A timing line that does not parse is a hard error
//!   (`BadTimestamp`). Stamps are strictly `[H+:]MM:SS.mmm`: `.` only,
//!   minutes and seconds exactly 2 digits and below 60, milliseconds
//!   exactly 3 digits, hours at least 1 digit. Negative, non-digit,
//!   comma and over-long fields are rejected; a stamp past 100 h is
//!   `TimestampTooLarge`; `end < start` is `TimestampOrder`. Cue
//!   settings after the end stamp are ignored.
//! - Text lines are joined with `\n`. `<...>` tags are stripped by one
//!   linear scan per line (an unclosed `<` is kept as literal text) and
//!   then exactly six entities are decoded in one linear pass:
//!   `&amp; &lt; &gt; &nbsp;` (to a space) and `&lrm; &rlm;` (dropped).

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

/// What the parser is doing with the current block when it is not
/// collecting cue text.
#[derive(Clone, Copy)]
enum Mode {
    /// At the start of a block (nothing read yet).
    Idle,
    /// The first line of the block was a non-timing line (a possible
    /// identifier); the next line must be the timing line.
    AfterId,
    /// The rest of this block is ignored.
    Skip,
}

/// Parse already-decoded WebVTT text into cues. Unsorted and
/// unfiltered; `parse_subtitle` sorts, drops empty cues and applies
/// the cue cap.
pub(super) fn parse(text: &str) -> Result<Vec<Cue>, SubtitleError> {
    let lines = split_lines(text)?;
    match lines.first() {
        Some(first) if is_header(first) => {}
        _ => return Err(SubtitleError::MissingHeader),
    }
    let mut cues: Vec<Cue> = Vec::new();
    let mut pending: Option<Pending> = None;
    let mut mode = Mode::Idle;
    let mut piece = String::new();
    let mut decoded = String::new();

    for (index, raw) in lines.iter().enumerate().skip(1) {
        let line_no = line_number(index);
        if raw.trim().is_empty() {
            flush(&mut pending, &mut cues)?;
            mode = Mode::Idle;
            continue;
        }
        if let Some(cue) = pending.as_mut() {
            piece.clear();
            strip_tags(raw, &mut piece);
            decoded.clear();
            decode_entities(&piece, &mut decoded);
            if !decoded.is_empty() {
                if !cue.text.is_empty() {
                    push_cue_char(&mut cue.text, '\n', line_no)?;
                }
                push_cue_text(&mut cue.text, &decoded, line_no)?;
            }
            continue;
        }
        match mode {
            Mode::Idle => {
                if is_skipped_block_start(raw) {
                    mode = Mode::Skip;
                } else if raw.contains("-->") {
                    pending = Some(parse_timing_line(raw, line_no)?);
                    mode = Mode::Skip;
                } else {
                    mode = Mode::AfterId;
                }
            }
            Mode::AfterId => {
                if raw.contains("-->") {
                    pending = Some(parse_timing_line(raw, line_no)?);
                }
                mode = Mode::Skip;
            }
            Mode::Skip => {}
        }
    }
    flush(&mut pending, &mut cues)?;
    Ok(cues)
}

/// True for `WEBVTT` alone or `WEBVTT` followed by a space or tab.
fn is_header(line: &str) -> bool {
    match line.strip_prefix("WEBVTT") {
        Some(rest) => rest.is_empty() || rest.starts_with([' ', '\t']),
        None => false,
    }
}

/// True when a block starting with `line` is a NOTE, STYLE or REGION
/// block.
fn is_skipped_block_start(line: &str) -> bool {
    ["NOTE", "STYLE", "REGION"].iter().any(|keyword| {
        line.strip_prefix(keyword)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with([' ', '\t']))
    })
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

/// Parse `START --> END [settings]` into a pending cue.
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

/// Parse one `[H+:]MM:SS.mmm` stamp into milliseconds. Shape errors
/// are `BadTimestamp`; a value above the 100 h cap is
/// `TimestampTooLarge`.
fn parse_stamp(s: &str, line: u32) -> Result<u32, SubtitleError> {
    let bad = || SubtitleError::BadTimestamp { line };
    let mut parts = s.split(':');
    let first = parts.next().ok_or_else(bad)?;
    let second = parts.next().ok_or_else(bad)?;
    let third = parts.next();
    if parts.next().is_some() {
        return Err(bad());
    }
    let (hours, minutes, rest) = match third {
        Some(rest) => (first, second, rest),
        None => ("0", first, second),
    };
    let (seconds, frac) = rest.split_once('.').ok_or_else(bad)?;
    let h = parse_digits(hours).ok_or_else(bad)?;
    let m = two_digit_below_60(minutes).ok_or_else(bad)?;
    let sec = two_digit_below_60(seconds).ok_or_else(bad)?;
    if frac.len() != 3 {
        return Err(bad());
    }
    let ms = frac_to_ms(frac).ok_or_else(bad)?;
    timestamp_to_ms(line, h, m, sec, ms)
}

/// Exactly two ASCII digits with a value below 60.
fn two_digit_below_60(s: &str) -> Option<u32> {
    if s.len() != 2 {
        return None;
    }
    parse_digits(s).filter(|v| *v < 60)
}

/// Strip `<...>` markup from one line into `out` (dropping control and
/// bidi characters on the way).
///
/// One linear pass: a `<` looks ahead for its `>`; on success the scan
/// jumps past the `>`, on failure the `<` is kept as literal text and
/// no later `<` on this line looks ahead again, so `<<<<...` stays
/// O(n).
fn strip_tags(line: &str, out: &mut String) {
    let mut skip_until: usize = 0;
    let mut no_close = false;
    for (i, c) in line.char_indices() {
        if i < skip_until {
            continue;
        }
        if c == '<' && !no_close {
            let found = line
                .get(i.saturating_add(1)..)
                .and_then(|rest| rest.find('>'));
            match found {
                Some(off) => {
                    // '>' is ASCII: one byte past its offset.
                    skip_until = i.saturating_add(1).saturating_add(off).saturating_add(1);
                    continue;
                }
                None => no_close = true,
            }
        }
        if !is_dropped_char(c) {
            out.push(c);
        }
    }
}

/// The only entities decoded, with their replacement text.
const ENTITIES: [(&str, &str); 6] = [
    ("&amp;", "&"),
    ("&lt;", "<"),
    ("&gt;", ">"),
    ("&nbsp;", " "),
    ("&lrm;", ""),
    ("&rlm;", ""),
];

/// Match one of the six entities at the start of `rest`; returns the
/// entity length in bytes and its replacement.
fn match_entity(rest: &str) -> Option<(usize, &'static str)> {
    ENTITIES
        .iter()
        .find(|entity| rest.starts_with(entity.0))
        .map(|entity| (entity.0.len(), entity.1))
}

/// Decode the six supported entities from `line` into `out` in one
/// pass. Decoded output is never rescanned, so `&amp;lt;` becomes the
/// literal text `&lt;`. Unknown entities stay as written.
fn decode_entities(line: &str, out: &mut String) {
    let mut skip_until: usize = 0;
    for (i, c) in line.char_indices() {
        if i < skip_until {
            continue;
        }
        if c == '&' {
            if let Some((len, replacement)) = line.get(i..).and_then(match_entity) {
                out.push_str(replacement);
                skip_until = i.saturating_add(len);
                continue;
            }
        }
        out.push(c);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::media::subtitles::{parse_vtt, MAX_CUES, MAX_CUE_TEXT_BYTES, MAX_LINE_BYTES};
    use std::time::{Duration, Instant};

    fn p(s: &str) -> Result<Vec<Cue>, SubtitleError> {
        parse_vtt(s.as_bytes())
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
    fn vtt_valid_minimal() {
        let got = p("WEBVTT\n\n00:01.000 --> 00:02.500\nHello\n").unwrap();
        assert_eq!(got, vec![cue(1_000, 2_500, "Hello")]);
    }

    #[test]
    fn vtt_with_and_without_hours() {
        let got = p("WEBVTT\n\n00:01.000 --> 00:02.000\na\n\n01:02:03.004 --> 01:02:04.000\nb\n")
            .unwrap();
        assert_eq!(
            got,
            vec![cue(1_000, 2_000, "a"), cue(3_723_004, 3_724_000, "b")]
        );
    }

    #[test]
    fn vtt_hours_with_more_than_two_digits() {
        let got = p("WEBVTT\n\n100:00:00.000 --> 100:00:00.000\nx\n").unwrap();
        assert_eq!(got, vec![cue(360_000_000, 360_000_000, "x")]);
        let got = p("WEBVTT\n\n001:00:00.000 --> 1:00:01.000\nx\n").unwrap();
        assert_eq!(got, vec![cue(3_600_000, 3_601_000, "x")]);
    }

    #[test]
    fn vtt_cue_identifiers_are_ignored() {
        let got = p(
            "WEBVTT\n\nintro\n00:01.000 --> 00:02.000\nOne\n\n2\n00:03.000 --> 00:04.000\nTwo\n",
        )
        .unwrap();
        assert_eq!(
            got,
            vec![cue(1_000, 2_000, "One"), cue(3_000, 4_000, "Two")]
        );
    }

    #[test]
    fn vtt_identifier_that_looks_like_a_note_prefix_is_still_a_cue() {
        let got = p("WEBVTT\n\nNOTEBOOK\n00:01.000 --> 00:02.000\nx\n").unwrap();
        assert_eq!(got, vec![cue(1_000, 2_000, "x")]);
    }

    #[test]
    fn vtt_cue_settings_are_ignored() {
        let got =
            p("WEBVTT\n\n00:01.000 --> 00:02.000 align:start position:10% line:0 size:50%\nx\n")
                .unwrap();
        assert_eq!(got, vec![cue(1_000, 2_000, "x")]);
    }

    #[test]
    fn vtt_arrow_without_surrounding_spaces_is_accepted() {
        let got = p("WEBVTT\n\n00:01.000-->00:02.000\nx\n").unwrap();
        assert_eq!(got, vec![cue(1_000, 2_000, "x")]);
    }

    #[test]
    fn vtt_note_style_and_region_blocks_are_skipped() {
        let got = p(
            "WEBVTT\n\nNOTE this is a comment\nacross two lines\n\nSTYLE\n::cue { color: red }\n\nREGION\nid:r1\nwidth:40%\n\n00:01.000 --> 00:02.000 region:r1\nkept\n",
        )
        .unwrap();
        assert_eq!(got, vec![cue(1_000, 2_000, "kept")]);
    }

    #[test]
    fn vtt_note_block_containing_an_arrow_is_skipped_without_error() {
        let got =
            p("WEBVTT\n\nNOTE\n00:00:01.000 --> junk\n\n00:01.000 --> 00:02.000\nkept\n").unwrap();
        assert_eq!(got, vec![cue(1_000, 2_000, "kept")]);
    }

    #[test]
    fn vtt_identifier_only_block_is_skipped() {
        let got = p("WEBVTT\n\nlonely id\n\n00:01.000 --> 00:02.000\nx\n").unwrap();
        assert_eq!(got, vec![cue(1_000, 2_000, "x")]);
    }

    #[test]
    fn vtt_block_whose_second_line_is_not_timing_is_skipped() {
        assert_eq!(
            p("WEBVTT\n\nid\nnot a timing line\n00:01.000 --> 00:02.000\nx\n"),
            Err(SubtitleError::NoCues)
        );
    }

    #[test]
    fn vtt_header_only_is_no_cues() {
        assert_eq!(p("WEBVTT\n"), Err(SubtitleError::NoCues));
        assert_eq!(p("WEBVTT\n\n\n"), Err(SubtitleError::NoCues));
    }

    #[test]
    fn vtt_multi_line_text_is_joined_with_newline() {
        let got = p("WEBVTT\n\n00:01.000 --> 00:02.000\nline one\nline two\nline three\n").unwrap();
        assert_eq!(
            got,
            vec![cue(1_000, 2_000, "line one\nline two\nline three")]
        );
    }

    #[test]
    fn vtt_tags_are_stripped_and_entities_decoded() {
        let got = p(
            "WEBVTT\n\n00:01.000 --> 00:02.000\n<v Bob><i>Tom &amp; Jerry</i> &lt;3 &gt; &nbsp;x&lrm;&rlm;</v><c.red>!</c>\n",
        )
        .unwrap();
        assert_eq!(got, vec![cue(1_000, 2_000, "Tom & Jerry <3 >  x!")]);
    }

    #[test]
    fn vtt_decoded_lt_gt_do_not_form_tags() {
        let got = p("WEBVTT\n\n00:01.000 --> 00:02.000\n&lt;b&gt;bold&lt;/b&gt;\n").unwrap();
        assert_eq!(got, vec![cue(1_000, 2_000, "<b>bold</b>")]);
    }

    #[test]
    fn vtt_entities_are_decoded_once_only() {
        let got = p("WEBVTT\n\n00:01.000 --> 00:02.000\n&amp;lt; &amp;amp;\n").unwrap();
        assert_eq!(got, vec![cue(1_000, 2_000, "&lt; &amp;")]);
    }

    #[test]
    fn vtt_unknown_and_unterminated_entities_stay_literal() {
        let got = p("WEBVTT\n\n00:01.000 --> 00:02.000\n&copy; &amp &#65; &\n").unwrap();
        assert_eq!(got, vec![cue(1_000, 2_000, "&copy; &amp &#65; &")]);
    }

    #[test]
    fn vtt_header_variants() {
        for header in [
            "WEBVTT",
            "WEBVTT - title",
            "WEBVTT\ttitle",
            "WEBVTT ",
            "WEBVTT -->",
        ] {
            let got = p(&format!("{header}\n\n00:01.000 --> 00:02.000\nx\n")).unwrap();
            assert_eq!(got, vec![cue(1_000, 2_000, "x")], "{header:?}");
        }
    }

    #[test]
    fn vtt_cue_directly_after_header_without_blank_line_is_read() {
        let got = p("WEBVTT\n00:01.000 --> 00:02.000\nx\n").unwrap();
        assert_eq!(got, vec![cue(1_000, 2_000, "x")]);
    }

    #[test]
    fn vtt_bom_plus_valid_content_is_ok() {
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(b"WEBVTT\r\n\r\n00:01.000 --> 00:02.000\r\nHello\r\n");
        let got = parse_vtt(&bytes).unwrap();
        assert_eq!(got, vec![cue(1_000, 2_000, "Hello")]);
    }

    #[test]
    fn vtt_crlf_line_endings() {
        let got = p("WEBVTT\r\n\r\n00:01.000 --> 00:02.000\r\nHi\r\nthere\r\n\r\n00:03.000 --> 00:04.000\r\nBye\r\n").unwrap();
        assert_eq!(
            got,
            vec![cue(1_000, 2_000, "Hi\nthere"), cue(3_000, 4_000, "Bye")]
        );
    }

    #[test]
    fn vtt_cr_only_line_endings() {
        let got =
            p("WEBVTT\r\r00:01.000 --> 00:02.000\rHi\r\r00:03.000 --> 00:04.000\rBye\r").unwrap();
        assert_eq!(got, vec![cue(1_000, 2_000, "Hi"), cue(3_000, 4_000, "Bye")]);
    }

    #[test]
    fn vtt_mixed_line_endings() {
        let got =
            p("WEBVTT\n\r\n00:01.000 --> 00:02.000\r\nHi\r\r00:03.000 --> 00:04.000\nBye\r\n\n")
                .unwrap();
        assert_eq!(got, vec![cue(1_000, 2_000, "Hi"), cue(3_000, 4_000, "Bye")]);
    }

    #[test]
    fn vtt_end_equal_to_start_is_allowed() {
        let got = p("WEBVTT\n\n00:05.000 --> 00:05.000\nx\n").unwrap();
        assert_eq!(got, vec![cue(5_000, 5_000, "x")]);
    }

    #[test]
    fn vtt_output_is_sorted_by_start() {
        let got =
            p("WEBVTT\n\n00:09.000 --> 00:10.000\nb\n\n00:01.000 --> 00:02.000\na\n").unwrap();
        assert_eq!(got[0].text, "a");
        assert_eq!(got[1].text, "b");
    }

    #[test]
    fn vtt_control_and_bidi_characters_are_dropped() {
        let got = p("WEBVTT\n\n00:01.000 --> 00:02.000\na\u{1}b\u{202E}c\tz\n").unwrap();
        assert_eq!(got, vec![cue(1_000, 2_000, "abcz")]);
    }

    #[test]
    fn vtt_multi_byte_text_around_tags_and_entities() {
        let got = p("WEBVTT\n\n00:01.000 --> 00:02.000\n\u{e9}<b>\u{fc}</b>\u{20ac}<c.x>\u{65e5}\u{672c}\u{1f600}</c>\u{e9}&amp;\u{65e5}&nbsp;\u{1f600}\n").unwrap();
        assert_eq!(
            got,
            vec![cue(
                1_000,
                2_000,
                "\u{e9}\u{fc}\u{20ac}\u{65e5}\u{672c}\u{1f600}\u{e9}&\u{65e5} \u{1f600}"
            )]
        );
    }

    #[test]
    fn vtt_multi_byte_text_inside_tags_is_removed() {
        let got =
            p("WEBVTT\n\n00:01.000 --> 00:02.000\nx<\u{65e5}\u{672c}>y<\u{1f600}>z\n").unwrap();
        assert_eq!(got, vec![cue(1_000, 2_000, "xyz")]);
    }

    #[test]
    fn vtt_unclosed_tag_opener_is_kept_as_text() {
        let got = p("WEBVTT\n\n00:01.000 --> 00:02.000\na < b <\u{e9} \u{65e5}\n").unwrap();
        assert_eq!(got, vec![cue(1_000, 2_000, "a < b <\u{e9} \u{65e5}")]);
    }

    #[test]
    fn vtt_empty_text_cues_are_skipped() {
        let got = p("WEBVTT\n\n00:01.000 --> 00:02.000\n\n00:03.000 --> 00:04.000\n<i></i>\n\n00:05.000 --> 00:06.000\n&lrm;\n\n00:07.000 --> 00:08.000\nreal\n").unwrap();
        assert_eq!(got, vec![cue(7_000, 8_000, "real")]);
    }

    #[test]
    fn vtt_tag_only_line_does_not_leave_a_blank_line() {
        let got = p("WEBVTT\n\n00:01.000 --> 00:02.000\n<c></c>\nText\n").unwrap();
        assert_eq!(got, vec![cue(1_000, 2_000, "Text")]);
    }

    #[test]
    fn vtt_cue_text_at_the_cap_is_ok() {
        let body = "a".repeat(MAX_CUE_TEXT_BYTES);
        let got = p(&format!("WEBVTT\n\n00:01.000 --> 00:02.000\n{body}\n")).unwrap();
        assert_eq!(got[0].text.len(), MAX_CUE_TEXT_BYTES);
    }

    // ----- malicious -----

    #[test]
    fn mal_vtt_missing_header_variants() {
        for input in [
            "WEBVTTX\n\n00:01.000 --> 00:02.000\nx\n",
            "WEBVTT-title\n\n00:01.000 --> 00:02.000\nx\n",
            "webvtt\n\n00:01.000 --> 00:02.000\nx\n",
            "\n00:01.000 --> 00:02.000\nx\n",
            "\nWEBVTT\n\n00:01.000 --> 00:02.000\nx\n",
            " WEBVTT\n\n00:01.000 --> 00:02.000\nx\n",
            "00:01.000 --> 00:02.000\nx\n",
            "1\n00:00:01,000 --> 00:00:02,000\nx\n",
        ] {
            assert_eq!(p(input), Err(SubtitleError::MissingHeader), "{input:?}");
        }
    }

    #[test]
    fn mal_vtt_empty_and_blank_input_is_rejected() {
        assert_eq!(p(""), Err(SubtitleError::EmptyInput));
        assert_eq!(p("\n\r\n \t\n"), Err(SubtitleError::EmptyInput));
    }

    #[test]
    fn mal_vtt_bom_negative_timestamp_is_rejected() {
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(b"WEBVTT\n\n-00:00:01.000 --> 00:00:02.000\nx\n");
        assert_eq!(
            parse_vtt(&bytes),
            Err(SubtitleError::BadTimestamp { line: 3 })
        );
    }

    #[test]
    fn mal_vtt_bom_negative_timestamp_and_4gb_string_is_rejected() {
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(
            b"WEBVTT\r\n\r\n4294967296\r\n-00:00:01.000 --> 00:00:02.000\r\n4294967296\r\n\r\n",
        );
        assert_eq!(
            parse_vtt(&bytes),
            Err(SubtitleError::BadTimestamp { line: 4 })
        );
    }

    #[test]
    fn mal_vtt_negative_fields_are_rejected() {
        for t in [
            "-00:00:01.000 --> 00:00:02.000",
            "00:00:01.000 --> -00:00:02.000",
            "00:-1.000 --> 00:02.000",
            "+0:00:01.000 --> 00:00:02.000",
            "00:01.-00 --> 00:02.000",
        ] {
            assert_eq!(
                p(&format!("WEBVTT\n\n{t}\nx\n")),
                Err(SubtitleError::BadTimestamp { line: 3 }),
                "{t}"
            );
        }
    }

    #[test]
    fn mal_vtt_hour_4294967296_is_rejected() {
        assert_eq!(
            p("WEBVTT\n\n4294967296:00:00.000 --> 4294967296:00:01.000\nx\n"),
            Err(SubtitleError::BadTimestamp { line: 3 })
        );
        assert_eq!(
            p("WEBVTT\n\n00:01.000 --> 4294967296:00:01.000\nx\n"),
            Err(SubtitleError::BadTimestamp { line: 3 })
        );
    }

    #[test]
    fn mal_vtt_milliseconds_4294967296_is_rejected() {
        assert_eq!(
            p("WEBVTT\n\n00:00:01.4294967296 --> 00:00:02.000\nx\n"),
            Err(SubtitleError::BadTimestamp { line: 3 })
        );
        assert_eq!(
            p("WEBVTT\n\n00:00:01.18446744073709551615 --> 00:00:02.000\nx\n"),
            Err(SubtitleError::BadTimestamp { line: 3 })
        );
    }

    #[test]
    fn mal_vtt_4gb_identifier_alone_is_harmless() {
        let got = p("WEBVTT\n\n4294967296\n00:01.000 --> 00:02.000\nx\n").unwrap();
        assert_eq!(got, vec![cue(1_000, 2_000, "x")]);
        let got = p("WEBVTT\n\n18446744073709551616\n00:01.000 --> 00:02.000\nx\n").unwrap();
        assert_eq!(got, vec![cue(1_000, 2_000, "x")]);
    }

    #[test]
    fn mal_vtt_malformed_timing_shapes_are_rejected() {
        for t in [
            "00:00:01,000 --> 00:00:02,000",
            "00:01,000 --> 00:02.000",
            "99:00.000 --> 99:01.000",
            "00:60.000 --> 00:61.000",
            "00:00:99.000 --> 00:00:99.500",
            "00:60:00.000 --> 00:60:01.000",
            "00:01.00 --> 00:02.000",
            "00:01.0000 --> 00:02.000",
            "00:01. --> 00:02.000",
            "0:01.000 --> 00:02.000",
            "00:1.000 --> 00:02.000",
            "00:001.000 --> 00:02.000",
            ":00:01.000 --> 00:02.000",
            "00:00:00:01.000 --> 00:02.000",
            "01.000 --> 02.000",
            "ff:ff.fff --> 00:02.000",
            "00:01;000 --> 00:02.000",
            "\u{661}0:01.000 --> 00:02.000",
            "00:01.000 -->",
            "-->",
            "nonsense --> 00:02.000",
        ] {
            assert_eq!(
                p(&format!("WEBVTT\n\n{t}\nx\n")),
                Err(SubtitleError::BadTimestamp { line: 3 }),
                "{t}"
            );
        }
    }

    #[test]
    fn mal_vtt_hour_over_100_hours_is_too_large() {
        assert_eq!(
            p("WEBVTT\n\n100:00:00.001 --> 100:00:00.001\nx\n"),
            Err(SubtitleError::TimestampTooLarge { line: 3 })
        );
        assert_eq!(
            p("WEBVTT\n\n00:01.000 --> 101:00:00.000\nx\n"),
            Err(SubtitleError::TimestampTooLarge { line: 3 })
        );
        assert_eq!(
            p("WEBVTT\n\n999999999:00:00.000 --> 999999999:00:00.000\nx\n"),
            Err(SubtitleError::TimestampTooLarge { line: 3 })
        );
    }

    #[test]
    fn mal_vtt_end_before_start_is_timestamp_order() {
        assert_eq!(
            p("WEBVTT\n\n00:05.000 --> 00:04.999\nx\n"),
            Err(SubtitleError::TimestampOrder { line: 3 })
        );
    }

    #[test]
    fn mal_vtt_bad_timing_line_after_good_cues_still_rejects_the_file() {
        assert_eq!(
            p("WEBVTT\n\n00:01.000 --> 00:02.000\nok\n\nid\nnonsense --> 00:04.000\nx\n"),
            Err(SubtitleError::BadTimestamp { line: 7 })
        );
    }

    #[test]
    fn mal_vtt_nul_byte_is_rejected_at_decode() {
        assert_eq!(
            parse_vtt(b"WEBVTT\n\n00:01.000 --> 00:02.000\nab\0cd\n"),
            Err(SubtitleError::InvalidEncoding)
        );
    }

    #[test]
    fn mal_vtt_100k_open_angle_lines_after_a_cue_complete_without_panic() {
        let mut s = String::from("WEBVTT\n\n00:01.000 --> 00:02.000\nx\n\n");
        for _ in 0..100_000 {
            s.push_str("<\n");
        }
        let started = Instant::now();
        let got = p(&s).unwrap();
        assert_eq!(got, vec![cue(1_000, 2_000, "x")]);
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn mal_vtt_100k_open_angle_lines_inside_a_cue_are_too_long_not_a_panic() {
        let mut s = String::from("WEBVTT\n\n00:01.000 --> 00:02.000\n");
        for _ in 0..100_000 {
            s.push_str("<\n");
        }
        let started = Instant::now();
        assert!(matches!(p(&s), Err(SubtitleError::CueTooLong { .. })));
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn mal_vtt_strip_tags_is_linear_on_unclosed_openers() {
        let started = Instant::now();
        let line = "<".repeat(MAX_LINE_BYTES);
        let mut out = String::new();
        strip_tags(&line, &mut out);
        assert_eq!(out, line);
        let line = "<\u{e9}".repeat(MAX_LINE_BYTES / 3);
        let mut out = String::new();
        strip_tags(&line, &mut out);
        assert_eq!(out, line);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn mal_vtt_strip_tags_handles_nested_and_adjacent_markup() {
        let mut out = String::new();
        strip_tags("<<b>>x<i>y</i><", &mut out);
        // "<<b>" is one tag (first '<' pairs with first '>'), leaving
        // ">x"; "<i>" and "</i>" go; the trailing '<' is unclosed.
        assert_eq!(out, ">xy<");
    }

    #[test]
    fn mal_vtt_decode_entities_is_linear_on_ampersand_floods() {
        let started = Instant::now();
        for unit in ["&", "&amp", "&amp;", "&&;", "&\u{e9}"] {
            let line = unit.repeat(MAX_LINE_BYTES / unit.len());
            let mut out = String::new();
            decode_entities(&line, &mut out);
            assert!(!out.is_empty());
        }
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn mal_vtt_one_mebibyte_of_newlines_is_an_error_not_a_panic() {
        let bytes = vec![b'\n'; 1024 * 1024];
        assert_eq!(parse_vtt(&bytes), Err(SubtitleError::EmptyInput));
        let mut with_text = vec![b'a'];
        with_text.extend(std::iter::repeat(b'\n').take(1024 * 1024));
        assert_eq!(parse_vtt(&with_text), Err(SubtitleError::TooManyLines));
        let mut with_header = b"WEBVTT".to_vec();
        with_header.extend(std::iter::repeat(b'\n').take(1024 * 1024));
        assert_eq!(parse_vtt(&with_header), Err(SubtitleError::TooManyLines));
    }

    #[test]
    fn mal_vtt_one_mebibyte_of_carriage_returns_is_an_error_not_a_panic() {
        let bytes = vec![b'\r'; 1024 * 1024];
        assert_eq!(parse_vtt(&bytes), Err(SubtitleError::EmptyInput));
    }

    #[test]
    fn mal_vtt_overlong_line_is_rejected() {
        let long = "a".repeat(MAX_LINE_BYTES + 1);
        assert_eq!(
            p(&format!("WEBVTT\n\n00:01.000 --> 00:02.000\n{long}\n")),
            Err(SubtitleError::LineTooLong { line: 4 })
        );
    }

    #[test]
    fn mal_vtt_max_cues_plus_one_is_too_many_cues() {
        let max = usize::try_from(MAX_CUES).unwrap();
        let mut s = String::from("WEBVTT\n\n");
        for _ in 0..=max {
            s.push_str("00:00.000 --> 00:01.000\nx\n\n");
        }
        assert_eq!(p(&s), Err(SubtitleError::TooManyCues { limit: MAX_CUES }));
        // Exactly MAX_CUES is fine.
        let mut ok = String::from("WEBVTT\n\n");
        for _ in 0..max {
            ok.push_str("00:00.000 --> 00:01.000\nx\n\n");
        }
        assert_eq!(p(&ok).unwrap().len(), max);
    }

    #[test]
    fn mal_vtt_cue_text_over_the_cap_is_cue_too_long() {
        let body = "a".repeat(MAX_CUE_TEXT_BYTES + 1);
        assert_eq!(
            p(&format!("WEBVTT\n\n00:01.000 --> 00:02.000\n{body}\n")),
            Err(SubtitleError::CueTooLong { line: 4 })
        );
        // Over the cap across several lines.
        let half = "b".repeat(MAX_CUE_TEXT_BYTES / 2);
        assert!(matches!(
            p(&format!(
                "WEBVTT\n\n00:01.000 --> 00:02.000\n{half}\n{half}\n{half}\n"
            )),
            Err(SubtitleError::CueTooLong { .. })
        ));
    }

    #[test]
    fn mal_vtt_invalid_utf8_and_lone_surrogate_are_rejected_at_decode() {
        assert_eq!(
            parse_vtt(b"WEBVTT\n\n00:01.000 --> 00:02.000\n\xff\xfe\n"),
            Err(SubtitleError::InvalidEncoding)
        );
        assert_eq!(
            parse_vtt(&[0xFF, 0xFE, 0x00, 0xD8]),
            Err(SubtitleError::InvalidEncoding)
        );
    }

    #[test]
    fn mal_vtt_utf16_bom_valid_content_is_ok() {
        let text = "WEBVTT\n\n00:01.000 --> 00:02.000\nHi\n";
        let mut bytes = vec![0xFF, 0xFE];
        for u in text.encode_utf16() {
            bytes.extend_from_slice(&u.to_le_bytes());
        }
        assert_eq!(parse_vtt(&bytes).unwrap(), vec![cue(1_000, 2_000, "Hi")]);
    }
}
