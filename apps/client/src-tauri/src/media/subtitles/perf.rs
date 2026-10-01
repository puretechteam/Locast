//! Test-only: parse-time and linear-scaling checks for the
//! subtitle parsers (P8-T03, architecture sections 17.3 and 21.8).
//!
//! The 100 ms budget tests are `#[ignore]` because debug builds are
//! far slower than release; run them with `cargo test --release -p
//! locast-client --lib media::subtitles::perf -- --ignored
//! --nocapture`. The linear-scaling test is not ignored and uses a
//! generous ratio so it stays stable on loaded CI machines.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use std::fmt::Write as _;
use std::time::{Duration, Instant};

use super::{parse_ass, parse_srt, Cue, SubtitleError, MAX_CUES, MAX_INPUT_BYTES};

type Parser = fn(&[u8]) -> Result<Vec<Cue>, SubtitleError>;

/// Build an SRT of at least `target_bytes` made of ~160 byte cues
/// (a markup tag on the first text line so the tag stripper works).
fn build_srt(target_bytes: usize) -> Vec<u8> {
    let mut out = String::with_capacity(target_bytes + 512);
    let mut n: u32 = 0;
    while out.len() < target_bytes {
        n += 1;
        let ms = u64::from(n) * 2000;
        let (h, m, s) = (ms / 3_600_000, (ms / 60_000) % 60, (ms / 1000) % 60);
        let end = ms + 1500;
        let (eh, em, es) = (end / 3_600_000, (end / 60_000) % 60, (end / 1000) % 60);
        write!(
            out,
            "{n}\n{h:02}:{m:02}:{s:02},000 --> {eh:02}:{em:02}:{es:02},500\n\
             <i>The quick brown fox jumps over the lazy dog, again.</i>\n\
             second line of cue number {n} with filler\n\n"
        )
        .unwrap();
    }
    out.into_bytes()
}

/// Build an ASS of at least `target_bytes` made of Dialogue lines.
fn build_ass(target_bytes: usize) -> Vec<u8> {
    let mut out = String::with_capacity(target_bytes + 512);
    out.push_str("[Script Info]\nTitle: perf\n\n[Events]\n");
    out.push_str(
        "Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n",
    );
    let mut n: u32 = 0;
    while out.len() < target_bytes {
        n += 1;
        let cs = u64::from(n) * 200;
        let (h, m, s, c) = (cs / 360_000, (cs / 6000) % 60, (cs / 100) % 60, cs % 100);
        let end = cs + 150;
        let (eh, em, es, ec) = (
            end / 360_000,
            (end / 6000) % 60,
            (end / 100) % 60,
            end % 100,
        );
        writeln!(
            out,
            "Dialogue: 0,{h}:{m:02}:{s:02}.{c:02},{eh}:{em:02}:{es:02}.{ec:02},Default,,0,0,0,,\
             {{\\i1}}Cue number {n}, with commas{{\\i0}} and\\Na second line of text"
        )
        .unwrap();
    }
    out.into_bytes()
}

/// Minimum wall time over `runs` parses of `data`, asserting every
/// parse succeeds. Returns the minimum and the cue count.
fn min_parse_time(parser: Parser, data: &[u8], runs: usize) -> (Duration, usize) {
    let mut best = Duration::MAX;
    let mut cues = 0;
    for _ in 0..runs {
        let start = Instant::now();
        let parsed = parser(data).expect("perf input must parse");
        let elapsed = start.elapsed();
        cues = parsed.len();
        std::hint::black_box(&parsed);
        best = best.min(elapsed);
    }
    (best, cues)
}

fn assert_in_caps(data: &[u8], cues: usize) {
    let input_cap = usize::try_from(MAX_INPUT_BYTES).unwrap();
    let cue_cap = usize::try_from(MAX_CUES).unwrap();
    assert!(data.len() < input_cap, "perf input exceeds the input cap");
    assert!(
        cues > 1000 && cues < cue_cap,
        "perf input must exercise the success path inside the cue cap, got {cues}"
    );
}

#[test]
#[ignore = "perf: run in release"]
fn ten_mb_srt_parses_in_under_100_ms() {
    let data = build_srt(10 * 1024 * 1024);
    let (best, cues) = min_parse_time(parse_srt, &data, 5);
    println!(
        "srt perf: {} bytes, {cues} cues, min-of-5 = {best:?}",
        data.len()
    );
    assert_in_caps(&data, cues);
    assert!(best < Duration::from_millis(100), "SRT took {best:?}");
}

#[test]
#[ignore = "perf: run in release"]
fn five_mb_ass_parses_in_under_100_ms() {
    let data = build_ass(5 * 1024 * 1024);
    let (best, cues) = min_parse_time(parse_ass, &data, 5);
    println!(
        "ass perf: {} bytes, {cues} cues, min-of-5 = {best:?}",
        data.len()
    );
    assert_in_caps(&data, cues);
    assert!(best < Duration::from_millis(100), "ASS took {best:?}");
}

#[test]
fn srt_parse_time_scales_linearly_with_input_size() {
    let small = build_srt(1024 * 1024);
    let large = build_srt(4 * 1024 * 1024);
    let (t_small, c_small) = min_parse_time(parse_srt, &small, 5);
    let (t_large, c_large) = min_parse_time(parse_srt, &large, 5);
    assert!(c_large > c_small);
    // 4x the input: linear is ~4x, quadratic would be ~16x. The
    // timings are min-of-5 and the limit is 12 so a noisy CI runner
    // does not fail a linear parser, while a quadratic one (~16x or
    // worse) still does. A 2 ms floor on the small time absorbs
    // timer noise on fast machines.
    let denom = t_small.max(Duration::from_millis(2));
    let ratio = t_large.as_secs_f64() / denom.as_secs_f64();
    println!("srt scaling: 1 MB = {t_small:?}, 4 MB = {t_large:?}, ratio = {ratio:.2}");
    assert!(ratio < 12.0, "non-linear scaling: ratio {ratio:.2}");
}
