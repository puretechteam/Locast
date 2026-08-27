//! Shared manifest crate.
//!
//! P0-T01: placeholder. P3+ adds the manifest data model, the canonical-JSON
//! signer (host side), and the signature verifier (viewer side). Depends on
//! `locast-protocol` for the shared wire types. See
//! `docs/ARCHITECTURE.md` section 8 and section 26.4.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

/// Library version string.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
