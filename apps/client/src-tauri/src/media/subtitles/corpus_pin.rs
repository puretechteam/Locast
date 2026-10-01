//! Test-only: pins the committed fuzz corpus seeds for the
//! subtitle targets to their expected parse results (P8-T03,
//! architecture sections 17.3 and 21.8).
//!
//! The seeds live under
//! `apps/client/src-tauri/fuzz/corpus/subtitle_{srt,vtt,ass}/`.
//! Local fuzzing adds libFuzzer hash-named files to those
//! directories (git-ignored), so this module asserts that each
//! authored seed exists and decodes to its exact outcome rather
//! than asserting a directory listing.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use super::{parse_ass, parse_srt, parse_vtt, Cue, SubtitleError};

type Parser = fn(&[u8]) -> Result<Vec<Cue>, SubtitleError>;

const BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];

const SRT_SEEDS: [&str; 4] = [
    "valid_basic.srt",
    "multi_line_tags.srt",
    "mal_bom_negative_timestamp.srt",
    "mal_4gb_size.srt",
];
const VTT_SEEDS: [&str; 4] = [
    "valid_basic.vtt",
    "note_style_blocks.vtt",
    "mal_bom_negative_timestamp.vtt",
    "mal_4gb_size.vtt",
];
const ASS_SEEDS: [&str; 4] = [
    "valid_basic.ass",
    "dialogue_commas_overrides.ass",
    "mal_bom_negative_timestamp.ass",
    "mal_4gb_size.ass",
];

fn corpus_dir(target: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fuzz/corpus")
        .join(target)
}

fn seed(target: &str, name: &str) -> Vec<u8> {
    std::fs::read(corpus_dir(target).join(name))
        .unwrap_or_else(|e| panic!("committed seed {target}/{name} must exist: {e}"))
}

fn cue(start_ms: u32, end_ms: u32, text: &str) -> Cue {
    Cue {
        start_ms,
        end_ms,
        text: text.to_string(),
    }
}

fn parse_ok(parser: Parser, target: &str, name: &str) -> Vec<Cue> {
    parser(&seed(target, name)).unwrap_or_else(|e| panic!("{target}/{name} must parse: {e:?}"))
}

fn parse_err(parser: Parser, target: &str, name: &str) -> SubtitleError {
    match parser(&seed(target, name)) {
        Ok(cues) => panic!("{target}/{name} must be rejected, got {} cues", cues.len()),
        Err(e) => e,
    }
}

#[test]
fn seeds_are_lf_only_so_eol_normalization_cannot_alter_them() {
    // .gitattributes is `* text=auto eol=lf`: a CR byte in a text
    // seed would be silently normalized on commit and these tests
    // would then pin altered bytes. CRLF / CR coverage lives in
    // the inline unit tests instead.
    for (target, names) in [
        ("subtitle_srt", SRT_SEEDS),
        ("subtitle_vtt", VTT_SEEDS),
        ("subtitle_ass", ASS_SEEDS),
    ] {
        for name in names {
            let bytes = seed(target, name);
            assert!(!bytes.is_empty(), "{target}/{name} is empty");
            assert!(!bytes.contains(&0x0d), "{target}/{name} contains a CR byte");
        }
    }
}

#[test]
fn bom_seeds_start_with_the_utf8_bom() {
    for (target, name) in [
        ("subtitle_srt", "mal_bom_negative_timestamp.srt"),
        ("subtitle_vtt", "mal_bom_negative_timestamp.vtt"),
        ("subtitle_ass", "mal_bom_negative_timestamp.ass"),
    ] {
        let bytes = seed(target, name);
        assert_eq!(
            bytes.get(..3),
            Some(&BOM[..]),
            "{target}/{name} lost its BOM"
        );
    }
}

#[test]
fn srt_valid_basic_seed_parses_to_two_cues() {
    let cues = parse_ok(parse_srt, "subtitle_srt", "valid_basic.srt");
    assert_eq!(
        cues,
        vec![
            cue(1000, 3500, "Hello, world."),
            cue(4000, 6000, "Second cue."),
        ]
    );
}

#[test]
fn srt_multi_line_tags_seed_parses_to_two_cues_and_skips_the_arrowless_block() {
    let cues = parse_ok(parse_srt, "subtitle_srt", "multi_line_tags.srt");
    assert_eq!(
        cues,
        vec![
            cue(
                1000,
                4000,
                "Italic and top
bold nested unclosed <"
            ),
            cue(
                2500,
                3250,
                "Line one
Line two <<<<"
            ),
        ]
    );
}

#[test]
fn srt_malicious_seeds_are_rejected_with_bad_timestamp() {
    assert_eq!(
        parse_err(parse_srt, "subtitle_srt", "mal_bom_negative_timestamp.srt"),
        SubtitleError::BadTimestamp { line: 2 }
    );
    assert_eq!(
        parse_err(parse_srt, "subtitle_srt", "mal_4gb_size.srt"),
        SubtitleError::BadTimestamp { line: 6 }
    );
}

#[test]
fn vtt_valid_basic_seed_parses_to_two_cues() {
    let cues = parse_ok(parse_vtt, "subtitle_vtt", "valid_basic.vtt");
    assert_eq!(
        cues,
        vec![
            cue(1000, 3500, "Hello, world."),
            cue(4000, 6000, "Second & last."),
        ]
    );
}

#[test]
fn vtt_note_style_blocks_seed_parses_to_one_cue() {
    let cues = parse_ok(parse_vtt, "subtitle_vtt", "note_style_blocks.vtt");
    assert_eq!(cues, vec![cue(1000, 2000, "Hi <there> ok")]);
}

#[test]
fn vtt_malicious_seeds_are_rejected_with_bad_timestamp() {
    assert_eq!(
        parse_err(parse_vtt, "subtitle_vtt", "mal_bom_negative_timestamp.vtt"),
        SubtitleError::BadTimestamp { line: 3 }
    );
    assert_eq!(
        parse_err(parse_vtt, "subtitle_vtt", "mal_4gb_size.vtt"),
        SubtitleError::BadTimestamp { line: 7 }
    );
}

#[test]
fn ass_valid_basic_seed_parses_to_two_cues() {
    let cues = parse_ok(parse_ass, "subtitle_ass", "valid_basic.ass");
    assert_eq!(
        cues,
        vec![
            cue(1000, 3500, "Hello, world."),
            cue(4000, 6000, "Second\ncue"),
        ]
    );
}

#[test]
fn ass_dialogue_commas_overrides_seed_parses_to_two_cues() {
    // The \p1 drawing cue is dropped, the short Dialogue and the
    // Comment line are skipped, the unclosed override is removed.
    let cues = parse_ok(parse_ass, "subtitle_ass", "dialogue_commas_overrides.ass");
    assert_eq!(
        cues,
        vec![
            cue(1000, 2000, "a, b, c "),
            cue(7000, 8000, "line\nbreak space"),
        ]
    );
}

#[test]
fn ass_malicious_seeds_are_rejected_with_bad_timestamp() {
    assert_eq!(
        parse_err(parse_ass, "subtitle_ass", "mal_bom_negative_timestamp.ass"),
        SubtitleError::BadTimestamp { line: 6 }
    );
    assert_eq!(
        parse_err(parse_ass, "subtitle_ass", "mal_4gb_size.ass"),
        SubtitleError::BadTimestamp { line: 7 }
    );
}
