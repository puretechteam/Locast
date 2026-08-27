//! Streaming hashing helpers used by the client downloader.
//!
//! This module is pure-Rust. It has **no** filesystem, network, clock, or
//! environment access. The only thing it does is turn `&[u8]` into
//! hex-encoded digests. That property keeps the unit tests fast and
//! deterministic, and matches the architecture's "core has no filesystem
//! dependency" rule (section 26.2.x).
//!
//! Two algorithms are exposed:
//!
//! - **BLAKE3** for full-file integrity. Re-exported from `locast-crypto`
//!   so the client shares one implementation with any other consumer of
//!   `shared/crypto`.
//! - **SHA-256** for per-chunk integrity. The architecture (section 6)
//!   mandates SHA-256 here because the manifest spec needs a
//!   content-address key that external tools can verify independently
//!   of Locast.
//!
//! The canonical chunk size is 256 KiB (architecture sections 9 and 26.6).
//! `chunked_sha256_hex` returns one SHA-256 digest per chunk, in order.

#![forbid(unsafe_code)]

use digest::Digest;
use sha2::Sha256;

pub use locast_crypto::blake3::{blake3_hex, Blake3Hasher};

/// Canonical download chunk size, in bytes. Architecture sections 9 and
/// 26.6. Used by the downloader to slice incoming byte streams for
/// per-chunk SHA-256 verification and progress reporting.
pub const CHUNK_SIZE: usize = 262144; // 256 KiB

/// One-shot SHA-256 of a byte slice. Returns 64 lowercase hex chars.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

/// Streaming SHA-256 hasher. Wraps `sha2::Sha256` so callers can feed
/// bytes incrementally (e.g. a 1 MiB read at a time from a download
/// stream) without ever materializing the whole file.
pub struct Sha256Hasher {
    inner: Sha256,
}

impl Sha256Hasher {
    pub fn new() -> Self {
        Self {
            inner: Sha256::new(),
        }
    }

    /// Absorb bytes. May be called any number of times. An empty slice
    /// is a no-op.
    pub fn update(&mut self, data: &[u8]) {
        self.inner.update(data);
    }

    /// Finalize and return the 32-byte digest as 64 lowercase hex chars.
    pub fn finalize_hex(self) -> String {
        hex::encode(self.inner.finalize())
    }
}

impl Default for Sha256Hasher {
    fn default() -> Self {
        Self::new()
    }
}

/// Split `bytes` into fixed-size chunks and return the SHA-256 hex digest
/// of each chunk, in order. The final chunk may be shorter than
/// `CHUNK_SIZE`. An empty input returns an empty `Vec`.
pub fn chunked_sha256_hex(bytes: &[u8]) -> Vec<String> {
    bytes.chunks(CHUNK_SIZE).map(sha256_hex).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Known SHA-256 test vectors.
    const EMPTY_DIGEST: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    const ABC_DIGEST: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    // ----- sha256_hex -----

    #[test]
    fn sha256_empty_input_known_digest() {
        assert_eq!(sha256_hex(b""), EMPTY_DIGEST);
    }

    #[test]
    fn sha256_abc_known_digest() {
        assert_eq!(sha256_hex(b"abc"), ABC_DIGEST);
    }

    #[test]
    fn sha256_tiny_input() {
        // "hello world" -> locked value so any regression in the
        // binding surfaces here.
        let expected = "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9";
        assert_eq!(sha256_hex(b"hello world"), expected);
    }

    #[test]
    fn sha256_one_byte_input() {
        // SHA-256 of a single 0x00 byte.
        let expected = "6e340b9cffb37a989ca544e6bb780a2c78901d3fb33738768511a30617afa01d";
        assert_eq!(sha256_hex(&[0u8]), expected);
    }

    #[test]
    fn sha256_hex_is_64_lowercase_chars() {
        for input in [b"".as_slice(), b"a", b"abc", &vec![0u8; 1024]] {
            let hex = sha256_hex(input);
            assert_eq!(
                hex.len(),
                64,
                "wrong length for input of {} bytes",
                input.len()
            );
            assert!(
                hex.chars()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
                "expected lowercase hex, got {hex}"
            );
        }
    }

    #[test]
    fn sha256_one_shot_equals_streaming_for_multichunk() {
        let data: Vec<u8> = (0u32..50_000).map(|i| (i & 0xFF) as u8).collect();
        let one_shot = sha256_hex(&data);

        let mut h = Sha256Hasher::new();
        for chunk in data.chunks(8192) {
            h.update(chunk);
        }
        let streamed = h.finalize_hex();

        assert_eq!(one_shot, streamed);
    }

    #[test]
    fn sha256_byte_by_byte_matches_single_write() {
        let data = b"The quick brown fox jumps over the lazy dog";
        let all_at_once = sha256_hex(data);

        let mut h = Sha256Hasher::new();
        for b in data {
            h.update(&[*b]);
        }
        assert_eq!(h.finalize_hex(), all_at_once);
    }

    #[test]
    fn sha256_chunk_boundary_256kib_plus_one() {
        const SZ: usize = (256 * 1024) + 1;
        let buf = vec![0xA5u8; SZ];

        let all_at_once = sha256_hex(&buf);

        let mut h = Sha256Hasher::new();
        h.update(&buf[..256 * 1024]);
        h.update(&buf[256 * 1024..]);
        let split_at = h.finalize_hex();

        let mut h = Sha256Hasher::new();
        for chunk in buf.chunks(8192) {
            h.update(chunk);
        }
        let many_small = h.finalize_hex();

        assert_eq!(all_at_once, split_at);
        assert_eq!(all_at_once, many_small);
    }

    // ----- chunked_sha256_hex -----

    #[test]
    fn chunked_empty_input_is_empty_vec() {
        let got = chunked_sha256_hex(b"");
        assert!(got.is_empty());
    }

    #[test]
    fn chunked_input_smaller_than_chunk_size_is_one_chunk() {
        let data = b"hello world";
        let got = chunked_sha256_hex(data);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0], sha256_hex(data));
    }

    #[test]
    fn chunked_exact_chunk_size_is_one_chunk() {
        let data = vec![0x33u8; CHUNK_SIZE];
        let got = chunked_sha256_hex(&data);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0], sha256_hex(&data));
    }

    /// The primary per-chunk acceptance test. 600 KiB = two full
    /// 256 KiB chunks + an 88 KiB tail. The chunked output must match
    /// one-shot per-chunk digests, and the result must be stable across
    /// repeated calls.
    #[test]
    fn chunked_600kib_matches_per_chunk_one_shot_and_is_stable() {
        const SZ: usize = 600 * 1024;
        let data: Vec<u8> = (0..SZ).map(|i| (i & 0xFF) as u8).collect();

        let first = chunked_sha256_hex(&data);
        let second = chunked_sha256_hex(&data);
        assert_eq!(first, second, "chunked_sha256_hex must be deterministic");

        // 2 full + 1 tail = 3 chunks
        assert_eq!(first.len(), 3);
        assert_eq!(first[0], sha256_hex(&data[..CHUNK_SIZE]));
        assert_eq!(first[1], sha256_hex(&data[CHUNK_SIZE..2 * CHUNK_SIZE]));
        assert_eq!(first[2], sha256_hex(&data[2 * CHUNK_SIZE..]));

        // Full-buffer digest is independent of chunking but must also
        // be stable across runs.
        let full_first = sha256_hex(&data);
        let full_second = sha256_hex(&data);
        assert_eq!(full_first, full_second);
    }

    #[test]
    fn chunked_chunk_boundary_256kib_plus_one() {
        const SZ: usize = (256 * 1024) + 1;
        let data: Vec<u8> = (0..SZ).map(|i| (i & 0xFF) as u8).collect();

        let got = chunked_sha256_hex(&data);
        assert_eq!(got.len(), 2, "1 byte past a chunk boundary => 2 chunks");
        assert_eq!(got[0], sha256_hex(&data[..CHUNK_SIZE]));
        assert_eq!(got[1], sha256_hex(&data[CHUNK_SIZE..]));
    }

    // ----- BLAKE3 re-export sanity -----

    #[test]
    fn blake3_re_export_empty() {
        assert_eq!(
            blake3_hex(b""),
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
        );
    }

    // ----- 1 GiB determinism (SHA-256) -----

    /// The roadmap acceptance test, SHA-256 side: hash the same 1 GiB
    /// pseudo-random buffer twice and assert identical digests. Like
    /// the BLAKE3 version, we stream a small scratch buffer (4 MiB)
    /// rather than allocating the full 1 GiB.
    #[test]
    fn one_gib_random_buffer_hashes_identically_twice_sha256() {
        const ONE_GIB: u64 = 1u64 << 30;
        const SCRATCH: usize = 1 << 22; // 4 MiB
        const ITERATIONS: u64 = ONE_GIB / (SCRATCH as u64);

        // Deterministic SplitMix64 so the test is reproducible.
        let mut seed: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut buf = vec![0u8; SCRATCH];
        for chunk in buf.chunks_mut(8) {
            seed = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = seed;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^= z >> 31;
            let bytes = z.to_le_bytes();
            for (dst, src) in chunk.iter_mut().zip(bytes.iter()) {
                *dst = *src;
            }
        }

        let started = std::time::Instant::now();
        let mut h1 = Sha256Hasher::new();
        let mut h2 = Sha256Hasher::new();
        let mut fed: u64 = 0;
        for _ in 0..ITERATIONS {
            h1.update(&buf);
            h2.update(&buf);
            fed += buf.len() as u64;
        }
        let d1 = h1.finalize_hex();
        let d2 = h2.finalize_hex();
        let elapsed = started.elapsed();

        assert_eq!(fed, ONE_GIB);
        assert_eq!(d1, d2, "two SHA-256 passes over the same buffer must match");
        eprintln!(
            "sha256 1GiB x2 random buffer: {elapsed:?} ({} MB/s)",
            (2 * ONE_GIB / (1024 * 1024)) / (elapsed.as_millis().max(1) as u64 / 1000).max(1)
        );
    }

    // ---- Default impl symmetry with BLAKE3

    #[test]
    fn sha256_default_impl_matches_new() {
        let via_default = Sha256Hasher::default().finalize_hex();
        let via_new = Sha256Hasher::new().finalize_hex();
        assert_eq!(via_default, via_new);
        // And both equal the empty-input SHA-256 digest.
        assert_eq!(via_default, sha256_hex(b""));
    }

    // ---- Combined per-chunk SHA-256 + final BLAKE3 (P1-T03 acceptance)

    /// P1-T03 roadmap acceptance: "a chunked hash test that splits a
    /// buffer into 256 KiB chunks and asserts per-chunk SHA-256 + final
    /// BLAKE3 are stable." This test pins BOTH halves on the SAME input
    /// so a future regression in either is caught by one failure.
    #[test]
    fn chunked_sha256_plus_final_blake3_is_stable_on_600kib() {
        let mut data = Vec::with_capacity(600 * 1024);
        let mut rng = 0xC0FFEE_u64;
        for _ in 0..(600 * 1024 / 8) {
            // SplitMix64 step
            rng = rng.wrapping_add(0x9E3779B97F4A7C15);
            let mut z = rng;
            for _ in 0..8 {
                data.push((z as u8) ^ ((z >> 8) as u8) ^ ((z >> 16) as u8) ^ ((z >> 24) as u8));
                z >>= 8;
            }
        }
        assert_eq!(data.len(), 600 * 1024);

        // Per-chunk SHA-256 (256 KiB chunks, last is 88 KiB).
        let per_chunk_1 = chunked_sha256_hex(&data);
        let per_chunk_2 = chunked_sha256_hex(&data);
        assert_eq!(per_chunk_1, per_chunk_2);
        assert_eq!(per_chunk_1.len(), 3, "600 KiB -> 2 full + 1 partial chunk");
        // And each chunk matches the per-chunk one-shot.
        for (i, chunk) in data.chunks(CHUNK_SIZE).enumerate() {
            assert_eq!(
                per_chunk_1[i],
                sha256_hex(chunk),
                "chunk {i} per-chunk SHA-256 must match one-shot"
            );
        }

        // Final full-file BLAKE3, stable across repeated calls.
        let blake3_a = blake3_hex(&data);
        let blake3_b = blake3_hex(&data);
        assert_eq!(blake3_a, blake3_b);
        assert_eq!(
            blake3_a.len(),
            64,
            "BLAKE3 digest must be 64 lowercase hex chars"
        );
    }

    /// Same combined assertion at the 256 KiB + 1 byte boundary.
    #[test]
    fn chunked_sha256_plus_final_blake3_is_stable_on_chunk_boundary_plus_one() {
        let size = CHUNK_SIZE + 1;
        let mut data = vec![0u8; size];
        for (i, b) in data.iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(31).wrapping_add(7);
        }

        let per_chunk_1 = chunked_sha256_hex(&data);
        let per_chunk_2 = chunked_sha256_hex(&data);
        assert_eq!(per_chunk_1, per_chunk_2);
        assert_eq!(per_chunk_1.len(), 2, "256 KiB + 1 -> 1 full + 1 byte chunk");
        assert_eq!(per_chunk_1[0], sha256_hex(&data[..CHUNK_SIZE]));
        assert_eq!(per_chunk_1[1], sha256_hex(&data[CHUNK_SIZE..]));

        let blake3_a = blake3_hex(&data);
        let blake3_b = blake3_hex(&data);
        assert_eq!(blake3_a, blake3_b);
    }
}
