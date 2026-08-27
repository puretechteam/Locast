//! `core` - pure-Rust domain logic that has no Tauri, filesystem, or network
//! dependency.
//!
//! Modules under `core` must compile and test on their own (see architecture
//! section 26.2.x). P1-T01 adds `core::library::sanitize`; P1-T02 adds
//! `core::paths`; P1-T03 adds `core::hashing` (BLAKE3 re-export + per-chunk
//! SHA-256 + the canonical 256 KiB `CHUNK_SIZE`); later phase-1 tasks add
//! `core::quota`.

pub mod hashing;
pub mod library;
pub mod paths;
pub mod quota;
