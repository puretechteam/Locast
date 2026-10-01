//! `media::subtitles::ass` - ASS / SSA parser (P8-T03, architecture
//! sections 17.3 and 21.8).
//!
//! Parses the `[Events]` section's `Format:` and `Dialogue:` lines.
//! Input arrives already decoded by `common::decode_text` (BOM
//! stripped).
//!
//! Rules:
//!
//! - Section headers (`[Name]`) are matched case-insensitively after
//!   trimming. Only `[Events]` is parsed; `[Script Info]`,
//!   `[V4+ Styles]`, `[V4 Styles]`, `[Fonts]`, `[Graphics]` and any
//!   other section are skipped. No `[Events]` section is
//!   `MissingHeader`.
//! - A `Format:` line defines the columns (names trimmed, compared
//!   case-insensitively). `Start`, `End` and `Text` are located by
//!   name; all three must exist and `Text` must be the last column,
//!   otherwise `Malformed`. A `Dialogue:` line before any `Format:`
//!   line is `Malformed`.
//! - A `Dialogue:` line is split with `splitn(columns, ',')`, so
//!   commas inside the text are preserved. A line with too few
//!   fields is skipped. `Comment:`, `Picture:`, `Sound:`, `Movie:`,
//!   `Command:` and unknown lines are skipped.
//! - Stamps are `H:MM:SS.cc`: hours 1 to 9 digits, minutes and
//!   seconds exactly 2 digits, fraction 1 to 3 digits (centiseconds
//!   are scaled by `frac_to_ms`). Negative, non-digit and over-long
//!   fields are `BadTimestamp`; more than 100 h is
//!   `TimestampTooLarge`; `end < start` is `TimestampOrder`. A
//!   `Dialogue:` line whose stamps do not parse rejects the file.
//! - Text is processed in one linear scan: `{...}` override blocks
//!   are removed (an unclosed `{` drops the rest of the text),
//!   `\N` and `\n` become a newline and `\h` a space. If an override
//!   block holds `\p` followed by a nonzero number the cue is vector
//!   drawing data, not text, and the whole cue is dropped.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

use super::common::{
    check_cue_count, check_order, frac_to_ms, line_number, parse_digits, push_cue_char,
    push_cue_text, split_lines, timestamp_to_ms,
};
use super::{Cue, SubtitleError};

/// Column layout from a `Format:` line (indices into the field list).
#[derive(Clone, Copy)]
struct Columns {
    /// Total number of columns; `Text` is the last one.
    count: usize,
    start: usize,
    end: usize,
}

/// Parse already-decoded ASS / SSA text into cues. Unsorted and
/// unfiltered; `parse_subtitle` sorts, drops empty cues and applies
/// the cue cap.
pub(super) fn parse(text: &str) -> Result<Vec<Cue>, SubtitleError> {
    let lines = split_lines(text)?;
    let mut cues: Vec<Cue> = Vec::new();
    let mut seen_events = false;
    let mut in_events = false;
    let mut columns: Option<Columns> = None;

    for (index, raw) in lines.iter().enumerate() {
        let line_no = line_number(index);
        let line = raw.trim();
        if let Some(name) = section_name(line) {
            in_events = name.eq_ignore_ascii_case("events");
            seen_events |= in_events;
            continue;
        }
        if !in_events {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        if key.eq_ignore_ascii_case("format") {
            columns = Some(parse_format(value, line_no)?);
        } else if key.eq_ignore_ascii_case("dialogue") {
            let cols = columns.ok_or(SubtitleError::Malformed {
                line: line_no,
                what: "dialogue before format",
            })?;
            if let Some(cue) = parse_dialogue(value.trim_start(), cols, line_no)? {
                check_cue_count(cues.len().saturating_add(1))?;
                cues.push(cue);
            }
        }
    }
    if !seen_events {
        return Err(SubtitleError::MissingHeader);
    }
    Ok(cues)
}

/// The inner name of a `[Section]` header line, trimmed, or `None`
/// when `line` is not a section header.
fn section_name(line: &str) -> Option<&str> {
    line.strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .map(str::trim)
}

/// Parse the value of a `Format:` line into a column layout.
fn parse_format(value: &str, line: u32) -> Result<Columns, SubtitleError> {
    let mut count: usize = 0;
    let mut start: Option<usize> = None;
    let mut end: Option<usize> = None;
    let mut text: Option<usize> = None;
    for (i, name) in value.split(',').enumerate() {
        let name = name.trim();
        if name.eq_ignore_ascii_case("start") {
            start = start.or(Some(i));
        } else if name.eq_ignore_ascii_case("end") {
            end = end.or(Some(i));
        } else if name.eq_ignore_ascii_case("text") {
            text = text.or(Some(i));
        }
        // Trailing empty names (`Format: Start, End, Text,`) are not
        // columns; an empty name in the middle still is.
        if !name.is_empty() {
            count = i.saturating_add(1);
        }
    }
    match (start, end, text) {
        (Some(start), Some(end), Some(text)) => {
            if text.saturating_add(1) != count {
                return Err(SubtitleError::Malformed {
                    line,
                    what: "text column must be last",
                });
            }
            Ok(Columns { count, start, end })
        }
        _ => Err(SubtitleError::Malformed {
            line,
            what: "missing format column",
        }),
    }
}

/// Parse the value of a `Dialogue:` line. `Ok(None)` when the line is
/// skipped (too few fields, vector drawing, or empty text).
fn parse_dialogue(value: &str, cols: Columns, line: u32) -> Result<Option<Cue>, SubtitleError> {
    let mut start_field: Option<&str> = None;
    let mut end_field: Option<&str> = None;
    let mut text_field: Option<&str> = None;
    let mut seen: usize = 0;
    for (i, field) in value.splitn(cols.count, ',').enumerate() {
        if i == cols.start {
            start_field = Some(field);
        }
        if i == cols.end {
            end_field = Some(field);
        }
        seen = i.saturating_add(1);
        text_field = Some(field);
    }
    if seen < cols.count {
        return Ok(None);
    }
    let (Some(start_field), Some(end_field), Some(text_field)) =
        (start_field, end_field, text_field)
    else {
        return Ok(None);
    };
    let start_ms = parse_stamp(start_field.trim(), line)?;
    let end_ms = parse_stamp(end_field.trim(), line)?;
    check_order(line, start_ms, end_ms)?;
    let Some(text) = clean_text(text_field, line)? else {
        return Ok(None);
    };
    if text.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(Cue {
        start_ms,
        end_ms,
        text,
    }))
}

/// Parse one `H:MM:SS.cc` stamp into milliseconds.
fn parse_stamp(s: &str, line: u32) -> Result<u32, SubtitleError> {
    let bad = || SubtitleError::BadTimestamp { line };
    let mut parts = s.split(':');
    let hours = parts.next().ok_or_else(bad)?;
    let minutes = parts.next().ok_or_else(bad)?;
    let rest = parts.next().ok_or_else(bad)?;
    if parts.next().is_some() {
        return Err(bad());
    }
    let (seconds, frac) = rest.split_once('.').ok_or_else(bad)?;
    let h = parse_digits(hours).ok_or_else(bad)?;
    let m = two_digits(minutes).ok_or_else(bad)?;
    let sec = two_digits(seconds).ok_or_else(bad)?;
    let ms = frac_to_ms(frac).ok_or_else(bad)?;
    timestamp_to_ms(line, h, m, sec, ms)
}

/// Exactly two ASCII digits.
fn two_digits(s: &str) -> Option<u32> {
    if s.len() == 2 {
        parse_digits(s)
    } else {
        None
    }
}

/// True when the override block `body` (the text between `{` and `}`)
/// switches on drawing mode: `\p` followed by a nonzero number
/// (leading zeros allowed, so `\p01` counts and `\p0` does not).
fn enables_drawing(body: &str) -> bool {
    body.match_indices("\\p").any(|(i, _)| {
        body.get(i.saturating_add(2)..).is_some_and(|rest| {
            rest.trim_start_matches('0')
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_digit())
        })
    })
}

/// Remember the first cap error from a push instead of returning it:
/// a vector drawing cue must be dropped however long its path data is,
/// so the verdict (drop or `CueTooLong`) is taken after the scan.
fn note(overflow: &mut Option<SubtitleError>, result: Result<(), SubtitleError>) {
    if let Err(e) = result {
        overflow.get_or_insert(e);
    }
}

/// Turn raw ASS dialogue text into plain cue text in one linear
/// scan. `Ok(None)` when an override block turns on drawing mode
/// (decided even if the text is over the size cap); otherwise a cap
/// overflow is `CueTooLong`.
fn clean_text(raw: &str, line: u32) -> Result<Option<String>, SubtitleError> {
    let mut out = String::new();
    let mut overflow: Option<SubtitleError> = None;
    let mut pos: usize = 0;
    while let Some(rest) = raw.get(pos..) {
        let Some(c) = rest.chars().next() else {
            break;
        };
        match c {
            '{' => {
                let inner = rest.get(1..).unwrap_or("");
                match inner.find('}') {
                    Some(off) => {
                        if inner.get(..off).is_some_and(enables_drawing) {
                            return Ok(None);
                        }
                        // '{' and '}' are ASCII: one byte each.
                        pos = pos.saturating_add(off.saturating_add(2));
                    }
                    None => {
                        // Unclosed: the rest of the text is override data.
                        if enables_drawing(inner) {
                            return Ok(None);
                        }
                        break;
                    }
                }
            }
            '\\' => match rest.get(1..).and_then(|r| r.chars().next()) {
                Some('N') | Some('n') => {
                    if overflow.is_none() {
                        note(&mut overflow, push_cue_char(&mut out, '\n', line));
                    }
                    pos = pos.saturating_add(2);
                }
                Some('h') => {
                    if overflow.is_none() {
                        note(&mut overflow, push_cue_char(&mut out, ' ', line));
                    }
                    pos = pos.saturating_add(2);
                }
                _ => {
                    if overflow.is_none() {
                        note(&mut overflow, push_cue_char(&mut out, '\\', line));
                    }
                    pos = pos.saturating_add(1);
                }
            },
            _ => {
                let len = c.len_utf8();
                if overflow.is_none() {
                    let piece = rest.get(..len).unwrap_or("");
                    note(&mut overflow, push_cue_text(&mut out, piece, line));
                }
                pos = pos.saturating_add(len);
            }
        }
    }
    match overflow {
        Some(e) => Err(e),
        None => Ok(Some(out)),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::media::subtitles::{parse_ass, MAX_CUES, MAX_CUE_TEXT_BYTES, MAX_LINE_BYTES};
    use std::time::{Duration, Instant};

    const HEAD: &str = "[Script Info]\nTitle: t\n\n[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n";

    fn p(s: &str) -> Result<Vec<Cue>, SubtitleError> {
        parse_ass(s.as_bytes())
    }

    /// A v4+ file: the standard header and Format line, then `body`.
    fn file(body: &str) -> String {
        format!("{HEAD}{body}")
    }

    fn dlg(start: &str, end: &str, text: &str) -> String {
        format!("Dialogue: 0,{start},{end},Default,,0,0,0,,{text}\n")
    }

    fn cue(start_ms: u32, end_ms: u32, text: &str) -> Cue {
        Cue {
            start_ms,
            end_ms,
            text: text.to_string(),
        }
    }

    /// Parse a file with a single 1 s to 2 s dialogue holding `text`
    /// (dialogue is on line 6).
    fn one(text: &str) -> Result<Vec<Cue>, SubtitleError> {
        p(&file(&dlg("0:00:01.00", "0:00:02.00", text)))
    }

    // ----- behavior -----

    #[test]
    fn ass_valid_minimal_v4_plus() {
        let got = p(&file(&dlg("0:00:01.00", "0:00:03.50", "Hello"))).unwrap();
        assert_eq!(got, vec![cue(1_000, 3_500, "Hello")]);
    }

    #[test]
    fn ass_ssa_v4_variant_with_marked_column() {
        let s = "[Script Info]\nScriptType: v4.00\n\n[V4 Styles]\nFormat: Name, Fontname\nStyle: Default,Arial\n\n[Events]\nFormat: Marked, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\nDialogue: Marked=0,0:00:01.00,0:00:02.00,Default,,0,0,0,,Hi there\n";
        assert_eq!(p(s).unwrap(), vec![cue(1_000, 2_000, "Hi there")]);
    }

    #[test]
    fn ass_commas_in_text_are_preserved() {
        let got = one("a, b, c,,d").unwrap();
        assert_eq!(got, vec![cue(1_000, 2_000, "a, b, c,,d")]);
    }

    #[test]
    fn ass_line_break_and_hard_space_escapes() {
        assert_eq!(one("a\\Nb\\nc\\hd").unwrap()[0].text, "a\nb\nc d");
    }

    #[test]
    fn ass_unknown_backslash_escape_is_kept_literally() {
        assert_eq!(one("a\\qb \\\\ c").unwrap()[0].text, "a\\qb \\\\ c");
    }

    #[test]
    fn ass_override_blocks_are_stripped() {
        let got = one("{\\i1}Hel{\\b1}lo{\\i0} {\\pos(1,2)}world").unwrap();
        assert_eq!(got[0].text, "Hello world");
    }

    #[test]
    fn ass_empty_override_block_is_stripped() {
        assert_eq!(one("a{}b").unwrap()[0].text, "ab");
    }

    #[test]
    fn ass_unclosed_brace_drops_the_rest_of_the_text() {
        assert_eq!(one("kept {\\b1 gone").unwrap()[0].text, "kept ");
        assert_eq!(one("kept{").unwrap()[0].text, "kept");
    }

    #[test]
    fn ass_text_that_is_only_overrides_or_breaks_is_skipped() {
        assert_eq!(one("{\\an8}{\\b1}"), Err(SubtitleError::NoCues));
        assert_eq!(one("\\N\\h"), Err(SubtitleError::NoCues));
    }

    #[test]
    fn ass_drawing_cue_is_dropped_but_p0_is_kept() {
        let body = format!(
            "{}{}{}",
            dlg("0:00:01.00", "0:00:02.00", "{\\p1}m 0 0 l 10 10{\\p0}"),
            dlg("0:00:03.00", "0:00:04.00", "{\\p0}text"),
            dlg("0:00:05.00", "0:00:06.00", "{\\pos(1,2)}placed"),
        );
        let got = p(&file(&body)).unwrap();
        assert_eq!(
            got,
            vec![cue(3_000, 4_000, "text"), cue(5_000, 6_000, "placed")]
        );
    }

    #[test]
    fn ass_drawing_scale_variants() {
        assert_eq!(one("{\\p4}m 0 0"), Err(SubtitleError::NoCues));
        assert_eq!(one("{\\i1\\p01}m 0 0"), Err(SubtitleError::NoCues));
        assert_eq!(one("{\\p00}x").unwrap()[0].text, "x");
        assert_eq!(one("{\\pbo5}x").unwrap()[0].text, "x");
        // An unclosed block that turns drawing on still drops the cue.
        assert_eq!(one("hi {\\p1"), Err(SubtitleError::NoCues));
    }

    #[test]
    fn ass_comment_and_other_event_lines_are_skipped() {
        let body = format!(
            "Comment: 0,0:00:00.00,0:00:01.00,Default,,0,0,0,,ignored\nPicture: 0,0:00:00.00,0:00:01.00,x.png\nSound: 0,0:00:00.00,0:00:01.00,x.wav\nMovie: 0,0:00:00.00,0:00:01.00,x.avi\nCommand: 0,0:00:00.00,0:00:01.00,x\nrandom junk without colon\nMystery: 1,2,3\n{}",
            dlg("0:00:01.00", "0:00:02.00", "real")
        );
        assert_eq!(p(&file(&body)).unwrap(), vec![cue(1_000, 2_000, "real")]);
    }

    #[test]
    fn ass_comment_with_bad_timestamp_is_not_an_error() {
        let body = format!(
            "Comment: 0,garbage,-1,Default,,0,0,0,,x\n{}",
            dlg("0:00:01.00", "0:00:02.00", "real")
        );
        assert_eq!(p(&file(&body)).unwrap().len(), 1);
    }

    #[test]
    fn ass_dialogue_before_format_is_malformed() {
        let s = "[Events]\nDialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,x\n";
        assert_eq!(
            p(s),
            Err(SubtitleError::Malformed {
                line: 2,
                what: "dialogue before format"
            })
        );
    }

    #[test]
    fn ass_text_not_last_is_malformed() {
        let s = "[Events]\nFormat: Layer, Start, End, Text, Style\nDialogue: 0,0:00:01.00,0:00:02.00,x,Default\n";
        assert_eq!(
            p(s),
            Err(SubtitleError::Malformed {
                line: 2,
                what: "text column must be last"
            })
        );
    }

    #[test]
    fn ass_missing_columns_are_malformed() {
        for fmt in [
            "Format: Layer, End, Text",
            "Format: Layer, Start, Text",
            "Format: Layer, Start, End",
            "Format: ",
            "Format:",
        ] {
            let s = format!("[Events]\n{fmt}\nDialogue: 0,0:00:01.00,0:00:02.00,x\n");
            assert!(
                matches!(p(&s), Err(SubtitleError::Malformed { line: 2, .. })),
                "{fmt}"
            );
        }
    }

    #[test]
    fn ass_format_names_are_trimmed_and_case_insensitive() {
        let s = "[Events]\nFORMAT:   layer ,  START,end  ,  TEXT  \nDIALOGUE: 0,0:00:01.00,0:00:02.00,hello\n";
        assert_eq!(p(s).unwrap(), vec![cue(1_000, 2_000, "hello")]);
    }

    #[test]
    fn ass_columns_may_be_reordered() {
        let s = "[Events]\nFormat: End, Start, Text\nDialogue: 0:00:02.00,0:00:01.00,hi, there\n";
        assert_eq!(p(s).unwrap(), vec![cue(1_000, 2_000, "hi, there")]);
    }

    #[test]
    fn ass_a_later_format_line_replaces_the_earlier_one() {
        let s = "[Events]\nFormat: Start, End, Text\nDialogue: 0:00:01.00,0:00:02.00,one\nFormat: Layer, Start, End, Text\nDialogue: 0,0:00:03.00,0:00:04.00,two\n";
        assert_eq!(
            p(s).unwrap(),
            vec![cue(1_000, 2_000, "one"), cue(3_000, 4_000, "two")]
        );
    }

    #[test]
    fn ass_short_dialogue_is_skipped() {
        let body = format!(
            "Dialogue: 0,0:00:01.00,0:00:02.00,Default\nDialogue:\nDialogue: \n{}",
            dlg("0:00:03.00", "0:00:04.00", "kept")
        );
        assert_eq!(p(&file(&body)).unwrap(), vec![cue(3_000, 4_000, "kept")]);
    }

    #[test]
    fn ass_dialogue_with_empty_text_is_skipped() {
        let body = format!(
            "{}{}",
            dlg("0:00:01.00", "0:00:02.00", ""),
            dlg("0:00:03.00", "0:00:04.00", "x")
        );
        assert_eq!(p(&file(&body)).unwrap(), vec![cue(3_000, 4_000, "x")]);
    }

    #[test]
    fn ass_no_events_section_is_missing_header() {
        assert_eq!(
            p("[Script Info]\nTitle: t\n\n[V4+ Styles]\nFormat: Name\nStyle: Default\n"),
            Err(SubtitleError::MissingHeader)
        );
        assert_eq!(p("just some text\n"), Err(SubtitleError::MissingHeader));
        // Format and Dialogue lines outside [Events] are ignored.
        assert_eq!(
            p("Format: Start, End, Text\nDialogue: 0:00:01.00,0:00:02.00,x\n"),
            Err(SubtitleError::MissingHeader)
        );
    }

    #[test]
    fn ass_events_without_dialogue_is_no_cues() {
        assert_eq!(p(&file("")), Err(SubtitleError::NoCues));
    }

    #[test]
    fn ass_section_names_are_case_insensitive_and_trimmed() {
        for header in ["[events]", "[EVENTS]", "  [ Events ]  ", "\t[Events]\t"] {
            let s =
                format!("{header}\nFormat: Start, End, Text\nDialogue: 0:00:01.00,0:00:02.00,x\n");
            assert_eq!(p(&s).unwrap(), vec![cue(1_000, 2_000, "x")], "{header:?}");
        }
    }

    #[test]
    fn ass_extra_sections_are_skipped() {
        let s = format!(
            "[Script Info]\nDialogue: 0,0:00:00.00,0:00:01.00,Default,,0,0,0,,not me\n[V4+ Styles]\nFormat: Name, Text\nStyle: Default,x\n[Fonts]\nfontname: arial_0.ttf\nABCDEF==\n[Graphics]\nfilename: a.png\n[Aegisub Project Garbage]\nLast Style Storage: Default\n{HEAD}{}[Fonts]\nDialogue: 0,0:00:09.00,0:00:10.00,Default,,0,0,0,,also not me\n",
            dlg("0:00:01.00", "0:00:02.00", "me")
        );
        assert_eq!(p(&s).unwrap(), vec![cue(1_000, 2_000, "me")]);
    }

    #[test]
    fn ass_fraction_digits_one_to_three_are_scaled() {
        let body = format!(
            "{}{}{}",
            dlg("0:00:01.5", "0:00:02.5", "a"),
            dlg("0:00:03.25", "0:00:04.25", "b"),
            dlg("0:00:05.125", "0:00:06.125", "c"),
        );
        assert_eq!(
            p(&file(&body)).unwrap(),
            vec![
                cue(1_500, 2_500, "a"),
                cue(3_250, 4_250, "b"),
                cue(5_125, 6_125, "c")
            ]
        );
    }

    #[test]
    fn ass_multi_digit_hours_and_exact_100_hours() {
        let got = p(&file(&dlg("10:00:00.00", "100:00:00.00", "x"))).unwrap();
        assert_eq!(got, vec![cue(36_000_000, 360_000_000, "x")]);
    }

    #[test]
    fn ass_cues_are_sorted_by_start() {
        let body = format!(
            "{}{}",
            dlg("0:00:05.00", "0:00:06.00", "late"),
            dlg("0:00:01.00", "0:00:02.00", "early")
        );
        let got = p(&file(&body)).unwrap();
        assert_eq!(got[0].text, "early");
        assert_eq!(got[1].text, "late");
    }

    #[test]
    fn ass_control_characters_in_text_are_dropped() {
        assert_eq!(one("a\tb\u{1}c\u{202E}d").unwrap()[0].text, "abcd");
    }

    #[test]
    fn ass_crlf_cr_and_mixed_line_endings() {
        let lf = file(&dlg("0:00:01.00", "0:00:02.00", "x"));
        let expected = vec![cue(1_000, 2_000, "x")];
        assert_eq!(p(&lf.replace('\n', "\r\n")).unwrap(), expected);
        assert_eq!(p(&lf.replace('\n', "\r")).unwrap(), expected);
        let mut mixed = String::new();
        for (i, l) in lf.lines().enumerate() {
            mixed.push_str(l);
            mixed.push_str(["\r\n", "\r", "\n"][i % 3]);
        }
        assert_eq!(p(&mixed).unwrap(), expected);
    }

    #[test]
    fn ass_bom_with_valid_content_is_ok() {
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(file(&dlg("0:00:01.00", "0:00:02.00", "x")).as_bytes());
        assert_eq!(parse_ass(&bytes).unwrap(), vec![cue(1_000, 2_000, "x")]);
    }

    #[test]
    fn ass_multibyte_text_around_braces_and_backslashes() {
        let got = one("\u{e9}{\\b1}\u{4e2d}\u{6587}\\N\u{1F600}{x}\u{e9}\\h\u{1F600}").unwrap();
        assert_eq!(
            got[0].text,
            "\u{e9}\u{4e2d}\u{6587}\n\u{1F600}\u{e9} \u{1F600}"
        );
        // A backslash directly before a multi-byte char is kept literally.
        assert_eq!(
            one("\\\u{e9}\\\u{1F600}").unwrap()[0].text,
            "\\\u{e9}\\\u{1F600}"
        );
        // Multi-byte char right after an opening brace, unclosed.
        assert_eq!(one("a{\u{1F600}").unwrap()[0].text, "a");
        // Multi-byte chars inside a closed block, next to `\p`.
        assert_eq!(one("a{\u{4e2d}\\p\u{4e2d}}b").unwrap()[0].text, "ab");
    }

    #[test]
    fn ass_trailing_backslash_is_literal() {
        assert_eq!(one("abc\\").unwrap()[0].text, "abc\\");
        assert_eq!(one("\\").unwrap()[0].text, "\\");
    }

    #[test]
    fn ass_dialogue_key_with_spaces_around_it() {
        let s = "[Events]\nFormat: Start, End, Text\n   Dialogue :0:00:01.00,0:00:02.00,x\n";
        assert_eq!(p(s).unwrap(), vec![cue(1_000, 2_000, "x")]);
    }

    // ----- malicious -----

    #[test]
    fn mal_ass_bom_negative_timestamp_is_rejected() {
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(file(&dlg("-0:00:01.00", "0:00:02.00", "x")).as_bytes());
        assert_eq!(
            parse_ass(&bytes),
            Err(SubtitleError::BadTimestamp { line: 6 })
        );
    }

    #[test]
    fn mal_ass_bom_negative_timestamp_and_4gb_string_is_rejected() {
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        let body = "Dialogue: 0,-0:00:01.00,0:00:02.00,Default,,4294967296,4294967296,4294967296,,File size: 4GB\n";
        bytes.extend_from_slice(file(body).as_bytes());
        assert_eq!(
            parse_ass(&bytes),
            Err(SubtitleError::BadTimestamp { line: 6 })
        );
    }

    #[test]
    fn mal_ass_negative_and_malformed_stamps_are_rejected() {
        for (start, end) in [
            ("-0:00:01.00", "0:00:02.00"),
            ("0:00:01.00", "-0:00:02.00"),
            ("0:-0:01.00", "0:00:02.00"),
            ("0:00:-1.00", "0:00:02.00"),
            ("0:00:01.-5", "0:00:02.00"),
            ("+0:00:01.00", "0:00:02.00"),
            ("0:0x:01.00", "0:00:02.00"),
            ("ffffffffffffffff", "0:00:02.00"),
            ("0:00:01,00", "0:00:02.00"),
            ("0:00:01", "0:00:02.00"),
            ("0:00:01.", "0:00:02.00"),
            ("0:00:01.0000", "0:00:02.00"),
            ("0:0:01.00", "0:00:02.00"),
            ("0:00:001.00", "0:00:02.00"),
            ("0:00:00:01.00", "0:00:02.00"),
            ("", "0:00:02.00"),
            ("\u{661}:00:01.00", "0:00:02.00"),
        ] {
            assert_eq!(
                p(&file(&dlg(start, end, "x"))),
                Err(SubtitleError::BadTimestamp { line: 6 }),
                "{start:?} {end:?}"
            );
        }
    }

    #[test]
    fn mal_ass_hour_4294967296_is_rejected() {
        assert_eq!(
            p(&file(&dlg(
                "4294967296:00:00.00",
                "4294967296:00:01.00",
                "x"
            ))),
            Err(SubtitleError::BadTimestamp { line: 6 })
        );
    }

    #[test]
    fn mal_ass_ten_digit_and_longer_centiseconds_are_rejected() {
        assert_eq!(
            p(&file(&dlg("0:00:01.00", "0:00:01.4294967296", "x"))),
            Err(SubtitleError::BadTimestamp { line: 6 })
        );
        assert_eq!(
            p(&file(&dlg(
                "0:00:01.00",
                "0:00:01.99999999999999999999",
                "x"
            ))),
            Err(SubtitleError::BadTimestamp { line: 6 })
        );
    }

    #[test]
    fn mal_ass_4gb_margin_fields_and_text_are_harmless() {
        let body = "Dialogue: 0,0:00:01.00,0:00:02.00,Default,,4294967296,4294967296,4294967296,,File size: 4GB\n";
        assert_eq!(
            p(&file(body)).unwrap(),
            vec![cue(1_000, 2_000, "File size: 4GB")]
        );
        // 4294967296 as the layer and in style / name fields is harmless too.
        let body = "Dialogue: 4294967296,0:00:01.00,0:00:02.00,4294967296,4294967296,0,0,0,,x\n";
        assert_eq!(p(&file(body)).unwrap().len(), 1);
    }

    #[test]
    fn mal_ass_hour_over_100_hours_is_too_large() {
        assert_eq!(
            p(&file(&dlg("100:00:00.01", "100:00:01.00", "x"))),
            Err(SubtitleError::TimestampTooLarge { line: 6 })
        );
        assert_eq!(
            p(&file(&dlg("999999999:00:00.00", "999999999:00:01.00", "x"))),
            Err(SubtitleError::TimestampTooLarge { line: 6 })
        );
        // Minutes and seconds past 59 are tolerated under the 100 h cap.
        assert_eq!(
            p(&file(&dlg("0:99:99.99", "100:00:00.00", "x")))
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn mal_ass_end_before_start_is_timestamp_order() {
        assert_eq!(
            p(&file(&dlg("0:00:02.00", "0:00:01.00", "x"))),
            Err(SubtitleError::TimestampOrder { line: 6 })
        );
        // Equal start and end is allowed.
        assert_eq!(
            p(&file(&dlg("0:00:02.00", "0:00:02.00", "x")))
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn mal_ass_bad_timestamp_on_a_drawing_cue_still_rejects_the_file() {
        assert_eq!(
            p(&file(&dlg("-0:00:01.00", "0:00:02.00", "{\\p1}m 0 0"))),
            Err(SubtitleError::BadTimestamp { line: 6 })
        );
    }

    #[test]
    fn mal_ass_bad_timestamp_after_good_cues_rejects_the_file() {
        let body = format!(
            "{}{}",
            dlg("0:00:01.00", "0:00:02.00", "good"),
            dlg("zz", "0:00:02.00", "bad")
        );
        assert_eq!(
            p(&file(&body)),
            Err(SubtitleError::BadTimestamp { line: 7 })
        );
    }

    #[test]
    fn mal_ass_nul_byte_is_rejected_at_decode() {
        assert_eq!(
            parse_ass(
                b"[Events]\nFormat: Start, End, Text\nDialogue: 0:00:01.00,0:00:02.00,a\x00b\n"
            ),
            Err(SubtitleError::InvalidEncoding)
        );
    }

    #[test]
    fn mal_ass_many_open_braces_are_linear() {
        let started = Instant::now();
        assert_eq!(one(&"{".repeat(60_000)), Err(SubtitleError::NoCues));
        let mixed = format!("a{}", "{".repeat(60_000));
        assert_eq!(one(&mixed).unwrap()[0].text, "a");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn mal_ass_many_closed_and_unclosed_blocks_are_linear() {
        let started = Instant::now();
        let closed = format!("{}x", "{}".repeat(30_000));
        assert_eq!(one(&closed).unwrap()[0].text, "x");
        assert_eq!(one(&"{\\".repeat(30_000)), Err(SubtitleError::NoCues));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn mal_ass_deeply_nested_braces_do_not_panic() {
        let depth = 3_000;
        let text = format!("a{}b{}c", "{".repeat(depth), "}".repeat(depth));
        // The first '}' closes the block; the remaining '}' are literal.
        let got = one(&text).unwrap();
        assert!(got[0].text.starts_with('a'));
        assert!(got[0].text.ends_with('c'));
        assert_eq!(got[0].text.len(), depth + 1);
    }

    #[test]
    fn mal_ass_backslash_flood_is_linear_and_capped() {
        let started = Instant::now();
        assert_eq!(one(&"\\".repeat(8_000)).unwrap()[0].text.len(), 8_000);
        assert_eq!(
            one(&"\\".repeat(30_000)),
            Err(SubtitleError::CueTooLong { line: 6 })
        );
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn mal_ass_one_mebibyte_of_newlines_is_an_error_not_a_panic() {
        let bytes = vec![b'\n'; 1024 * 1024];
        assert_eq!(parse_ass(&bytes), Err(SubtitleError::EmptyInput));
        let mut with_text = b"a".to_vec();
        with_text.resize(1 + 1024 * 1024, b'\n');
        assert_eq!(parse_ass(&with_text), Err(SubtitleError::TooManyLines));
        let mut with_header = b"[Events]".to_vec();
        with_header.resize(8 + 1024 * 1024, b'\n');
        assert_eq!(parse_ass(&with_header), Err(SubtitleError::TooManyLines));
    }

    #[test]
    fn mal_ass_one_mebibyte_of_carriage_returns_is_an_error_not_a_panic() {
        let bytes = vec![b'\r'; 1024 * 1024];
        assert_eq!(parse_ass(&bytes), Err(SubtitleError::EmptyInput));
    }

    #[test]
    fn angle_bracket_text_survives_literally_so_renderers_must_use_text_content() {
        let out = p(&file(&dlg("0:00:01.00", "0:00:02.00", "<img src=x>"))).unwrap();
        assert_eq!(out, vec![cue(1_000, 2_000, "<img src=x>")]);
    }

    #[test]
    fn mal_ass_overlong_line_is_rejected() {
        let long = "a".repeat(MAX_LINE_BYTES + 1);
        assert_eq!(
            p(&file(&dlg("0:00:01.00", "0:00:02.00", &long))),
            Err(SubtitleError::LineTooLong { line: 6 })
        );
    }

    #[test]
    fn mal_ass_max_cues_plus_one_is_too_many_cues() {
        let max = usize::try_from(MAX_CUES).unwrap();
        let line = dlg("0:00:00.00", "0:00:01.00", "x");
        let mut s = String::from(HEAD);
        for _ in 0..=max {
            s.push_str(&line);
        }
        assert_eq!(p(&s), Err(SubtitleError::TooManyCues { limit: MAX_CUES }));
        // Exactly MAX_CUES is fine.
        let mut ok = String::from(HEAD);
        for _ in 0..max {
            ok.push_str(&line);
        }
        assert_eq!(p(&ok).unwrap().len(), max);
    }

    #[test]
    fn mal_ass_cue_text_over_the_cap_is_cue_too_long() {
        let body = "a".repeat(MAX_CUE_TEXT_BYTES + 1);
        assert_eq!(one(&body), Err(SubtitleError::CueTooLong { line: 6 }));
        // Exactly at the cap is fine.
        let at_cap = "a".repeat(MAX_CUE_TEXT_BYTES);
        assert_eq!(one(&at_cap).unwrap()[0].text.len(), MAX_CUE_TEXT_BYTES);
        // Line-break escapes count against the cap too.
        let breaks = "\\N".repeat(MAX_CUE_TEXT_BYTES + 1);
        assert_eq!(one(&breaks), Err(SubtitleError::CueTooLong { line: 6 }));
    }

    #[test]
    fn mal_ass_oversized_drawing_cue_is_dropped_not_rejected() {
        let path = "m 0 0 l 1 1 ".repeat(750); // 9000 bytes, over the cap
        assert!(path.len() > MAX_CUE_TEXT_BYTES);
        let body = format!(
            "{}{}",
            dlg("0:00:01.00", "0:00:02.00", &format!("{{\\p1}}{path}")),
            dlg("0:00:03.00", "0:00:04.00", "kept")
        );
        assert_eq!(p(&file(&body)).unwrap(), vec![cue(3_000, 4_000, "kept")]);
        // \p after over-cap text, in a closed block and in an unclosed one.
        let long = "a".repeat(MAX_CUE_TEXT_BYTES + 10);
        assert_eq!(
            one(&format!("{long}{{\\p2}}{path}")),
            Err(SubtitleError::NoCues)
        );
        assert_eq!(one(&format!("{long}{{\\p1")), Err(SubtitleError::NoCues));
        // Over-cap line breaks before the drawing switch.
        let breaks = "\\N".repeat(MAX_CUE_TEXT_BYTES + 1);
        assert_eq!(
            one(&format!("{breaks}{{\\p1}}x")),
            Err(SubtitleError::NoCues)
        );
    }

    #[test]
    fn mal_ass_oversized_non_drawing_cue_is_still_cue_too_long() {
        let body = "a".repeat(MAX_CUE_TEXT_BYTES + 1);
        assert_eq!(one(&body), Err(SubtitleError::CueTooLong { line: 6 }));
        let at_cap = "a".repeat(MAX_CUE_TEXT_BYTES);
        assert_eq!(one(&at_cap).unwrap()[0].text.len(), MAX_CUE_TEXT_BYTES);
        // Line-break escapes count against the cap too.
        let breaks = "\\N".repeat(MAX_CUE_TEXT_BYTES + 1);
        assert_eq!(one(&breaks), Err(SubtitleError::CueTooLong { line: 6 }));
        // A \p0 block and other overrides do not rescue an over-cap cue.
        let long = "a".repeat(MAX_CUE_TEXT_BYTES + 10);
        assert_eq!(
            one(&format!("{long}{{\\p0}}{long}")),
            Err(SubtitleError::CueTooLong { line: 6 })
        );
        assert_eq!(
            one(&format!("{{\\i1}}{long}{{\\pos(1,2)}}")),
            Err(SubtitleError::CueTooLong { line: 6 })
        );
    }

    #[test]
    fn ass_format_with_trailing_empty_column_names_is_accepted() {
        let s = "[Events]\nFormat: Start, End, Text,\nDialogue: 0:00:01.00,0:00:02.00,a, b\n";
        assert_eq!(p(s).unwrap(), vec![cue(1_000, 2_000, "a, b")]);
        let s =
            "[Events]\nFormat: Layer, Start, End, Text, , \nDialogue: 0,0:00:01.00,0:00:02.00,x\n";
        assert_eq!(p(s).unwrap(), vec![cue(1_000, 2_000, "x")]);
        // An empty name in the middle is an ordinary unnamed column.
        let s = "[Events]\nFormat: Start, , End, Text\nDialogue: 0:00:01.00,junk,0:00:02.00,x\n";
        assert_eq!(p(s).unwrap(), vec![cue(1_000, 2_000, "x")]);
    }

    #[test]
    fn mal_ass_text_not_last_is_still_rejected_with_trailing_commas() {
        for fmt in [
            "Format: Start, End, Text, Style",
            "Format: Start, End, Text, Style,",
            "Format: Start, End, Text, , Style",
        ] {
            let s = format!("[Events]\n{fmt}\nDialogue: 0:00:01.00,0:00:02.00,x,y\n");
            assert_eq!(
                p(&s),
                Err(SubtitleError::Malformed {
                    line: 2,
                    what: "text column must be last"
                }),
                "{fmt}"
            );
        }
        // Only empty names: no columns at all.
        assert!(matches!(
            p("[Events]\nFormat: , ,\n"),
            Err(SubtitleError::Malformed { line: 2, .. })
        ));
    }

    #[test]
    fn mal_ass_invalid_utf8_and_lone_surrogate_are_rejected_at_decode() {
        assert_eq!(
            parse_ass(
                b"[Events]\nFormat: Start, End, Text\nDialogue: 0:00:01.00,0:00:02.00,\xff\xfe\n"
            ),
            Err(SubtitleError::InvalidEncoding)
        );
        assert_eq!(
            parse_ass(&[0xFF, 0xFE, 0x00, 0xD8]),
            Err(SubtitleError::InvalidEncoding)
        );
    }

    #[test]
    fn mal_ass_utf16_bom_valid_content_is_ok() {
        let text = "[Events]\nFormat: Start, End, Text\nDialogue: 0:00:01.00,0:00:02.00,Hi\n";
        let mut bytes = vec![0xFF, 0xFE];
        for u in text.encode_utf16() {
            bytes.extend_from_slice(&u.to_le_bytes());
        }
        assert_eq!(parse_ass(&bytes).unwrap(), vec![cue(1_000, 2_000, "Hi")]);
    }

    #[test]
    fn mal_ass_huge_format_line_is_handled_without_allocating_per_column() {
        let names = "x,".repeat(20_000);
        let s = format!("[Events]\nFormat: {names}Start, End, Text\n");
        assert_eq!(p(&s), Err(SubtitleError::NoCues));
        let s = format!(
            "[Events]\nFormat: {names}Start, End, Text\nDialogue: {names}0:00:01.00,0:00:02.00,hi\n"
        );
        assert_eq!(p(&s).unwrap(), vec![cue(1_000, 2_000, "hi")]);
    }
}
