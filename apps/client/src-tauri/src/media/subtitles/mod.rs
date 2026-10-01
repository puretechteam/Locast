//! `media::subtitles` - hand-written, dependency-free SRT, WebVTT
//! and ASS/SSA parsers for untrusted subtitle files (P8-T03,
//! architecture sections 17.3 and 21.8).
//!
//! There is exactly one accept / reject gate, [`parse_subtitle`]:
//! it decodes the bytes (size cap before any allocation, BOM
//! handling, strict UTF-8 / UTF-16, NUL and empty-input rejection),
//! runs the format parser, then sorts the cues stably by start time,
//! drops empty-text cues, enforces the cue cap, and returns
//! `NoCues` when nothing is left. [`parse_srt`], [`parse_vtt`] and
//! [`parse_ass`] are thin wrappers over it; they are what the
//! cargo-fuzz targets under `apps/client/src-tauri/fuzz/` call.
//!
//! Every hard cap in [`limits`] rejects the whole file; nothing is
//! truncated. Errors ([`SubtitleError`]) carry only numbers and
//! fixed labels, never file content.
//!
//! Design notes:
//! - The parsers are hand-written and dependency-free on purpose.
//!   The architecture stack table names the `subparse` crate, but the
//!   Rust parsers are the source of truth (architecture 17.3) and
//!   this keeps the untrusted-input surface small and fuzzable.
//! - Cue text is plain text with markup stripped, but `<` and `>`
//!   can survive (VTT entities decode to a literal `<`, ASS keeps
//!   unknown angle text). Any renderer MUST insert cue text with
//!   `textContent`, never as HTML.
//! - A renderer needs its own cap on active cues and total text;
//!   this module only bounds one file.
//! - The optional `style` field of architecture 17.3 is deferred.
//!
//! The module is pure (no I/O, no globals) and panic-free by
//! construction: `unwrap`, `expect`, indexing and `panic!` are
//! denied by lint outside test code.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

pub mod common;
pub mod error;
pub mod limits;

mod ass;
mod srt;
mod vtt;

#[cfg(test)]
mod corpus_pin;
#[cfg(test)]
mod perf;

pub use error::SubtitleError;
pub use limits::{
    MAX_CUES, MAX_CUE_TEXT_BYTES, MAX_INPUT_BYTES, MAX_LINES, MAX_LINE_BYTES, MAX_TIMESTAMP_DIGITS,
    MAX_TIMESTAMP_MS,
};

/// One subtitle cue. Times are milliseconds from the start of the
/// media; `u32` covers the 100 hour cap ([`MAX_TIMESTAMP_MS`]).
///
/// `text` is plain text with markup removed and may contain `\n`
/// for line breaks. Parsed cues satisfy `start_ms <= end_ms <=
/// MAX_TIMESTAMP_MS` and `0 < text.len() <= MAX_CUE_TEXT_BYTES`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cue {
    /// Cue start in milliseconds.
    pub start_ms: u32,
    /// Cue end in milliseconds (never before `start_ms`).
    pub end_ms: u32,
    /// Cue text, tag-stripped and sanitized.
    pub text: String,
}

/// The subtitle formats this module can parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubtitleFormat {
    /// SubRip (`.srt`).
    Srt,
    /// WebVTT (`.vtt`).
    Vtt,
    /// Advanced / SubStation Alpha (`.ass`, `.ssa`).
    Ass,
}

impl SubtitleFormat {
    /// Map a codec / extension string to a format, ASCII
    /// case-insensitively: `srt`; `vtt` or `webvtt`; `ass` or `ssa`.
    /// Anything else (including surrounding whitespace) is `None`.
    pub fn from_codec(codec: &str) -> Option<Self> {
        let is = |name: &str| codec.eq_ignore_ascii_case(name);
        if is("srt") {
            Some(SubtitleFormat::Srt)
        } else if is("vtt") || is("webvtt") {
            Some(SubtitleFormat::Vtt)
        } else if is("ass") || is("ssa") {
            Some(SubtitleFormat::Ass)
        } else {
            None
        }
    }

    /// The canonical codec string: only `"srt"`, `"vtt"` or `"ass"`.
    pub fn codec_str(self) -> &'static str {
        match self {
            SubtitleFormat::Srt => "srt",
            SubtitleFormat::Vtt => "vtt",
            SubtitleFormat::Ass => "ass",
        }
    }
}

/// Parse an untrusted subtitle file. The single entry point.
///
/// Applies [`common::decode_text`], the format parser, then
/// normalizes the result (see the module docs). Never panics on any
/// input; every failure is a [`SubtitleError`].
pub fn parse_subtitle(fmt: SubtitleFormat, bytes: &[u8]) -> Result<Vec<Cue>, SubtitleError> {
    let text = common::decode_text(bytes)?;
    let cues = match fmt {
        SubtitleFormat::Srt => srt::parse(&text)?,
        SubtitleFormat::Vtt => vtt::parse(&text)?,
        SubtitleFormat::Ass => ass::parse(&text)?,
    };
    finalize_cues(cues)
}

/// [`parse_subtitle`] for SRT input.
pub fn parse_srt(bytes: &[u8]) -> Result<Vec<Cue>, SubtitleError> {
    parse_subtitle(SubtitleFormat::Srt, bytes)
}

/// [`parse_subtitle`] for WebVTT input.
pub fn parse_vtt(bytes: &[u8]) -> Result<Vec<Cue>, SubtitleError> {
    parse_subtitle(SubtitleFormat::Vtt, bytes)
}

/// [`parse_subtitle`] for ASS / SSA input.
pub fn parse_ass(bytes: &[u8]) -> Result<Vec<Cue>, SubtitleError> {
    parse_subtitle(SubtitleFormat::Ass, bytes)
}

/// Shared post-processing: drop empty-text cues, enforce the cue
/// cap, sort once (stable, by `start_ms`), and reject an empty
/// result with `NoCues`.
fn finalize_cues(mut cues: Vec<Cue>) -> Result<Vec<Cue>, SubtitleError> {
    cues.retain(|c| !c.text.trim().is_empty());
    common::check_cue_count(cues.len())?;
    if cues.is_empty() {
        return Err(SubtitleError::NoCues);
    }
    cues.sort_by_key(|c| c.start_ms);
    Ok(cues)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn cue(start_ms: u32, end_ms: u32, text: &str) -> Cue {
        Cue {
            start_ms,
            end_ms,
            text: text.to_string(),
        }
    }

    // ----- SubtitleFormat -----

    #[test]
    fn from_codec_maps_the_known_names() {
        assert_eq!(SubtitleFormat::from_codec("srt"), Some(SubtitleFormat::Srt));
        assert_eq!(SubtitleFormat::from_codec("vtt"), Some(SubtitleFormat::Vtt));
        assert_eq!(
            SubtitleFormat::from_codec("webvtt"),
            Some(SubtitleFormat::Vtt)
        );
        assert_eq!(SubtitleFormat::from_codec("ass"), Some(SubtitleFormat::Ass));
        assert_eq!(SubtitleFormat::from_codec("ssa"), Some(SubtitleFormat::Ass));
    }

    #[test]
    fn from_codec_is_ascii_case_insensitive() {
        assert_eq!(SubtitleFormat::from_codec("SRT"), Some(SubtitleFormat::Srt));
        assert_eq!(
            SubtitleFormat::from_codec("WebVTT"),
            Some(SubtitleFormat::Vtt)
        );
        assert_eq!(SubtitleFormat::from_codec("Ssa"), Some(SubtitleFormat::Ass));
    }

    #[test]
    fn from_codec_rejects_everything_else() {
        for s in [
            "",
            " srt",
            "srt ",
            ".srt",
            "subrip",
            "mov_text",
            "text",
            "srt\0",
            "\u{ff53}rt",
        ] {
            assert_eq!(SubtitleFormat::from_codec(s), None, "{s:?}");
        }
    }

    #[test]
    fn codec_str_round_trips() {
        assert_eq!(SubtitleFormat::Srt.codec_str(), "srt");
        assert_eq!(SubtitleFormat::Vtt.codec_str(), "vtt");
        assert_eq!(SubtitleFormat::Ass.codec_str(), "ass");
        for f in [
            SubtitleFormat::Srt,
            SubtitleFormat::Vtt,
            SubtitleFormat::Ass,
        ] {
            assert_eq!(SubtitleFormat::from_codec(f.codec_str()), Some(f));
        }
    }

    // ----- finalize_cues -----

    #[test]
    fn finalize_sorts_stably_by_start() {
        let out = finalize_cues(vec![
            cue(5_000, 6_000, "late"),
            cue(1_000, 2_000, "first"),
            cue(1_000, 3_000, "second"),
            cue(0, 500, "zero"),
        ])
        .unwrap();
        let texts: Vec<&str> = out.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, ["zero", "first", "second", "late"]);
    }

    #[test]
    fn finalize_drops_empty_and_blank_text_cues() {
        let out =
            finalize_cues(vec![cue(0, 1, ""), cue(1, 2, " \n\t "), cue(2, 3, "keep")]).unwrap();
        assert_eq!(out, vec![cue(2, 3, "keep")]);
    }

    #[test]
    fn finalize_with_only_empty_cues_is_no_cues() {
        assert_eq!(
            finalize_cues(vec![cue(0, 1, ""), cue(1, 2, "  ")]),
            Err(SubtitleError::NoCues)
        );
        assert_eq!(finalize_cues(Vec::new()), Err(SubtitleError::NoCues));
    }

    #[test]
    fn finalize_accepts_exactly_max_cues_and_rejects_one_more() {
        let max = usize::try_from(MAX_CUES).unwrap();
        let mut cues: Vec<Cue> = (0..max).map(|_| cue(0, 1, "x")).collect();
        assert_eq!(finalize_cues(cues.clone()).unwrap().len(), max);
        cues.push(cue(0, 1, "x"));
        assert_eq!(
            finalize_cues(cues),
            Err(SubtitleError::TooManyCues { limit: MAX_CUES })
        );
    }

    #[test]
    fn finalize_does_not_count_dropped_empty_cues_against_the_cap() {
        let max = usize::try_from(MAX_CUES).unwrap();
        let mut cues: Vec<Cue> = (0..max).map(|_| cue(0, 1, "x")).collect();
        cues.push(cue(0, 1, ""));
        assert_eq!(finalize_cues(cues).unwrap().len(), max);
    }

    // ----- parse_subtitle gate (decode errors come before the parser) -----

    #[test]
    fn gate_rejects_oversized_input_for_every_format() {
        let big = vec![b'a'; usize::try_from(MAX_INPUT_BYTES).unwrap() + 1];
        for f in [
            SubtitleFormat::Srt,
            SubtitleFormat::Vtt,
            SubtitleFormat::Ass,
        ] {
            assert_eq!(
                parse_subtitle(f, &big),
                Err(SubtitleError::TooLarge {
                    limit: MAX_INPUT_BYTES
                })
            );
        }
    }

    #[test]
    fn gate_rejects_empty_and_bom_only_input_for_every_format() {
        for f in [
            SubtitleFormat::Srt,
            SubtitleFormat::Vtt,
            SubtitleFormat::Ass,
        ] {
            assert_eq!(parse_subtitle(f, b""), Err(SubtitleError::EmptyInput));
            assert_eq!(
                parse_subtitle(f, &[0xEF, 0xBB, 0xBF]),
                Err(SubtitleError::EmptyInput)
            );
            assert_eq!(
                parse_subtitle(f, &[0xFF, 0xFE]),
                Err(SubtitleError::EmptyInput)
            );
            assert_eq!(
                parse_subtitle(f, b" \r\n\t"),
                Err(SubtitleError::EmptyInput)
            );
        }
    }

    #[test]
    fn gate_rejects_bad_encodings_for_every_format() {
        for f in [
            SubtitleFormat::Srt,
            SubtitleFormat::Vtt,
            SubtitleFormat::Ass,
        ] {
            assert_eq!(
                parse_subtitle(f, &[b'a', 0xFF]),
                Err(SubtitleError::InvalidEncoding)
            );
            assert_eq!(
                parse_subtitle(f, b"a\0b"),
                Err(SubtitleError::InvalidEncoding)
            );
            assert_eq!(
                parse_subtitle(f, &[0xFF, 0xFE, 0x00, 0xD8]),
                Err(SubtitleError::InvalidEncoding)
            );
            assert_eq!(
                parse_subtitle(f, &[0xFE, 0xFF, 0x00]),
                Err(SubtitleError::InvalidEncoding)
            );
        }
    }

    const SRT_DOC: &[u8] = b"1\n00:00:01,000 --> 00:00:02,000\nsrt text\n";
    const VTT_DOC: &[u8] = b"WEBVTT\n\n00:01.000 --> 00:02.000\nvtt text\n";
    const ASS_DOC: &[u8] = b"[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\nDialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,ass text\n";

    #[test]
    fn wrappers_dispatch_to_the_matching_format() {
        assert_eq!(parse_srt(SRT_DOC), Ok(vec![cue(1_000, 2_000, "srt text")]));
        assert_eq!(parse_vtt(VTT_DOC), Ok(vec![cue(1_000, 2_000, "vtt text")]));
        assert_eq!(parse_ass(ASS_DOC), Ok(vec![cue(1_000, 2_000, "ass text")]));
        // A document of the wrong format is rejected: VTT and ASS
        // need their header, SRT sees a malformed timestamp.
        assert_eq!(parse_vtt(SRT_DOC), Err(SubtitleError::MissingHeader));
        assert_eq!(
            parse_srt(VTT_DOC),
            Err(SubtitleError::BadTimestamp { line: 3 })
        );
        assert_eq!(parse_ass(SRT_DOC), Err(SubtitleError::MissingHeader));
        // The wrappers agree with the gate on gate-level failures.
        for input in [&b""[..], &[0xFF][..], b"a\0"] {
            assert_eq!(parse_srt(input), parse_subtitle(SubtitleFormat::Srt, input));
            assert_eq!(parse_vtt(input), parse_subtitle(SubtitleFormat::Vtt, input));
            assert_eq!(parse_ass(input), parse_subtitle(SubtitleFormat::Ass, input));
        }
    }

    #[test]
    fn gate_parses_utf16_be_with_bom() {
        let text = "1\n00:00:01,000 --> 00:00:02,500\nh\u{e9}llo\n";
        let mut bytes = vec![0xFE, 0xFF];
        for u in text.encode_utf16() {
            bytes.extend_from_slice(&u.to_be_bytes());
        }
        assert_eq!(parse_srt(&bytes), Ok(vec![cue(1_000, 2_500, "h\u{e9}llo")]));
    }

    #[test]
    fn three_or_more_equal_start_cues_keep_input_order_through_a_real_parse() {
        let doc = "1\n00:00:05,000 --> 00:00:06,000\nlate\n\n\
                   2\n00:00:01,000 --> 00:00:02,000\nA\n\n\
                   3\n00:00:01,000 --> 00:00:03,000\nB\n\n\
                   4\n00:00:01,000 --> 00:00:04,000\nC\n\n\
                   5\n00:00:01,000 --> 00:00:05,000\nD\n";
        let out = parse_srt(doc.as_bytes()).unwrap();
        let texts: Vec<&str> = out.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, ["A", "B", "C", "D", "late"]);
    }

    #[test]
    fn reexported_limits_match_the_limits_module() {
        assert_eq!(MAX_CUES, limits::MAX_CUES);
        assert_eq!(MAX_INPUT_BYTES, limits::MAX_INPUT_BYTES);
        assert_eq!(MAX_TIMESTAMP_MS, limits::MAX_TIMESTAMP_MS);
    }
}
