//! SHA-256 digest helpers used by the server (bearer token hashing
//! and other one-shot digests) and shared with the client.
//!
//! The output is the 32-byte SHA-256 digest; this module exposes the
//! raw `[u8; 32]` form, the lowercase hex form, and the standard
//! base64 form.

#![forbid(unsafe_code)]

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use sha2::{Digest, Sha256};

/// Compute the SHA-256 digest of `bytes` and return the raw
/// 32-byte output.
pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let out = hasher.finalize();
    let mut result = [0u8; 32];
    result.copy_from_slice(&out);
    result
}

/// Compute the SHA-256 digest of `bytes` and return it as 64
/// lowercase hex characters.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(sha256(bytes))
}

/// Compute the SHA-256 digest of `bytes` and return it as
/// standard base64 (44 characters, including the `=` padding).
pub fn sha256_base64(bytes: &[u8]) -> String {
    BASE64.encode(sha256(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SHA-256 of the empty string is a documented known vector.
    #[test]
    fn empty_string_known_digest() {
        let expected = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        assert_eq!(sha256_hex(b""), expected);
    }

    /// SHA-256 of "abc" is a documented known vector.
    #[test]
    fn abc_known_digest() {
        let expected = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert_eq!(sha256_hex(b"abc"), expected);
    }

    /// `sha256_base64` is the standard base64 encoding of the
    /// 32-byte digest. SHA-256 of the empty string decodes to
    /// `e3b0c4...` which base64-encodes to `47DEQpj8...`.
    #[test]
    fn empty_string_base64() {
        let expected = "47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=";
        assert_eq!(sha256_base64(b""), expected);
    }

    /// All three forms are consistent with each other.
    #[test]
    fn consistent_across_forms() {
        let raw = sha256(b"locast");
        let hex = sha256_hex(b"locast");
        let b64 = sha256_base64(b"locast");
        assert_eq!(hex.len(), 64);
        assert_eq!(hex, hex::encode(raw));
        assert_eq!(b64, BASE64.encode(raw));
    }
}
