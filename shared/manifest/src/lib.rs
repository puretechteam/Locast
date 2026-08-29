//! Shared manifest crate.
//!
//! P0-T01: placeholder. P3-T01 added the manifest data model and the
//! canonical-JSON serializer used by the host (which builds a manifest
//! and signs it) and the viewer (which verifies the signature).
//!
//! See `docs/ARCHITECTURE.md` section 8 for the canonicalization rules
//! and section 26.4 for the crate's place in the workspace layout.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

pub mod canonical;
pub mod error;
pub mod model;

pub use canonical::{commit, serialize};
pub use error::CanonicalError;
pub use model::{
    Codecs, Dimensions, HostSignature, MediaEntry, MediaManifest, Source, SubtitleEntry,
};

/// Library version string.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
