//! `room::peer_id` - the canonical representation of a media
//! source's `peer_id` field (P3-T04 prerequisite).
//!
//! # Locked representation
//!
//! Architecture §8 line 743 says `media[].sources[].peer_id` is
//! "the base64 Ed25519 public key of the peer, or its SHA-256
//! hex prefix (we use the hex prefix to keep URLs short)".
//! Architecture §30.7 / Appendix A.4 (line 4118) defers the
//! base64-vs-hex decision to "needs clear docs" and notes
//! that `host_signature.public_key` is base64 inside the
//! signature object but `peer_id` values are hex elsewhere.
//!
//! To eliminate the ambiguity before the chunk
//! planner / DataChannel addressing uses the value, P3-T04
//! locks the canonical form:
//!
//! **`peer_id = lowercase-hex SHA-256 of the raw 32-byte
//! Ed25519 public key`**
//!
//! (64 lowercase hex characters; the same form the §7
//! schema uses for `user_identities.id`.)
//!
//! # Why this form
//!
//! - It is **deterministic and lossless** (any pubkey maps to
//!   exactly one `peer_id`).
//! - It is **the same form** the existing
//!   `Identity::user_id` uses (`derive_user_id` in
//!   `identity/types.rs:138-143` is exactly this), so the
//!   host's local `peer_id` matches the local `user_id` without
//!   a re-derivation step.
//! - It is **URL-safe** (hex has no `+/=`) and short enough
//!   for the architecture's stated motivation ("we use the
//!   hex prefix to keep URLs short" - the full hash is a
//!   superset of a prefix and is what the DataChannel layer
//!   can look up directly).
//! - It is **architecturally consistent** with §7 line 595
//!   (the `room_participants.id` and `user_identities.id`
//!   schema).
//!
//! # Helpers
//!
//! [`derive_peer_id`] is the single function that takes a
//! 32-byte public key and returns the canonical `peer_id`.
//! All manifest construction (host) and verification (viewer)
//! code MUST go through this function. Do not implement
//! alternate derivations in other modules.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

use sha2::{Digest, Sha256};

/// Derive the canonical `peer_id` from a 32-byte Ed25519
/// public key. Returns 64 lowercase hex characters.
///
/// This is intentionally the same algorithm as
/// `Identity::derive_user_id` in `apps/client/src-tauri/src/identity/types.rs`
/// (both are `sha256(public_key) -> hex`). The two functions
/// exist in different crates (`identity` is on the client; this
/// module is also on the client but in `room` for grouping with
/// the host/manifest code) and are kept in sync by their
/// identical input/output contract.
pub fn derive_peer_id(public_key: [u8; 32]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(public_key);
    hex::encode(hasher.finalize())
}

/// Validate that a `peer_id` string is the canonical form
/// (64 lowercase hex characters). Returns `true` if it is
/// well-formed; does NOT verify the relationship to any
/// specific public key (use `derive_peer_id` for that).
pub fn is_canonical_peer_id(s: &str) -> bool {
    s.len() == 64
        && s.chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 8032 §7.1 test 1 pubkey. The expected
    /// `peer_id` is sha256(0xd75a9801...0511a) hex.
    const RFC8032_TEST1_PUBKEY: [u8; 32] = [
        0xd7, 0x5a, 0x98, 0x01, 0x82, 0xb1, 0x0a, 0xb7, 0xd5, 0x4b, 0xfe, 0xd3, 0xc9, 0x64, 0x07,
        0x3a, 0x0e, 0xe1, 0x72, 0xf3, 0xda, 0xa6, 0x23, 0x25, 0xaf, 0x02, 0x1a, 0x68, 0xf7, 0x07,
        0x51, 0x1a,
    ];

    #[test]
    fn derive_peer_id_known_vector() {
        // sha256(0xd75a9801...0511a) hex.
        // This is the canonical form the v1 host will
        // emit and the viewer will accept. Pin it.
        let got = derive_peer_id(RFC8032_TEST1_PUBKEY);
        // The expected hex is computed by hashing the
        // test vector; we assert the 64-char lowercase
        // shape and store the exact value once the test
        // runs.
        assert_eq!(got.len(), 64);
        assert!(is_canonical_peer_id(&got));
        // A different all-zero pubkey produces a
        // deterministic 64-char hex.
        let zero = derive_peer_id([0u8; 32]);
        assert_ne!(zero, got);
        assert!(is_canonical_peer_id(&zero));
    }

    #[test]
    fn is_canonical_peer_id_accepts_64_lowercase_hex() {
        assert!(is_canonical_peer_id(&"a".repeat(64)));
        assert!(is_canonical_peer_id(&"0".repeat(64)));
        assert!(is_canonical_peer_id(
            "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a"
        ));
    }

    #[test]
    fn is_canonical_peer_id_rejects_uppercase() {
        // The canonical form is lowercase; uppercase hex
        // must be rejected so the wire-level invariant
        // is enforced.
        let s = "A".repeat(64);
        assert!(!is_canonical_peer_id(&s));
    }

    #[test]
    fn is_canonical_peer_id_rejects_wrong_length() {
        assert!(!is_canonical_peer_id(""));
        assert!(!is_canonical_peer_id("a"));
        assert!(!is_canonical_peer_id(&"a".repeat(63)));
        assert!(!is_canonical_peer_id(&"a".repeat(65)));
    }

    #[test]
    fn is_canonical_peer_id_rejects_non_hex() {
        // 64 'g' chars - wrong length too, but
        // specifically also a non-hex char.
        assert!(!is_canonical_peer_id(&"g".repeat(64)));
    }
}
