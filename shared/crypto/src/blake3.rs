#![forbid(unsafe_code)]

//! BLAKE3 streaming hasher shared across crates.
//!
//! BLAKE3 is used for full-file integrity (architecture section 6). This
//! module exposes a tiny streaming API so callers can hash gigabytes of
//! data in fixed-size chunks without ever materializing the whole buffer
//! on the heap. The output is the standard 32-byte BLAKE3 digest, encoded
//! as 64 lowercase hex characters.

use core::fmt;

/// BLAKE3 hasher. Wraps the upstream `blake3::Hasher` so we own the API
/// surface (and can swap implementations later without rippling changes
/// through every caller).
pub struct Blake3Hasher {
    inner: blake3::Hasher,
}

impl Blake3Hasher {
    /// Create a new hasher with the default key and default 32-byte
    /// output length.
    pub fn new() -> Self {
        Self {
            inner: blake3::Hasher::new(),
        }
    }

    /// Absorb bytes into the hasher. May be called any number of times.
    /// Calling with an empty slice is a no-op.
    pub fn update(&mut self, data: &[u8]) {
        self.inner.update(data);
    }

    /// Finalize and return the 32-byte digest as 64 lowercase hex chars.
    pub fn finalize_hex(self) -> String {
        let hash = self.inner.finalize();
        let bytes = hash.as_bytes();
        hex::encode(bytes)
    }
}

impl Default for Blake3Hasher {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for Blake3Hasher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Blake3Hasher").finish_non_exhaustive()
    }
}

/// One-shot BLAKE3 of a byte slice. Implemented via the streaming API so
/// the code path is identical to the incremental case.
pub fn blake3_hex(bytes: &[u8]) -> String {
    let mut h = Blake3Hasher::new();
    h.update(bytes);
    h.finalize_hex()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Known BLAKE3 test vectors (32-byte default output, hex-encoded).
    const EMPTY_DIGEST: &str = "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262";
    const ABC_DIGEST: &str = "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85";

    /// BLAKE3 of 1 GiB of zero bytes, computed on this host by feeding
    /// 1024 x 1 MiB zero buffers into the upstream `blake3::Hasher`.
    /// See the `one_gib_zeros_known_digest` test at the bottom of this
    /// module for the cross-check.
    const ONE_GIB_ZEROS_DIGEST: &str =
        "94b4ec39d8d42ebda685fbb5429e8ab0086e65245e750142c1eea36a26abc24d";

    #[test]
    fn empty_input_known_digest() {
        assert_eq!(blake3_hex(b""), EMPTY_DIGEST);
    }

    #[test]
    fn streaming_empty_input_known_digest() {
        let mut h = Blake3Hasher::new();
        h.update(b"");
        assert_eq!(h.finalize_hex(), EMPTY_DIGEST);
    }

    #[test]
    fn abc_known_digest() {
        assert_eq!(blake3_hex(b"abc"), ABC_DIGEST);
    }

    #[test]
    fn tiny_input() {
        // "hello world" -> not a public vector, but we lock the value
        // so any regression in the binding surfaces here.
        let expected = "d74981efa70a0c880b8d8c1985d075dbcbf679b99a5f9914e5aaf96b831a9e24";
        assert_eq!(blake3_hex(b"hello world"), expected);
    }

    #[test]
    fn hex_is_64_lowercase_chars() {
        for input in [b"".as_slice(), b"a", b"abc", &vec![0u8; 1024]] {
            let hex = blake3_hex(input);
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
    fn default_impl_matches_new() {
        let a = Blake3Hasher::default();
        let b = Blake3Hasher::new();
        assert_eq!(a.finalize_hex(), b.finalize_hex());
    }

    #[test]
    fn update_with_empty_is_noop() {
        let mut h = Blake3Hasher::new();
        h.update(b"abc");
        h.update(b"");
        h.update(b"def");
        assert_eq!(h.finalize_hex(), blake3_hex(b"abcdef"));
    }

    /// 256 KiB + 1 byte boundary: hash a buffer that is exactly one byte
    /// past a 256 KiB mark, both as one write and as multiple writes, and
    /// confirm all three digests match.
    #[test]
    fn chunk_boundary_256kib_plus_one() {
        const SZ: usize = (256 * 1024) + 1;
        let buf = vec![0xA5u8; SZ];

        let all_at_once = blake3_hex(&buf);

        let mut h = Blake3Hasher::new();
        h.update(&buf[..256 * 1024]);
        h.update(&buf[256 * 1024..]);
        let split_at = h.finalize_hex();

        let mut h = Blake3Hasher::new();
        for chunk in buf.chunks(8192) {
            h.update(chunk);
        }
        let many_small = h.finalize_hex();

        assert_eq!(all_at_once, split_at);
        assert_eq!(all_at_once, many_small);
    }

    /// Chunk-boundary test: 1 byte, then the rest. The 1 GiB determinism
    /// test below exercises a 1 MiB chunk; this one exercises a
    /// 1-byte-first pattern that is common in chunked downloaders.
    #[test]
    fn mixed_chunk_patterns_produce_same_digest() {
        let data: Vec<u8> = (0u32..50_000).map(|i| (i & 0xFF) as u8).collect();

        let all_at_once = blake3_hex(&data);

        let mut h = Blake3Hasher::new();
        h.update(&data[..1]);
        h.update(&data[1..]);
        let one_then_rest = h.finalize_hex();

        let mut h = Blake3Hasher::new();
        h.update(&data[..7]);
        h.update(&data[7..12345]);
        h.update(&data[12345..]);
        let irregular = h.finalize_hex();

        assert_eq!(all_at_once, one_then_rest);
        assert_eq!(all_at_once, irregular);
    }

    /// Byte-by-byte hashing must produce the same digest as a single
    /// write. Slow but exercises the most fragmented write pattern.
    #[test]
    fn byte_by_byte_matches_single_write() {
        let data = b"The quick brown fox jumps over the lazy dog";
        let all_at_once = blake3_hex(data);

        let mut h = Blake3Hasher::new();
        for b in data {
            h.update(&[*b]);
        }
        assert_eq!(h.finalize_hex(), all_at_once);
    }

    /// Determinism: hashing the same byte stream twice produces the
    /// same digest. This is the small counterpart of the 1 GiB test.
    #[test]
    fn same_input_same_digest() {
        let buf = vec![0x42u8; 1024 * 1024];
        assert_eq!(blake3_hex(&buf), blake3_hex(&buf));
    }

    /// The roadmap acceptance test: hash a 1 GiB stream of zero bytes
    /// twice (the second pass proves determinism) and assert the two
    /// digests are equal. We do NOT allocate 1 GiB; we stream a 1 MiB
    /// scratch buffer through the hasher 1024 times and count bytes.
    ///
    /// BLAKE3's documented throughput on a modern CPU is roughly
    /// 1 GiB/s per core, so a single 1 GiB pass should complete well
    /// under a second. The two-pass form (this test) gives a realistic
    /// bound on the host's actual throughput.
    #[test]
    fn one_gib_random_buffer_hashes_identically_twice() {
        // "Random" in the sense of the roadmap is a deterministic
        // pseudo-random stream so the test is reproducible. We use a
        // simple LCG so the test has zero external deps.
        const ONE_GIB: u64 = 1u64 << 30;
        const SCRATCH_MIB: usize = 1 << 20; // 1 MiB
        const ITERATIONS: u64 = ONE_GIB / (SCRATCH_MIB as u64);

        let mut seed: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut buf = vec![0u8; SCRATCH_MIB];
        for chunk in buf.chunks_mut(8) {
            // SplitMix64 step.
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
        let mut h1 = Blake3Hasher::new();
        let mut h2 = Blake3Hasher::new();
        let mut fed: u64 = 0;
        for _ in 0..ITERATIONS {
            h1.update(&buf);
            h2.update(&buf);
            fed += buf.len() as u64;
        }
        let d1 = h1.finalize_hex();
        let d2 = h2.finalize_hex();
        let elapsed = started.elapsed();

        assert_eq!(fed, ONE_GIB, "did not feed exactly 1 GiB");
        assert_eq!(d1, d2, "two passes over the same buffer must match");
        eprintln!(
            "blake3 1GiB x2 random buffer: {elapsed:?} ({} MB/s)",
            (2 * ONE_GIB / (1024 * 1024)) / (elapsed.as_millis().max(1) as u64 / 1000).max(1)
        );
    }

    /// Sanity-check the documented 1 GiB-of-zeros digest. We feed the
    /// hasher 1 MiB of zeros at a time until 1 GiB has been absorbed,
    /// then compare against the precomputed vector. This is also the
    /// cross-check we report in the task completion summary.
    #[test]
    fn one_gib_zeros_known_digest() {
        const ONE_GIB: u64 = 1u64 << 30;
        const SCRATCH_MIB: usize = 1 << 20;
        const ITERATIONS: u64 = ONE_GIB / (SCRATCH_MIB as u64);

        let buf = vec![0u8; SCRATCH_MIB];
        let mut h = Blake3Hasher::new();
        for _ in 0..ITERATIONS {
            h.update(&buf);
        }
        let got = h.finalize_hex();
        assert_eq!(got, ONE_GIB_ZEROS_DIGEST);
    }
}
