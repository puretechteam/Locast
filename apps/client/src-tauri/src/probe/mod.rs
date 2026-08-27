//! Optional `ffprobe` / `ffmpeg` sidecar probe.
//!
//! Per `docs/ARCHITECTURE.md` section 7 (`storage`), ffmpeg is an
//! **optional** sidecar used **only for probing** an imported file. It is
//! not used for decode or transcode; the `<video>` element does the
//! playback. The probe fills in six optional columns on `media_items`:
//! `duration_ms`, `width`, `height`, `video_codec`, `audio_codec`, and
//! `container`.
//!
//! The probe binary is downloaded at first run by a future task; P1-T06
//! only wraps the probe call. When the binary is not on `PATH` (or
//! probing fails for any other reason - timeout, nonzero exit, malformed
//! JSON), [`ffprobe::run`] returns `None` and the import still succeeds
//! with the six fields left as `NULL`. The probe is best-effort metadata
//! enhancement, never a precondition for import.
//!
//! The probe is invoked between the atomic file completion and the
//! `INSERT` in `commands::import::import_one`; see that module for the
//! exact ordering.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

pub mod ffprobe;

pub use ffprobe::{run, run_with_timeout, ProbeError, ProbeResult, DEFAULT_TIMEOUT};
