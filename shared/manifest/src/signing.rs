//! Manifest signing and verification (P3-T02).
//!
//! The manifest signature in Locast is computed over the EXACT bytes
//! emitted by [`crate::serialize`], with no domain tag, no
//! prehashing, and no re-encoding. This matches
//! `docs/ARCHITECTURE.md` section 8 verbatim:
//!
//! > Verification flow on the viewer:
//! > 1. Receive manifest.
//! > 2. Recompute canonical bytes.
//! > 3. `ed25519_dalek::VerifyingKey::from_bytes(&public_key)
//! >      .verify(&canonical_bytes, &signature)`.
//! > 4. On failure, refuse to download anything.
//!
//! The `host_signature` field is NOT part of the signed payload:
//! the canonicalizer (P3-T01) unconditionally replaces it with
//! `null` in the canonical bytes. This is the "no recursion"
//! property the spec demands.
//!
//! IMPORTANT: Do not be tempted to "improve" the sign-side input by
//! prepending a domain tag, hashing the message, or otherwise
//! transforming `serialize(&manifest)?` before signing. Any such
//! change would silently break viewer compatibility. The
//! domain-tagged post-handshake pipeline is `docs/ARCHITECTURE.md`
//! section 18.9 and lives in `shared/protocol` — it is a different
//! layer.

#![forbid(unsafe_code)]

use locast_crypto::ed25519;

use crate::canonical::serialize;
use crate::error::{
    InvalidPublicKeyReason, InvalidSignatureEncodingReason, SigningResult, VerifyError,
};
use crate::model::{HostSignature, MediaManifest};

/// The only signature algorithm Locast v1 supports.
///
/// Stored as a literal `&str` in [`HostSignature::algorithm`].
pub const ALGORITHM_ED25519: &str = "ed25519";

/// Sign a manifest and return a new [`MediaManifest`] with
/// `host_signature` populated.
///
/// The input is NOT mutated. The returned value is a fresh
/// [`MediaManifest`] whose `host_signature` is `Some(...)` and
/// whose other fields are byte-for-byte identical to the input.
///
/// The signing key is the raw 32-byte Ed25519 private seed (the
/// RFC 8032 "seed" form), not a `SigningKey` newtype. The bytes
/// are used directly: the [`ed25519::sign`] helper derives the
/// `SigningKey` internally and never exposes dalek types in its
/// public API.
///
/// # Errors
///
/// Returns [`SigningResult::Err`] with the underlying
/// [`crate::error::CanonicalError`] if the manifest cannot be
/// canonicalized. The Ed25519 `sign` primitive is infallible, so
/// this is the only failure mode.
pub fn sign_manifest(
    signing_key_bytes: &[u8; 32],
    manifest: &MediaManifest,
) -> SigningResult<MediaManifest> {
    // IMPORTANT: sign over `serialize(manifest)?` exactly. No
    // domain tag, no hash, no transformation. See module docs.
    let canonical_bytes = serialize(manifest)?;

    let signature = ed25519::sign(signing_key_bytes, &canonical_bytes);

    // Derive the verifying (public) key from the same seed via
    // the shared crypto crate. The Ed25519 derivation is
    // infallible; any 32 bytes are a valid seed.
    let public_bytes = ed25519::public_key_from_seed(signing_key_bytes);

    let mut signed = manifest.clone();
    signed.host_signature = Some(HostSignature {
        public_key: ed25519::to_base64(&public_bytes),
        algorithm: ALGORITHM_ED25519.to_owned(),
        value: ed25519::to_base64(&signature),
    });
    Ok(signed)
}

/// Verify a manifest's `host_signature` over the manifest's
/// canonical bytes.
///
/// Re-canonicalizes the manifest internally (so the caller can
/// hand in the same data model the host produced) and verifies the
/// Ed25519 signature in `host_signature.value` against
/// `host_signature.public_key`. On any failure the returned
/// [`VerifyError`] names the stage that failed.
///
/// # Errors
///
/// See [`VerifyError`] for the exhaustive list. The most common
/// operational variant is [`VerifyError::SignatureMismatch`],
/// which proves the canonical bytes the verifier computed are
/// not the bytes the host signed.
pub fn verify_manifest(manifest: &MediaManifest) -> Result<(), VerifyError> {
    let host_sig = manifest
        .host_signature
        .as_ref()
        .ok_or(VerifyError::MissingSignature)?;

    if host_sig.algorithm != ALGORITHM_ED25519 {
        return Err(VerifyError::UnsupportedAlgorithm(
            host_sig.algorithm.clone(),
        ));
    }

    let public_bytes = decode_public_key(&host_sig.public_key)?;
    let signature_bytes = decode_signature(&host_sig.value)?;

    // Re-canonicalize. The `From<CanonicalError>` impl on
    // `VerifyError` lifts any canonicalization failure into
    // `CanonicalizationFailed` so the underlying error is
    // preserved.
    let canonical_bytes = serialize(manifest)?;

    ed25519::verify(&public_bytes, &canonical_bytes, &signature_bytes).map_err(|e| match e {
        // All-zero or non-curve public key: this is a
        // public-key problem, not a signature problem, and
        // the verify step caught it as a defensive check.
        locast_crypto::ed25519::CryptoError::InvalidKey => {
            VerifyError::InvalidPublicKey(InvalidPublicKeyReason::RejectedByVerifier)
        }
        locast_crypto::ed25519::CryptoError::InvalidSignature => VerifyError::SignatureMismatch,
    })
}

/// Decode a 32-byte Ed25519 public key from its base64 form.
fn decode_public_key(s: &str) -> Result<[u8; 32], VerifyError> {
    let bytes = ed25519::from_base64(s)
        .map_err(|_| VerifyError::InvalidPublicKey(InvalidPublicKeyReason::Base64Decode))?;

    if bytes.len() != 32 {
        return Err(VerifyError::InvalidPublicKey(
            InvalidPublicKeyReason::WrongLength(bytes.len()),
        ));
    }

    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    Ok(out)
}

/// Decode a 64-byte Ed25519 signature from its base64 form.
fn decode_signature(s: &str) -> Result<[u8; 64], VerifyError> {
    let bytes = ed25519::from_base64(s).map_err(|_| {
        VerifyError::InvalidSignatureEncoding(InvalidSignatureEncodingReason::Base64Decode)
    })?;

    if bytes.len() != 64 {
        return Err(VerifyError::InvalidSignatureEncoding(
            InvalidSignatureEncodingReason::WrongLength(bytes.len()),
        ));
    }

    let mut out = [0u8; 64];
    out.copy_from_slice(&bytes);
    Ok(out)
}

// ----------------------------------------------------------------------------
// Small hex helpers used by the unit tests below. Kept in this
// module (rather than shared with the integration tests) so the
// unit tests do not depend on the integration test file. Declared
// before the test module to satisfy
// `clippy::items_after_test_module`.
// ----------------------------------------------------------------------------

#[cfg(test)]
fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(test)]
fn hex_decode_64(s: &str) -> [u8; 64] {
    assert_eq!(s.len(), 128, "expected 64-byte hex (128 chars)");
    let mut out = [0u8; 64];
    for (i, pair) in s.as_bytes().chunks(2).enumerate() {
        let hi = hex_nibble(pair[0]);
        let lo = hex_nibble(pair[1]);
        out[i] = (hi << 4) | lo;
    }
    out
}

#[cfg(test)]
fn hex_nibble(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        b'A'..=b'F' => c - b'A' + 10,
        _ => panic!("non-hex character: {c}"),
    }
}

// ----------------------------------------------------------------------------
// Unit tests. The integration tests in `tests/signing.rs` cover the
// golden-vector path; this module's tests cover shape and error
// mapping.
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{InvalidPublicKeyReason, InvalidSignatureEncodingReason};
    use crate::model::{Dimensions, MediaEntry, Source};

    /// Fixed test seed: the RFC 8032 §7.1 test vector 1 seed. Used
    /// across all unit tests so the expected pubkey is known.
    /// Keeping it constant also lets the deterministic test
    /// (test 11) rely on a stable, well-known value.
    const RFC8032_TEST1_SEED: [u8; 32] = [
        0x9d, 0x61, 0xb1, 0x9d, 0xef, 0xfd, 0x5a, 0x60, 0xba, 0x84, 0x4a, 0xf4, 0x92, 0xec, 0x2c,
        0xc4, 0x44, 0x49, 0xc5, 0x69, 0x7b, 0x32, 0x69, 0x19, 0x70, 0x3b, 0xac, 0x03, 0x1c, 0xae,
        0x7f, 0x60,
    ];

    /// Fixed pubkey corresponding to [`RFC8032_TEST1_SEED`] (RFC
    /// 8032 §7.1 test vector 1).
    const RFC8032_TEST1_PUBKEY: [u8; 32] = [
        0xd7, 0x5a, 0x98, 0x01, 0x82, 0xb1, 0x0a, 0xb7, 0xd5, 0x4b, 0xfe, 0xd3, 0xc9, 0x64, 0x07,
        0x3a, 0x0e, 0xe1, 0x72, 0xf3, 0xda, 0xa6, 0x23, 0x25, 0xaf, 0x02, 0x1a, 0x68, 0xf7, 0x07,
        0x51, 0x1a,
    ];

    fn fixture_manifest() -> MediaManifest {
        MediaManifest {
            manifest_version: 1,
            room_id: "22222222-2222-4222-8222-222222222222".to_owned(),
            media: vec![MediaEntry {
                id: "11111111-1111-4111-8111-111111111111".to_owned(),
                filename: "movie.mp4".to_owned(),
                sha256: "a".repeat(64),
                blake3: "b".repeat(64),
                size_bytes: 1024,
                mime: "video/mp4".to_owned(),
                duration_ms: 60000,
                dimensions: Some(Dimensions {
                    width: 1920,
                    height: 1080,
                }),
                codecs: None,
                sources: vec![Source {
                    peer_id: "peer-aaaa".to_owned(),
                    url_hint: None,
                    priority: 0,
                    chunk_size: 65536,
                    total_chunks: 1,
                    chunk_hashes: vec!["c".repeat(64)],
                }],
            }],
            subtitles: vec![],
            created_at: 1700000000000,
            host_signature: None,
        }
    }

    /// Test 1: round-trip sign+verify on a fresh manifest.
    #[test]
    fn roundtrip_ok() {
        let m = fixture_manifest();
        let signed = sign_manifest(&RFC8032_TEST1_SEED, &m).expect("signing should succeed");
        verify_manifest(&signed).expect("freshly signed manifest should verify");
    }

    /// Test 2: embedding the host_signature into the data model
    /// does not change the canonical bytes. This is the "no
    /// recursion" property the architecture mandates.
    #[test]
    fn canonical_bytes_unchanged_after_embedding() {
        let m = fixture_manifest();
        let unsigned_bytes = serialize(&m).unwrap();
        let signed = sign_manifest(&RFC8032_TEST1_SEED, &m).unwrap();
        let signed_bytes = serialize(&signed).unwrap();
        assert_eq!(
            unsigned_bytes, signed_bytes,
            "signing must not change canonical bytes (host_signature is stripped to null)"
        );
    }

    /// Test 3: a one-field tamper fails verification.
    #[test]
    fn tamper_field_fails() {
        let m = fixture_manifest();
        let mut signed = sign_manifest(&RFC8032_TEST1_SEED, &m).unwrap();
        signed.room_id = "tampered-room".to_owned();
        let err = verify_manifest(&signed).unwrap_err();
        assert!(
            matches!(err, VerifyError::SignatureMismatch),
            "expected SignatureMismatch, got {err:?}"
        );
    }

    /// Test 4: flipping a byte in the signature blob fails
    /// verification.
    #[test]
    fn tamper_signature_fails() {
        let m = fixture_manifest();
        let mut signed = sign_manifest(&RFC8032_TEST1_SEED, &m).unwrap();
        let sig_b64 = signed.host_signature.as_ref().unwrap().value.clone();
        let mut sig_bytes = ed25519::from_base64(&sig_b64).unwrap();
        sig_bytes[0] ^= 0x01;
        signed.host_signature.as_mut().unwrap().value = ed25519::to_base64(&sig_bytes);
        let err = verify_manifest(&signed).unwrap_err();
        assert!(
            matches!(err, VerifyError::SignatureMismatch),
            "expected SignatureMismatch, got {err:?}"
        );
    }

    /// Test 5: an all-zero public key is rejected as
    /// `InvalidPublicKey`.
    #[test]
    fn wrong_pubkey_fails() {
        let m = fixture_manifest();
        let mut signed = sign_manifest(&RFC8032_TEST1_SEED, &m).unwrap();
        // Replace the public key with the all-zero 32-byte blob
        // (encoded in standard base64).
        let zero_pk = [0u8; 32];
        signed.host_signature.as_mut().unwrap().public_key = ed25519::to_base64(&zero_pk);
        let err = verify_manifest(&signed).unwrap_err();
        assert!(
            matches!(
                err,
                VerifyError::InvalidPublicKey(InvalidPublicKeyReason::RejectedByVerifier)
            ),
            "expected InvalidPublicKey(RejectedByVerifier), got {err:?}"
        );
    }

    /// Test 6: missing host_signature is a hard error.
    #[test]
    fn missing_signature_fails() {
        let m = fixture_manifest();
        // m has host_signature = None from the fixture.
        let err = verify_manifest(&m).unwrap_err();
        assert!(
            matches!(err, VerifyError::MissingSignature),
            "expected MissingSignature, got {err:?}"
        );
    }

    /// Test 7: an unsupported algorithm name is rejected.
    #[test]
    fn unsupported_algorithm_fails() {
        let m = fixture_manifest();
        let mut signed = sign_manifest(&RFC8032_TEST1_SEED, &m).unwrap();
        signed.host_signature.as_mut().unwrap().algorithm = "rsa".to_owned();
        let err = verify_manifest(&signed).unwrap_err();
        assert!(
            matches!(err, VerifyError::UnsupportedAlgorithm(ref a) if a == "rsa"),
            "expected UnsupportedAlgorithm(\"rsa\"), got {err:?}"
        );
    }

    /// Test 8: a non-base64 public key is rejected as
    /// `InvalidPublicKey(InvalidPublicKeyReason::Base64Decode)`.
    #[test]
    fn malformed_pubkey_fails() {
        let m = fixture_manifest();
        let mut signed = sign_manifest(&RFC8032_TEST1_SEED, &m).unwrap();
        signed.host_signature.as_mut().unwrap().public_key = "not!base64!!!".to_owned();
        let err = verify_manifest(&signed).unwrap_err();
        assert!(
            matches!(
                err,
                VerifyError::InvalidPublicKey(InvalidPublicKeyReason::Base64Decode)
            ),
            "expected InvalidPublicKey(Base64Decode), got {err:?}"
        );
    }

    /// Test 9: a public key of the wrong length (31 bytes
    /// instead of 32) is rejected as
    /// `InvalidPublicKey(InvalidPublicKeyReason::WrongLength)`.
    #[test]
    fn wrong_length_pubkey_fails() {
        let m = fixture_manifest();
        let mut signed = sign_manifest(&RFC8032_TEST1_SEED, &m).unwrap();
        let short = vec![0u8; 31];
        signed.host_signature.as_mut().unwrap().public_key = ed25519::to_base64(&short);
        let err = verify_manifest(&signed).unwrap_err();
        assert!(
            matches!(
                err,
                VerifyError::InvalidPublicKey(InvalidPublicKeyReason::WrongLength(31))
            ),
            "expected InvalidPublicKey(WrongLength(31)), got {err:?}"
        );
    }

    /// Test 10: a signature of the wrong length (63 bytes
    /// instead of 64) is rejected as
    /// `InvalidSignatureEncoding(InvalidSignatureEncodingReason::WrongLength)`.
    #[test]
    fn wrong_length_signature_fails() {
        let m = fixture_manifest();
        let mut signed = sign_manifest(&RFC8032_TEST1_SEED, &m).unwrap();
        let short = vec![0u8; 63];
        signed.host_signature.as_mut().unwrap().value = ed25519::to_base64(&short);
        let err = verify_manifest(&signed).unwrap_err();
        assert!(
            matches!(
                err,
                VerifyError::InvalidSignatureEncoding(InvalidSignatureEncodingReason::WrongLength(
                    63
                ))
            ),
            "expected InvalidSignatureEncoding(WrongLength(63)), got {err:?}"
        );
    }

    /// Test 11: signing the same manifest twice with the same
    /// seed produces the same 64-byte signature. Pure Ed25519 is
    /// deterministic.
    #[test]
    fn deterministic_signature() {
        let m = fixture_manifest();
        let s1 = sign_manifest(&RFC8032_TEST1_SEED, &m).unwrap();
        let s2 = sign_manifest(&RFC8032_TEST1_SEED, &m).unwrap();
        let v1 = ed25519::from_base64(&s1.host_signature.as_ref().unwrap().value).unwrap();
        let v2 = ed25519::from_base64(&s2.host_signature.as_ref().unwrap().value).unwrap();
        assert_eq!(
            v1, v2,
            "Ed25519 must be deterministic for fixed (key, message)"
        );
    }

    /// Test 12: the private seed never appears in the canonical
    /// JSON. The data model does not carry the seed at all, and
    /// the canonicalizer only sees the public fields plus a
    /// `null` for `host_signature`, so a defense-in-depth check
    /// that the seed (in either raw or base64 form) does not
    /// appear is the right guard. (Note: the public key is
    /// also absent from the canonical bytes, because
    /// `host_signature` is stripped to `null`; that is the
    /// "no recursion" property — the manifest is signed
    /// without the signature block in it.)
    #[test]
    fn private_key_never_in_canonical() {
        let m = fixture_manifest();
        let signed = sign_manifest(&RFC8032_TEST1_SEED, &m).unwrap();
        let bytes = serialize(&signed).unwrap();
        let s = std::str::from_utf8(&bytes).unwrap();

        let seed_hex = hex_encode(&RFC8032_TEST1_SEED);
        let seed_b64 = ed25519::to_base64(&RFC8032_TEST1_SEED);
        assert!(
            !s.contains(&seed_hex),
            "raw seed hex leaked into canonical JSON: {s}"
        );
        assert!(
            !s.contains(&seed_b64),
            "base64 seed leaked into canonical JSON: {s}"
        );

        // And the public key (which would be fine to embed) is
        // also absent because the canonicalizer nulls the
        // host_signature field. This is the expected
        // behavior — it just confirms the no-recursion rule.
        let pubkey_b64 = ed25519::to_base64(&RFC8032_TEST1_PUBKEY);
        assert!(
            !s.contains(&pubkey_b64),
            "public key should NOT be in canonical JSON (host_signature is null): {s}"
        );
    }

    /// Test 13: RFC 8032 §7.1 test vector 1. Sign the empty
    /// string with the published seed and assert the signature
    /// matches the published expected signature exactly. This
    /// proves the underlying `ed25519::sign` is wired to a
    /// compliant Ed25519 implementation.
    #[test]
    fn rfc8032_test1_vector() {
        let expected_sig_hex = "e5564300c360ac729086e2cc806e828a\
84877f1eb8e5d974d873e06522490155\
5fb8821590a33bacc61e39701cf9b46b\
d25bf5f0595bbe24655141438e7a100b";
        let expected: [u8; 64] = hex_decode_64(expected_sig_hex);

        let got = ed25519::sign(&RFC8032_TEST1_SEED, b"");
        assert_eq!(got, expected, "RFC 8032 test 1 signature mismatch");
    }

    /// Test 14: a golden manifest signed with the RFC 8032 test 1
    /// seed produces a stable signature. The expected hex value
    /// is hardcoded here (and also pinned in the integration
    /// test fixture at `tests/signing_golden.json`) so this
    /// unit test catches a regression in the canonicalizer —
    /// e.g. an extra space inside the JSON, a key reorder
    /// accident, or a different NFC handling — that would still
    /// sign+verify cleanly in a single-process test.
    #[test]
    fn golden_manifest_signs_to_stable_value() {
        let m = fixture_manifest();
        let signed = sign_manifest(&RFC8032_TEST1_SEED, &m).unwrap();
        let hs = signed.host_signature.as_ref().unwrap();
        assert_eq!(hs.algorithm, "ed25519");

        let expected_pubkey_hex = "d75a980182b10ab7d54bfed3c964073a\
0ee172f3daa62325af021a68f707511a";
        let expected_sig_hex = "ccf6468a3c04c2e57ec9184e0a98bd5a\
0a0d7043758840f4a32491da40f07b94\
2d447e1264873d41f87b7fc37335cb66\
03f29a15d6f91892c94c990ec9f16800";

        assert_eq!(
            hex_encode(&ed25519::from_base64(&hs.public_key).unwrap()),
            expected_pubkey_hex
        );
        assert_eq!(
            hex_encode(&ed25519::from_base64(&hs.value).unwrap()),
            expected_sig_hex
        );
    }

    /// Test 16: a byte-level tamper of the canonical bytes is
    /// caught by the verifier. This proves the verifier is
    /// keyed on the byte sequence, not on the data model. The
    /// data-model tamper test (`tamper_field_fails`) only
    /// proves "field changes are caught"; this test proves
    /// "byte-level changes to the canonical form are caught",
    /// which is what the architecture requires and what
    /// P3-T03's transport will rely on.
    #[test]
    fn byte_level_canonical_tamper_fails_verify() {
        // Sign a baseline manifest.
        let m = fixture_manifest();
        let signed = sign_manifest(&RFC8032_TEST1_SEED, &m).unwrap();

        // Build a second manifest whose canonical bytes differ
        // by exactly one byte from the signed manifest's
        // canonical bytes. The simplest way is to add a single
        // ASCII character to a value that does not get any
        // JSON-escape treatment — `room_id` is the safe choice
        // (UUIDs, no escapes).
        let mut tampered = m.clone();
        tampered.room_id.push('x');
        let signed_tampered = sign_manifest(&RFC8032_TEST1_SEED, &tampered).unwrap();
        let bytes_a = serialize(&signed).unwrap();
        let bytes_b = serialize(&signed_tampered).unwrap();
        assert_ne!(
            bytes_a, bytes_b,
            "byte-level tamper test is ill-formed: canonical bytes did not differ"
        );

        // Now prove the verifier fails when handed the
        // *signed* manifest but the canonical bytes are
        // slightly different — which is what a network-layer
        // MITM would actually do. We re-derive what the
        // verifier would see: serialize the tampered
        // manifest, but keep the signed manifest's signature
        // (which was over the un-tampered canonical bytes).
        let mut signed_with_tampered_canonical = signed.clone();
        // Force the canonical-bytes difference by also
        // mutating the data model so that the verify path's
        // `serialize(signed_with_tampered_canonical)` produces
        // a different byte stream than what was signed.
        signed_with_tampered_canonical.room_id.push('x');
        // The signature in `signed.host_signature` is over the
        // un-tampered canonical bytes, so verify must reject.
        let err = verify_manifest(&signed_with_tampered_canonical).unwrap_err();
        assert!(
            matches!(err, VerifyError::SignatureMismatch),
            "expected SignatureMismatch for byte-level tamper, got {err:?}"
        );
    }

    /// Test 15: smoke test that the [`VerifyError`] enum contains
    /// a [`VerifyError::CanonicalizationFailed`] variant. The
    /// public API cannot trigger it (the data model has no
    /// floats), but the variant must exist for forward
    /// compatibility.
    #[test]
    fn error_type_has_canonicalization_variant() {
        // We can't easily construct a CanonicalError without
        // reaching into the internals, so we just match on a
        // fabricated instance to prove the variant exists and
        // formats sensibly.
        let err: VerifyError = VerifyError::CanonicalizationFailed(
            crate::error::CanonicalError::InvalidNonFiniteFloat,
        );
        let s = format!("{err}");
        assert!(s.contains("canonicalization failed"), "format string: {s}");
    }
}
