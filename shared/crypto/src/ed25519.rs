//! Ed25519 sign / verify helpers.
//!
//! Thin wrappers around `ed25519_dalek` that enforce two invariants:
//!
//! 1. The 32-byte "public key" input is non-zero. The all-zero public
//!    key is a known small-subgroup attack target on some Ed25519
//!    implementations; the verification API does not catch it
//!    uniformly, so we check up front and return [`CryptoError::InvalidKey`]
//!    for any all-zero input.
//! 2. The signature buffer is exactly 64 bytes. The dalek `Signature`
//!    type is exactly 64 bytes, so a wrong-length slice is a contract
//!    violation rather than an error from dalek; we surface it as
//!    [`CryptoError::InvalidSignature`].
//!
//! Both functions are pure: no I/O, no clocks, no panics on any input
//! shape.
//!
//! Per `docs/ARCHITECTURE.md` section 20.4.4 the handshake signs the
//! raw 32-byte nonce with no domain separation tag and no prehashing,
//! so we use `verify_strict` (the Ed25519 RFC 8032 "pure" mode). The
//! domain-tagged post-handshake signing pipeline (§18.9) is built on
//! top of the same primitive; the tag is concatenated in the
//! `signing` / `verifying` layer above this module.

#![forbid(unsafe_code)]

use thiserror::Error;

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;

/// Errors raised by [`sign`] and [`verify`].
#[derive(Debug, Error, PartialEq, Eq)]
pub enum CryptoError {
    /// The 32-byte public key is structurally invalid (all-zero or
    /// not a valid Ed25519 point).
    #[error("invalid Ed25519 public key")]
    InvalidKey,

    /// The signature is not exactly 64 bytes, or the inner
    /// verification failed.
    #[error("invalid Ed25519 signature")]
    InvalidSignature,
}

/// Sign `message` with the Ed25519 signing key derived from
/// `signing_key_bytes` (the 32-byte private seed). Returns a 64-byte
/// signature.
///
/// The signature is computed in Ed25519 "pure" mode: no prehashing,
/// no context, no domain separation tag. The handshake signs the
/// raw 32-byte nonce (§20.4.4); callers that need a domain tag
/// must concatenate it in front of the message.
pub fn sign(signing_key_bytes: &[u8; 32], message: &[u8]) -> [u8; 64] {
    let signing = SigningKey::from_bytes(signing_key_bytes);
    let sig = signing.sign(message);
    sig.to_bytes()
}

/// Verify an Ed25519 signature over `message` in "pure" mode (RFC
/// 8032 Ed25519, no prehashing, no context). The all-zero public
/// key is rejected up front as `Err(CryptoError::InvalidKey)`
/// because it is a known small-subgroup target.
///
/// Returns `Ok(())` on a valid signature and
/// `Err(CryptoError::InvalidSignature)` on any verification
/// failure (malformed signature, signature does not verify, etc.).
pub fn verify(
    public_key_bytes: &[u8; 32],
    message: &[u8],
    signature: &[u8; 64],
) -> Result<(), CryptoError> {
    if public_key_bytes.iter().all(|b| *b == 0) {
        return Err(CryptoError::InvalidKey);
    }

    let verifying =
        VerifyingKey::from_bytes(public_key_bytes).map_err(|_| CryptoError::InvalidKey)?;

    let sig = Signature::from_bytes(signature);

    verifying
        .verify_strict(message, &sig)
        .map_err(|_| CryptoError::InvalidSignature)
}

/// Encode a 32-byte public or private key as standard base64.
/// Provided as a convenience for the test harness and the
/// client-side identity layer; not used on the server.
pub fn to_base64(bytes: &[u8]) -> String {
    BASE64.encode(bytes)
}

/// Derive the 32-byte Ed25519 verifying (public) key from a raw
/// private seed. The Ed25519 key-derivation step is infallible
/// (any 32 bytes are a valid seed) so this never fails.
///
/// Callers that already hold a public key should use it directly
/// rather than re-deriving from the seed; this helper exists for
/// the manifest signing path and the test harness.
pub fn public_key_from_seed(seed: &[u8; 32]) -> [u8; 32] {
    let signing = SigningKey::from_bytes(seed);
    signing.verifying_key().to_bytes()
}

/// Decode a standard base64 string with `=` padding into its raw
/// bytes. Returns [`CryptoError::InvalidKey`] on any decode failure
/// (invalid alphabet, bad padding, or empty input). The variant is
/// reused because a malformed base64 blob is structurally
/// indistinguishable from "this is not a valid public key or
/// signature blob" at the call site, and adding a separate variant
/// would force every caller to match on two cases for the same
/// operational outcome: "the input bytes I got are garbage".
///
/// Empty input is rejected explicitly. The standard base64 engine
/// accepts `""` as the empty byte string, but no Locast key or
/// signature is ever the empty string, so treating it as a
/// successful decode would let the rest of the pipeline reach a
/// "wrong length" branch with a confusing error path.
pub fn from_base64(s: &str) -> Result<Vec<u8>, CryptoError> {
    if s.is_empty() {
        return Err(CryptoError::InvalidKey);
    }
    BASE64.decode(s).map_err(|_| CryptoError::InvalidKey)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::OsRng;
    use rand::RngCore;

    /// Round-trip: sign a message, verify it with the matching
    /// public key.
    #[test]
    fn round_trip_sign_verify() {
        let mut seed = [0u8; 32];
        OsRng.fill_bytes(&mut seed);
        let signing = SigningKey::from_bytes(&seed);
        let public = signing.verifying_key().to_bytes();

        let msg = b"hello locast";
        let sig = sign(&seed, msg);
        verify(&public, msg, &sig).expect("valid signature should verify");
    }

    /// A signature produced with a different key fails to verify.
    #[test]
    fn verify_rejects_wrong_key() {
        let mut seed_a = [0u8; 32];
        OsRng.fill_bytes(&mut seed_a);
        let mut seed_b = [0u8; 32];
        OsRng.fill_bytes(&mut seed_b);

        let b = SigningKey::from_bytes(&seed_b);
        let public_b = b.verifying_key().to_bytes();

        let msg = b"a signed me";
        let sig_a_over_msg = sign(&seed_a, msg);

        let res = verify(&public_b, msg, &sig_a_over_msg);
        assert!(matches!(res, Err(CryptoError::InvalidSignature)));
    }

    /// A modified message fails to verify.
    #[test]
    fn verify_rejects_modified_message() {
        let mut seed = [0u8; 32];
        OsRng.fill_bytes(&mut seed);
        let signing = SigningKey::from_bytes(&seed);
        let public = signing.verifying_key().to_bytes();

        let sig = sign(&seed, b"original");
        let res = verify(&public, b"modified", &sig);
        assert!(matches!(res, Err(CryptoError::InvalidSignature)));
    }

    /// The all-zero public key is rejected up front as
    /// `InvalidKey`, never reaching the dalek verify path.
    #[test]
    fn verify_rejects_all_zero_public_key() {
        let zero = [0u8; 32];
        let dummy_sig = [0u8; 64];
        let res = verify(&zero, b"anything", &dummy_sig);
        assert_eq!(res, Err(CryptoError::InvalidKey));
    }

    /// A modified signature byte is rejected.
    #[test]
    fn verify_rejects_modified_signature() {
        let mut seed = [0u8; 32];
        OsRng.fill_bytes(&mut seed);
        let signing = SigningKey::from_bytes(&seed);
        let public = signing.verifying_key().to_bytes();

        let mut sig = sign(&seed, b"abc");
        sig[0] ^= 0x01;
        let res = verify(&public, b"abc", &sig);
        assert!(matches!(res, Err(CryptoError::InvalidSignature)));
    }

    /// Determinism: signing the same (key, message) twice
    /// produces the same 64-byte signature.
    #[test]
    fn sign_is_deterministic() {
        let seed = [42u8; 32];
        let a = sign(&seed, b"deterministic");
        let b = sign(&seed, b"deterministic");
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
    }

    /// `to_base64` then `from_base64` round-trips to the same bytes.
    #[test]
    fn base64_round_trip() {
        let original = [0x42u8; 32];
        let encoded = to_base64(&original);
        let decoded = from_base64(&encoded).expect("valid base64 should decode");
        assert_eq!(decoded, original.to_vec());
    }

    /// Empty input is not a valid key blob. The standard base64
    /// engine accepts empty as "empty bytes" (a valid decoding),
    /// so we reject empty up front.
    #[test]
    fn from_base64_rejects_empty() {
        let res = from_base64("");
        assert_eq!(res, Err(CryptoError::InvalidKey));
    }

    /// Non-base64 alphabet characters are rejected.
    #[test]
    fn from_base64_rejects_garbage() {
        let res = from_base64("not!base64!!!");
        assert_eq!(res, Err(CryptoError::InvalidKey));
    }

    /// Base64 that does not decode to a multiple of 4 chars (bad
    /// padding) is rejected.
    #[test]
    fn from_base64_rejects_bad_padding() {
        // 5 chars: not a multiple of 4, no `=` padding makes it valid.
        let res = from_base64("AAAAA");
        assert_eq!(res, Err(CryptoError::InvalidKey));
    }
}
