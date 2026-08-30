//! `room` - the host-side manifest publication path.
//!
//! P3-T03. See [`host`] for the actual implementation. The
//! module is currently only the host path; viewer-side
//! manifest handling lives in `net::room`.
//!
//! P3-T04 prerequisites:
//! - [`chunk_plan`] is the file-streaming chunk planner
//!   (256 KiB, per-chunk SHA-256, full-file BLAKE3).
//! - [`peer_id`] locks the canonical `peer_id` form as
//!   `sha256(public_key)` lowercase hex.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

pub mod chunk_plan;
pub mod host;
pub mod invite;
pub mod peer_id;
