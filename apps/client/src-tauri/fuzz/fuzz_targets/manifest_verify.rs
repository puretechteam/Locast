//! `manifest_verify` - a cargo-fuzz target for the manifest trust path.
//!
//! The signed `MediaManifest` reaches the server (`MANIFEST_PUBLISH`)
//! and every viewer (`MANIFEST_PUBLISHED` / `MANIFEST_RESPONSE`) as
//! JSON inside the message envelope, where it is deserialized with
//! serde into `locast_manifest::MediaManifest` and then passed to
//! `verify_manifest`. This target does the same from raw bytes:
//! `serde_json::from_slice::<MediaManifest>` (the same JSON format,
//! the same serde derive), then on `Ok` the canonical serializer and
//! the verifier. `decode_public_key` / `decode_signature` are private
//! to `signing.rs`; they are reached through `verify_manifest`.
//!
//! Properties checked:
//! - nothing panics, whatever the manifest holds (huge numbers, odd
//!   unicode, deep or empty vectors, junk base64 in the signature);
//! - `serialize` is deterministic (two calls give identical bytes);
//! - the canonical form is a fixed point: parsing the canonical bytes
//!   and serializing again gives the same bytes;
//! - `verify_manifest` returning `Ok` on arbitrary input is not
//!   asserted against (a corpus seed may legitimately be signed), but
//!   it must agree with itself across two calls;
//! - sign/verify round trip: signing the parsed manifest with a fixed
//!   test seed always verifies, and the signature does not change the
//!   canonical bytes (`host_signature` is not part of the signed data);
//! - tamper rejection: changing a signed field (`created_at`) of the
//!   freshly signed manifest makes `verify_manifest` fail.
//!
//! The corpus in `corpus/manifest_verify/` is committed: manifests
//! built from the crate's own golden test fixtures.
#![no_main]

use libfuzzer_sys::fuzz_target;
use locast_manifest::{serialize, sign_manifest, verify_manifest, MediaManifest};

/// Fixed test seed (RFC 8032 section 7.1 test vector 1), the same one
/// the crate's own signing tests use. Not a secret.
const TEST_SEED: [u8; 32] = [
    0x9d, 0x61, 0xb1, 0x9d, 0xef, 0xfd, 0x5a, 0x60, 0xba, 0x84, 0x4a, 0xf4, 0x92, 0xec, 0x2c, 0xc4,
    0x44, 0x49, 0xc5, 0x69, 0x7b, 0x32, 0x69, 0x19, 0x70, 0x3b, 0xac, 0x03, 0x1c, 0xae, 0x7f, 0x60,
];

fuzz_target!(|data: &[u8]| {
    let Ok(manifest) = serde_json::from_slice::<MediaManifest>(data) else {
        return;
    };

    // The verifier on untrusted input: must return, never panic. Its
    // outcome is only checked for self-consistency.
    let first = verify_manifest(&manifest).is_ok();
    assert_eq!(first, verify_manifest(&manifest).is_ok());

    let Ok(canonical) = serialize(&manifest) else {
        return;
    };
    let again = serialize(&manifest).expect("serialize succeeded once");
    assert_eq!(canonical, again, "canonical form is not deterministic");

    // Fixed point: the canonical bytes parse back to a manifest whose
    // canonical form is the same bytes.
    let reparsed: MediaManifest =
        serde_json::from_slice(&canonical).expect("canonical bytes must parse");
    assert_eq!(
        serialize(&reparsed).expect("reparsed manifest must serialize"),
        canonical,
        "canonical form is not a fixed point"
    );

    // Sign/verify round trip with a known seed.
    let signed = sign_manifest(&TEST_SEED, &manifest).expect("serialize succeeded, so sign must");
    assert!(
        verify_manifest(&signed).is_ok(),
        "freshly signed manifest does not verify"
    );
    assert_eq!(
        serialize(&signed).expect("signed manifest must serialize"),
        canonical,
        "host_signature leaked into the canonical bytes"
    );

    // Tamper rejection: changing a signed field must invalidate the
    // signature, so a verifier that accepts everything cannot pass.
    let mut tampered = signed.clone();
    tampered.created_at = tampered.created_at.wrapping_add(1);
    assert!(
        verify_manifest(&tampered).is_err(),
        "manifest with a changed signed field still verifies"
    );
});
