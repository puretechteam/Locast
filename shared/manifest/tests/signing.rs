//! Integration tests for the manifest signing pipeline.
//!
//! Each test exercises a property of [`locast_manifest::sign_manifest`]
//! and [`locast_manifest::verify_manifest`] at the public API
//! boundary, in addition to the unit tests inside `src/signing.rs`.
//! The golden vector check pins the produced signature for a
//! representative manifest to a stable, byte-exact value so any
//! future change to the canonicalizer (which would shift the
//! signed bytes) is caught immediately.

use std::fs;
use std::path::PathBuf;

use locast_crypto::ed25519;
use locast_manifest::model::{Dimensions, MediaEntry, MediaManifest, Source};
use locast_manifest::{serialize, sign_manifest, verify_manifest, VerifyError};
use serde_json::Value;

const RFC8032_TEST1_SEED: [u8; 32] = [
    0x9d, 0x61, 0xb1, 0x9d, 0xef, 0xfd, 0x5a, 0x60, 0xba, 0x84, 0x4a, 0xf4, 0x92, 0xec, 0x2c, 0xc4,
    0x44, 0x49, 0xc5, 0x69, 0x7b, 0x32, 0x69, 0x19, 0x70, 0x3b, 0xac, 0x03, 0x1c, 0xae, 0x7f, 0x60,
];

const RFC8032_TEST1_PUBKEY: [u8; 32] = [
    0xd7, 0x5a, 0x98, 0x01, 0x82, 0xb1, 0x0a, 0xb7, 0xd5, 0x4b, 0xfe, 0xd3, 0xc9, 0x64, 0x07, 0x3a,
    0x0e, 0xe1, 0x72, 0xf3, 0xda, 0xa6, 0x23, 0x25, 0xaf, 0x02, 0x1a, 0x68, 0xf7, 0x07, 0x51, 0x1a,
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

fn hex_nibble(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        b'A'..=b'F' => c - b'A' + 10,
        _ => panic!("non-hex character: {c}"),
    }
}

fn hex_decode(s: &str) -> Vec<u8> {
    assert!(s.len() % 2 == 0, "hex string must be even length");
    let mut out = Vec::with_capacity(s.len() / 2);
    for pair in s.as_bytes().chunks(2) {
        let hi = hex_nibble(pair[0]);
        let lo = hex_nibble(pair[1]);
        out.push((hi << 4) | lo);
    }
    out
}

fn golden_fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("signing_golden.json")
}

struct GoldenFixture {
    #[allow(dead_code)]
    seed: [u8; 32],
    pubkey: [u8; 32],
    signature: [u8; 64],
    input: MediaManifest,
}

fn load_golden() -> GoldenFixture {
    let path = golden_fixture_path();
    let raw = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let v: Value =
        serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()));

    let seed_hex = v
        .get("seed_hex")
        .and_then(Value::as_str)
        .expect("fixture missing seed_hex");
    let pubkey_hex = v
        .get("expected_pubkey_hex")
        .and_then(Value::as_str)
        .expect("fixture missing expected_pubkey_hex");
    let sig_hex = v
        .get("expected_signature_hex")
        .and_then(Value::as_str)
        .expect("fixture missing expected_signature_hex");
    let input_json = v.get("input").cloned().expect("fixture missing input");

    let seed = {
        let v = hex_decode(seed_hex);
        let mut out = [0u8; 32];
        out.copy_from_slice(&v);
        out
    };
    let pubkey = {
        let v = hex_decode(pubkey_hex);
        let mut out = [0u8; 32];
        out.copy_from_slice(&v);
        out
    };
    let signature = {
        let v = hex_decode(sig_hex);
        assert_eq!(v.len(), 64, "expected 64-byte signature");
        let mut out = [0u8; 64];
        out.copy_from_slice(&v);
        out
    };
    let input: MediaManifest =
        serde_json::from_value(input_json).expect("fixture input deserialization failed");

    GoldenFixture {
        seed,
        pubkey,
        signature,
        input,
    }
}

#[test]
fn golden_manifest_signature_matches_fixture() {
    let g = load_golden();
    let signed = sign_manifest(&g.seed, &g.input).expect("signing should succeed");
    let hs = signed
        .host_signature
        .as_ref()
        .expect("host_signature populated");
    assert_eq!(hs.algorithm, "ed25519");
    let sig = ed25519::from_base64(&hs.value).expect("signature base64");
    let pk = ed25519::from_base64(&hs.public_key).expect("public key base64");
    assert_eq!(sig, g.signature, "signature drift from golden vector");
    assert_eq!(pk, g.pubkey, "public key drift from golden vector");
}

#[test]
fn golden_manifest_round_trips_through_verify() {
    let g = load_golden();
    let signed = sign_manifest(&g.seed, &g.input).expect("signing should succeed");
    verify_manifest(&signed).expect("golden manifest must verify");
}

#[test]
fn golden_canonical_bytes_are_signed_exactly() {
    // Signing must be over `serialize(&input)?` exactly. Confirm
    // by independently signing the canonical bytes via
    // locast_crypto::ed25519::sign and comparing to the
    // host_signature value produced by sign_manifest.
    let g = load_golden();
    let canonical = serialize(&g.input).expect("canonicalize input");
    let independent = ed25519::sign(&g.seed, &canonical);
    assert_eq!(
        independent, g.signature,
        "golden fixture signature is not the Ed25519 signature over the canonical bytes"
    );
}

#[test]
fn integration_roundtrip_with_fixture_manifest() {
    let m = fixture_manifest();
    let signed = sign_manifest(&RFC8032_TEST1_SEED, &m).unwrap();
    verify_manifest(&signed).expect("freshly signed manifest must verify");
    let hs = signed.host_signature.as_ref().unwrap();
    // Public key in the wire signature must be the RFC 8032 test 1
    // pubkey for the seed we signed with.
    let pk = ed25519::from_base64(&hs.public_key).unwrap();
    assert_eq!(pk, RFC8032_TEST1_PUBKEY.to_vec());
}

#[test]
fn integration_tamper_after_signing_fails() {
    let m = fixture_manifest();
    let mut signed = sign_manifest(&RFC8032_TEST1_SEED, &m).unwrap();
    signed.room_id = "attacker-room".to_owned();
    let err = verify_manifest(&signed).unwrap_err();
    assert!(matches!(err, VerifyError::SignatureMismatch));
}

#[test]
fn integration_host_signature_uses_ed25519_algorithm_name() {
    let m = fixture_manifest();
    let signed = sign_manifest(&RFC8032_TEST1_SEED, &m).unwrap();
    let hs = signed.host_signature.as_ref().unwrap();
    assert_eq!(hs.algorithm, "ed25519");
}

#[test]
fn integration_signature_blob_is_64_bytes_after_base64_decode() {
    let m = fixture_manifest();
    let signed = sign_manifest(&RFC8032_TEST1_SEED, &m).unwrap();
    let hs = signed.host_signature.as_ref().unwrap();
    let sig = ed25519::from_base64(&hs.value).unwrap();
    assert_eq!(sig.len(), 64);
}

#[test]
fn integration_pubkey_blob_is_32_bytes_after_base64_decode() {
    let m = fixture_manifest();
    let signed = sign_manifest(&RFC8032_TEST1_SEED, &m).unwrap();
    let hs = signed.host_signature.as_ref().unwrap();
    let pk = ed25519::from_base64(&hs.public_key).unwrap();
    assert_eq!(pk.len(), 32);
}
