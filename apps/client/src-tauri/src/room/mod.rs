//! `room` - the host-side manifest publication path.
//!
//! P3-T03. See [`host`] for the actual implementation. The
//! module is currently only the host path; viewer-side
//! manifest handling lives in `net::room`.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

pub mod host;
