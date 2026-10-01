//! `media` - pure media-side helpers that run on untrusted file
//! content (P8-T03).
//!
//! Currently this holds only the subtitle parsers
//! ([`subtitles`]). Everything in here is a pure function over
//! bytes: no I/O, no clock, no global state. Parsers are expected
//! to be driven by the cargo-fuzz targets under
//! `apps/client/src-tauri/fuzz/` (architecture sections 17.3 and
//! 21.8).

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

pub mod subtitles;
