//! Shared crypto crate.
//!
//! P0-T01: placeholder. P1-T03 added the BLAKE3 streaming hasher used for
//! full-file integrity. P3+ adds Ed25519 signing/verification
//! (`ed25519-dalek`) and the canonical-JSON signer for the manifest.
//! Per-chunk SHA-256 hashing lives in the client (`apps/client/src-tauri/
//! src/core/hashing.rs`) because the downloader is the only v1 consumer;
//! the server side will have its own hashing surface in P3. See
//! `docs/ARCHITECTURE.md` sections 6 (hashing strategy), 8 (manifest
//! signature), and 26.4 (shared layout).

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

pub mod blake3;

/// Library version string.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
