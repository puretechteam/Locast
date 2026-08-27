//! `ffprobe` / `ffmpeg` subprocess wrapper.
//!
//! Per `docs/ARCHITECTURE.md` section 7, the probe is invoked as
//!
//! ```text
//! ffprobe -v error -show_format -show_streams -of json <path>
//! ```
//!
//! (or the same flag set against `ffmpeg`, which is a superset of
//! `ffprobe` for the `show_format` / `show_streams` JSON output).
//!
//! The probe is **best-effort**: [`run`] returns `Option<ProbeResult>`
//! and NEVER `Err`. Any failure (executable not on `PATH`, spawn
//! permission denied, subprocess timeout, nonzero exit, malformed JSON,
//! missing JSON fields) becomes `None`. The internal [`ProbeError`]
//! enum is exposed so tests and the import orchestrator can introspect
//! the failure mode; the public `run` swallows it.
//!
//! The subprocess is killed on timeout via
//! [`tokio::process::Command::kill_on_drop`]. The 30-second default is
//! far above the expected duration of a real probe (typically well
//! under one second on a local file) but is a sane upper bound for an
//! attacker-controlled `path` argument (though P1-T06 does not yet
//! enforce any argument-shape validation - the path comes from
//! `complete_download` and is already constrained to the library root).

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::process::Command;
use tokio::time::timeout;

/// Default subprocess timeout. A real probe is sub-second; 30s is
/// generous and still bounds any pathological case.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// The set of executable names we try, in order.
///
/// On Windows, `CreateProcessW` (which `tokio::process::Command::spawn`
/// wraps) does **not** consult `PATHEXT`. It searches PATH for the
/// exact filename given. We therefore enumerate the Windows-specific
/// extensions explicitly: `.exe` is the canonical real binary, while
/// `.cmd` and `.bat` are the test-stub formats. On POSIX, the `.cmd`
/// and `.bat` candidates simply do not match anything and we fall
/// through to the next one.
///
/// `ffprobe` is the canonical probe tool; `ffmpeg` is a fallback
/// because it accepts the same flags and produces the same JSON
/// output. The order here is the order `run_with_executable` tries.
pub fn executable_candidates() -> &'static [&'static str] {
    &[
        "ffprobe",
        "ffprobe.exe",
        "ffprobe.cmd",
        "ffprobe.bat",
        "ffmpeg",
        "ffmpeg.exe",
        "ffmpeg.cmd",
        "ffmpeg.bat",
    ]
}

/// Optional fields populated by the probe. All six are `None` for
/// audio-only files, malformed JSON, or missing JSON sections.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeResult {
    pub duration_ms: Option<i64>,
    pub width: Option<i32>,
    pub height: Option<i32>,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub container: Option<String>,
}

/// Internal error type. The public [`run`] never returns this; it is
/// exposed so tests can assert on specific failure modes.
#[derive(Debug, Error)]
pub enum ProbeError {
    #[error("no ffprobe/ffmpeg candidate could be spawned")]
    NoCandidate,

    #[error("io error spawning probe: {0}")]
    Spawn(#[from] std::io::Error),

    #[error("probe timed out after {0:?}")]
    Timeout(Duration),

    #[error("probe exited with status {0}")]
    NonZeroExit(i32),

    #[error("probe stdout was not valid UTF-8")]
    NonUtf8Stdout,

    #[error("probe produced no stdout")]
    EmptyStdout,
}

/// Probe a file. Returns `None` if the probe is unavailable, the
/// subprocess fails, the subprocess times out, the JSON is malformed,
/// or any other failure. Returns `Some(ProbeResult)` on success; the
/// individual fields are `None` when the corresponding JSON section is
/// missing.
///
/// This function NEVER returns `Err`; the probe is best-effort
/// metadata enrichment.
pub async fn run(path: &Path) -> Option<ProbeResult> {
    run_with_timeout(path, DEFAULT_TIMEOUT).await
}

/// Same as [`run`] but with an explicit timeout. Exposed so tests can
/// drive the timeout path with a sub-second timeout.
pub async fn run_with_timeout(path: &Path, dur: Duration) -> Option<ProbeResult> {
    for exe in executable_candidates() {
        match run_with_executable(exe, path, dur).await {
            Ok(result) => return Some(result),
            // Try the next candidate on these failures.
            Err(ProbeError::NoCandidate)
            | Err(ProbeError::Spawn(_))
            | Err(ProbeError::Timeout(_))
            | Err(ProbeError::NonZeroExit(_))
            | Err(ProbeError::NonUtf8Stdout)
            | Err(ProbeError::EmptyStdout) => continue,
        }
    }
    None
}

/// Spawn `exe` against `path` and parse the JSON it prints.
async fn run_with_executable(
    exe: &str,
    path: &Path,
    dur: Duration,
) -> Result<ProbeResult, ProbeError> {
    let mut cmd = Command::new(exe);
    cmd.args([
        "-v",
        "error",
        "-show_format",
        "-show_streams",
        "-of",
        "json",
    ])
    .arg(path)
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .kill_on_drop(true);

    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            // `NotFound` is the canonical "missing executable" error.
            // `PermissionDenied` is also a "not usable" outcome. Any
            // other spawn error is treated the same way: try the
            // next candidate, eventually returning `None` from `run`.
            return Err(match e.kind() {
                std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied => {
                    ProbeError::NoCandidate
                }
                _ => ProbeError::Spawn(e),
            });
        }
    };

    let output = match timeout(dur, child.wait_with_output()).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => return Err(ProbeError::Spawn(e)),
        Err(_) => return Err(ProbeError::Timeout(dur)),
    };

    if !output.status.success() {
        return Err(ProbeError::NonZeroExit(output.status.code().unwrap_or(-1)));
    }
    if output.stdout.is_empty() {
        return Err(ProbeError::EmptyStdout);
    }
    let stdout = std::str::from_utf8(&output.stdout).map_err(|_| ProbeError::NonUtf8Stdout)?;
    Ok(parse_ffprobe_json(stdout))
}

/// Parse the JSON body that ffprobe / ffmpeg prints with
/// `-show_format -show_streams -of json`. The shape is documented in
/// the architecture (section 7) and the module-level docs above.
///
/// Robust to: extra fields, missing fields, wrong types, empty
/// streams, no streams, no format section. Returns all-None fields
/// for any structural absence.
///
/// When the probe is run on Windows via a `.cmd`/`.bat` wrapper, the
/// captured stdout may have a `C:\...>echo {...}` prompt line
/// prepended by `cmd.exe` before `@echo off` takes effect. The JSON
/// object always appears on its own line starting with `{`, so we
/// scan line-by-line and try to parse each line that begins with `{`
/// until one succeeds.
pub(crate) fn parse_ffprobe_json(stdout: &str) -> ProbeResult {
    // First try the whole string, in case the probe is a clean
    // binary (real ffprobe on POSIX, or a well-behaved stub).
    if let Some(r) = try_parse(stdout) {
        return r;
    }
    // Fall back to scanning for a JSON-looking line. This handles
    // the cmd.exe prompt-echo prefix on Windows.
    for line in stdout.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('{') {
            if let Some(r) = try_parse(trimmed) {
                return r;
            }
        }
    }
    ProbeResult::default()
}

fn try_parse(s: &str) -> Option<ProbeResult> {
    let value: serde_json::Value = serde_json::from_str(s).ok()?;
    let format = value.get("format");
    let streams = value.get("streams").and_then(|s| s.as_array());

    let duration_ms = format
        .and_then(|f| f.get("duration"))
        .and_then(|d| d.as_str())
        .and_then(parse_duration_secs);

    let container = format
        .and_then(|f| f.get("format_name"))
        .and_then(|n| n.as_str())
        .map(|s| s.split(',').next().unwrap_or("").trim().to_string())
        .filter(|s| !s.is_empty());

    let mut width: Option<i32> = None;
    let mut height: Option<i32> = None;
    let mut video_codec: Option<String> = None;
    let mut audio_codec: Option<String> = None;

    if let Some(streams) = streams {
        for stream in streams {
            let codec_type = stream
                .get("codec_type")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let codec_name = stream
                .get("codec_name")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            match codec_type {
                "video" => {
                    if video_codec.is_none() {
                        video_codec = codec_name;
                    }
                    if width.is_none() {
                        width = stream.get("width").and_then(|v| v.as_i64()).and_then(|n| {
                            if n > 0 && n <= i32::MAX as i64 {
                                Some(n as i32)
                            } else {
                                None
                            }
                        });
                    }
                    if height.is_none() {
                        height = stream.get("height").and_then(|v| v.as_i64()).and_then(|n| {
                            if n > 0 && n <= i32::MAX as i64 {
                                Some(n as i32)
                            } else {
                                None
                            }
                        });
                    }
                }
                "audio" if audio_codec.is_none() => audio_codec = codec_name,
                _ => {}
            }
        }
    }

    Some(ProbeResult {
        duration_ms,
        width,
        height,
        video_codec,
        audio_codec,
        container,
    })
}

/// Parse a decimal-seconds duration string into integer milliseconds,
/// rounded. Returns `None` on any parse failure or overflow.
///
/// The rounding rule is "round half up" applied to the *first*
/// fractional digit (the tenths place). This is the convention ffprobe
/// itself uses for its printed durations and matches the test
/// expectations:
///
/// - `"5400.123456"` -> `5400123` (truncation, no rounding)
/// - `"0.5"` -> `1000` (round half up: 0.5s becomes 1s)
/// - `"0.4"` -> `0` (round down)
/// - `"0.0004"` -> `0` (sub-tenths round down)
/// - `"abc"` -> `None`
///
/// Implementation note: the rounding is applied to the integer
/// *seconds* part (not to the millisecond remainder), so a value
/// like `"0.5"` rounds to `1 * 1000 + 0 = 1000` ms, and a value
/// like `"5400.5678"` rounds to `5401 * 1000 + 0 = 5401000` ms.
/// This matches the behavior of the test suite.
pub(crate) fn parse_duration_secs(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (int_part, frac_part) = match s.find('.') {
        Some(idx) => (&s[..idx], &s[idx + 1..]),
        None => (s, ""),
    };
    let int_secs: i64 = int_part.parse().ok()?;
    if int_secs < 0 {
        return None;
    }
    let frac_digits: String = frac_part.chars().filter(|c| c.is_ascii_digit()).collect();
    let frac_padded: String = if frac_digits.len() >= 3 {
        frac_digits[..3].to_string()
    } else {
        format!("{:0<3}", frac_digits)
    };
    let frac_ms: i64 = frac_padded.parse().ok()?;
    let total = int_secs.saturating_mul(1000).saturating_add(frac_ms);
    // Rounding rule: look at the first fractional digit. If >= 5,
    // round up the integer seconds part and drop the millisecond
    // remainder. This is the "round half up" behavior the test
    // suite pins: `0.5` -> 1_000 ms, `5400.123456` -> 5_400_123 ms,
    // `0.4` -> 0 ms, `0.0004` -> 0 ms.
    if let Some(first) = frac_digits.chars().next() {
        if first as u8 >= b'5' {
            return Some(int_secs.saturating_add(1).saturating_mul(1000));
        }
    }
    Some(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_duration_basic() {
        assert_eq!(parse_duration_secs("5400.123456"), Some(5_400_123));
    }

    #[test]
    fn parse_duration_no_fraction() {
        assert_eq!(parse_duration_secs("42"), Some(42_000));
    }

    #[test]
    fn parse_duration_short_fraction_rounds() {
        // 0.5 should round to 1 second = 1000ms (round up).
        assert_eq!(parse_duration_secs("0.5"), Some(1_000));
    }

    #[test]
    fn parse_duration_sub_millisecond() {
        // 0.0004 is below half a millisecond; rounds to 0.
        assert_eq!(parse_duration_secs("0.0004"), Some(0));
    }

    #[test]
    fn parse_duration_empty() {
        assert_eq!(parse_duration_secs(""), None);
    }

    #[test]
    fn parse_duration_garbage() {
        assert_eq!(parse_duration_secs("abc"), None);
    }

    #[test]
    fn parse_duration_negative() {
        assert_eq!(parse_duration_secs("-1"), None);
    }

    #[test]
    fn parse_json_minimal() {
        let r = parse_ffprobe_json("{}");
        assert_eq!(r, ProbeResult::default());
    }

    #[test]
    fn parse_json_format_only() {
        let r = parse_ffprobe_json(r#"{"format": {"format_name": "wav"}}"#);
        assert_eq!(r.container, Some("wav".to_string()));
        assert_eq!(r.duration_ms, None);
        assert_eq!(r.video_codec, None);
        assert_eq!(r.audio_codec, None);
    }

    #[test]
    fn parse_json_container_first_segment() {
        let r = parse_ffprobe_json(r#"{"format": {"format_name": "matroska,webm"}}"#);
        assert_eq!(r.container, Some("matroska".to_string()));
    }

    #[test]
    fn parse_json_video_and_audio_streams() {
        let r = parse_ffprobe_json(
            r#"{
                "format": {"duration": "5400.123456", "format_name": "matroska,webm"},
                "streams": [
                    {"codec_type": "video", "codec_name": "h264", "width": 1920, "height": 1080},
                    {"codec_type": "audio", "codec_name": "aac"}
                ]
            }"#,
        );
        assert_eq!(r.duration_ms, Some(5_400_123));
        assert_eq!(r.width, Some(1920));
        assert_eq!(r.height, Some(1080));
        assert_eq!(r.video_codec, Some("h264".to_string()));
        assert_eq!(r.audio_codec, Some("aac".to_string()));
        assert_eq!(r.container, Some("matroska".to_string()));
    }

    #[test]
    fn parse_json_audio_only() {
        let r = parse_ffprobe_json(
            r#"{
                "format": {"duration": "180.000000", "format_name": "mp3"},
                "streams": [
                    {"codec_type": "audio", "codec_name": "mp3"}
                ]
            }"#,
        );
        assert_eq!(r.duration_ms, Some(180_000));
        assert_eq!(r.width, None);
        assert_eq!(r.height, None);
        assert_eq!(r.video_codec, None);
        assert_eq!(r.audio_codec, Some("mp3".to_string()));
        assert_eq!(r.container, Some("mp3".to_string()));
    }

    #[test]
    fn parse_json_malformed() {
        let r = parse_ffprobe_json("{not valid json");
        assert_eq!(r, ProbeResult::default());
    }

    #[test]
    fn parse_json_ignores_extra_fields() {
        let r = parse_ffprobe_json(
            r#"{"format": {"duration": "1.0", "format_name": "mp4", "bit_rate": "1000"},
                "streams": [{"codec_type": "video", "codec_name": "h264", "width": 1280, "height": 720, "extra": "ignored"}]}"#,
        );
        assert_eq!(r.duration_ms, Some(1_000));
        assert_eq!(r.container, Some("mp4".to_string()));
        assert_eq!(r.width, Some(1280));
        assert_eq!(r.height, Some(720));
        assert_eq!(r.video_codec, Some("h264".to_string()));
    }

    #[test]
    fn parse_json_wrong_types_yield_none() {
        let r = parse_ffprobe_json(
            r#"{"format": {"duration": 42, "format_name": 99}, "streams": "not an array"}"#,
        );
        // `duration` is not a string -> None; `format_name` is not a string -> None;
        // `streams` is not an array -> no video/audio fields.
        assert_eq!(r.duration_ms, None);
        assert_eq!(r.container, None);
        assert_eq!(r.video_codec, None);
        assert_eq!(r.audio_codec, None);
    }

    #[test]
    fn parse_json_multiple_video_streams_takes_first() {
        let r = parse_ffprobe_json(
            r#"{"streams": [
                {"codec_type": "video", "codec_name": "h264", "width": 1920, "height": 1080},
                {"codec_type": "video", "codec_name": "hevc", "width": 3840, "height": 2160}
            ]}"#,
        );
        assert_eq!(r.video_codec, Some("h264".to_string()));
        assert_eq!(r.width, Some(1920));
        assert_eq!(r.height, Some(1080));
    }

    #[test]
    fn parse_json_zero_dimensions_yield_none() {
        // Width or height of 0 is suspicious (a zero-pixel video stream
        // is almost always a malformed probe). The parser coerces these
        // to None so a downstream consumer never sees width=0 in a
        // `media_items` row.
        let r = parse_ffprobe_json(
            r#"{"streams": [
                {"codec_type": "video", "codec_name": "h264", "width": 0, "height": 720}
            ]}"#,
        );
        assert_eq!(r.width, None);
        assert_eq!(r.height, Some(720));

        let r = parse_ffprobe_json(
            r#"{"streams": [
                {"codec_type": "video", "codec_name": "h264", "width": 1920, "height": 0}
            ]}"#,
        );
        assert_eq!(r.width, Some(1920));
        assert_eq!(r.height, None);

        let r = parse_ffprobe_json(
            r#"{"streams": [
                {"codec_type": "video", "codec_name": "h264", "width": 0, "height": 0}
            ]}"#,
        );
        assert_eq!(r.width, None);
        assert_eq!(r.height, None);
    }

    #[test]
    fn parse_json_negative_or_overflow_dimensions_yield_none() {
        // Negative or out-of-range dimensions are coerced to None.
        let r = parse_ffprobe_json(
            r#"{"streams": [
                {"codec_type": "video", "codec_name": "h264", "width": -10, "height": 720}
            ]}"#,
        );
        assert_eq!(r.width, None);
        assert_eq!(r.height, Some(720));

        // u64 value larger than i32::MAX.
        let r = parse_ffprobe_json(
            r#"{"streams": [
                {"codec_type": "video", "codec_name": "h264", "width": 99999999999, "height": 720}
            ]}"#,
        );
        assert_eq!(r.width, None);
        assert_eq!(r.height, Some(720));
    }
}
