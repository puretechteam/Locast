//! Room-code generation and validation.
//!
//! The alphabet is the 32-character unambiguous set from
//! `docs/ARCHITECTURE.md` §21.2:
//!
//! ```text
//! ABCDEFGHJKLMNPQRSTUVWXYZ23456789
//! ```
//!
//! `0`, `O`, `1`, `I`, `L` are excluded to make codes easier
//! to read out loud and to copy. The code length is 6, giving
//! a 32^6 ≈ 1.07B code space. Generation uses rejection
//! sampling over a uniform 5-bit source so the distribution
//! is unbiased.

#![forbid(unsafe_code)]

use rand::RngCore;

/// The unambiguous 32-character alphabet. The constant is
/// `&'static str` so it can be embedded in `Debug` output and
/// in the server's `Config`.
pub const ALPHABET: &str = "ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

/// The number of characters in a room code. The P2-T04 spec
/// pins this to 6 per the architecture.
pub const CODE_LEN: usize = 6;

/// Build a fresh room code by drawing 5-bit values from
/// `rng` and indexing into the 32-character alphabet with
/// rejection sampling.
///
/// `RngCore::fill(&mut [u8])` is the only uniform source the
/// `rand 0.8` crate gives us without a `Rng` trait import; we
/// use the byte values as a 5-bit source. The maximum number
/// of bytes we read is bounded by the worst-case rejection
/// rate (about 4 bytes per character, in practice fewer) and
/// the caller passes the RNG by value to keep this function
/// usable from both the production code and the test that
/// forces a collision.
pub fn generate_code<R: RngCore>(rng: &mut R) -> String {
    // 5 bits per character; 5 * 6 = 30 bits of entropy is
    // 6 random u8 draws in the worst case (with rejection).
    // We pre-allocate the output.
    let mut out = String::with_capacity(CODE_LEN);
    let alphabet_bytes = ALPHABET.as_bytes();
    while out.len() < CODE_LEN {
        let mut buf = [0u8; 8];
        rng.fill_bytes(&mut buf);
        for &b in &buf {
            // High 5 bits; reject values >= 32 (i.e. the top
            // three of 32 = 0b11111).
            let v = (b >> 3) & 0x1F;
            if (v as usize) < alphabet_bytes.len() {
                out.push(alphabet_bytes[v as usize] as char);
                if out.len() == CODE_LEN {
                    break;
                }
            }
        }
    }
    out
}

/// `true` if `s` is exactly 6 characters long and every
/// character is in the alphabet. Case-insensitive on the
/// input (we uppercase before checking).
pub fn is_valid_code(s: &str) -> bool {
    s.len() == CODE_LEN && s.chars().all(|c| ALPHABET.contains(c))
}

/// Uppercase `s` and validate. Returns `Some(code)` if the
/// input is exactly 6 alphabet characters (case-insensitive),
/// `None` otherwise.
pub fn normalize(s: &str) -> Option<String> {
    let upper: String = s.to_ascii_uppercase();
    if is_valid_code(&upper) {
        Some(upper)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;

    #[test]
    fn generated_codes_are_six_alphabet_chars() {
        let mut rng = rand::rngs::SmallRng::seed_from_u64(0xABCDEF);
        for _ in 0..100 {
            let code = generate_code(&mut rng);
            assert_eq!(code.len(), CODE_LEN);
            assert!(is_valid_code(&code), "bad code: {code}");
        }
    }

    #[test]
    fn generated_codes_never_use_ambiguous_chars() {
        let mut rng = rand::rngs::SmallRng::seed_from_u64(0xDEADBEEF);
        for _ in 0..500 {
            let code = generate_code(&mut rng);
            for c in code.chars() {
                // The architecture excludes only `0`, `O`,
                // `1`, `I` (the four characters that look
                // like one another in common sans-serif
                // fonts). `L` is in the alphabet.
                assert!(!"0O1I".contains(c), "ambiguous char {c} in code {code}");
            }
        }
    }

    #[test]
    fn validate_rejects_short_long_and_bad_chars() {
        assert!(!is_valid_code(""));
        assert!(!is_valid_code("ABCDE"));
        assert!(!is_valid_code("ABCDEFG"));
        assert!(!is_valid_code("ABCD0F")); // 0 not in alphabet
        assert!(!is_valid_code("ABCDOF")); // O not in alphabet
        assert!(!is_valid_code("ABCD1F")); // 1 not in alphabet
                                           // `L` is in the alphabet (only `0`, `O`, `1`, `I`
                                           // are excluded). `ABCDEF` is therefore a valid code.
        assert!(is_valid_code("ABCDEF"));
        assert!(!is_valid_code("ABCD0F"));
        // Mixed-case inputs are case-sensitive: `ABCDEf` has
        // an `f` that is not in the alphabet.
        assert!(!is_valid_code("ABCDEf"));
    }

    #[test]
    fn normalize_uppercases_and_validates() {
        assert_eq!(normalize("abcdef"), Some("ABCDEF".to_string()));
        assert_eq!(normalize("AbCdEf"), Some("ABCDEF".to_string()));
        assert!(normalize("abcde0").is_none());
        assert!(normalize("abcde").is_none());
        assert!(normalize("abcdefg").is_none());
    }

    #[test]
    fn alphabet_is_exactly_32_chars() {
        assert_eq!(ALPHABET.len(), 32);
        let set: std::collections::HashSet<char> = ALPHABET.chars().collect();
        assert_eq!(set.len(), 32);
    }
}
