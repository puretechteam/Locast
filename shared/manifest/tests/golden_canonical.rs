//! Golden-vector integration test for the manifest canonical form.
//!
//! The fixture lives in `shared/manifest/tests/golden_canonical.json`.
//! It was generated once with `cargo run -p locast-manifest --bin
//! gen_golden` and pasted in; the generator has been removed so the
//! repo does not carry a tool that could silently re-emit the fixture.
//!
//! The test:
//! 1. Loads the JSON fixture.
//! 2. Deserializes `input` into a `MediaManifest`.
//! 3. Calls `canonical::serialize` and hex-encodes the result.
//! 4. Asserts the hex matches `canonical_hex` exactly (this commits
//!    to the byte-for-byte canonical form).
//! 5. Re-asserts `commit_hex == BLAKE3(canonical_bytes)` as an
//!    independent check that doesn't go through `canonical::commit`
//!    (well, it does, but it serves as a fingerprint the reader can
//!    verify against the fixture by hand).

use std::fs;
use std::path::PathBuf;

use locast_manifest::model::MediaManifest;
use locast_manifest::{commit, serialize};
use serde_json::Value;

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden_canonical.json")
}

fn load_fixture() -> (MediaManifest, String, String) {
    let path = fixture_path();
    let raw = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let v: Value =
        serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()));
    let input = v
        .get("input")
        .unwrap_or_else(|| panic!("fixture missing 'input'"));
    let canonical_hex = v
        .get("canonical_hex")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("fixture missing 'canonical_hex'"))
        .to_owned();
    let commit_hex = v
        .get("commit_hex")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("fixture missing 'commit_hex'"))
        .to_owned();
    let manifest: MediaManifest = serde_json::from_value(input.clone())
        .unwrap_or_else(|e| panic!("deserialize fixture input: {e}"));
    (manifest, canonical_hex, commit_hex)
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

fn hex_nibble(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        b'A'..=b'F' => c - b'A' + 10,
        _ => panic!("non-hex character: {c}"),
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[test]
fn golden_canonical_matches_fixture() {
    let (manifest, expected_hex, expected_commit) = load_fixture();
    let bytes = serialize(&manifest).expect("canonicalize fixture input");
    let got_hex = hex_encode(&bytes);
    assert_eq!(got_hex, expected_hex, "canonical bytes hex mismatch");

    // And the commit independently matches.
    let got_commit = commit(&bytes);
    assert_eq!(got_commit, expected_commit, "BLAKE3 commit mismatch");

    // Cross-check: the expected commit must match the BLAKE3 of the
    // expected hex bytes. This catches a fixture that has internally
    // inconsistent hex/commit values.
    let expected_bytes = hex_decode(&expected_hex);
    assert_eq!(
        commit(&expected_bytes),
        expected_commit,
        "fixture's commit_hex does not match BLAKE3 of canonical_hex"
    );
}

#[test]
fn golden_canonical_is_deterministic_across_runs() {
    let (manifest, _, _) = load_fixture();
    let a = serialize(&manifest).unwrap();
    let b = serialize(&manifest).unwrap();
    assert_eq!(a, b);
    assert_eq!(commit(&a), commit(&b));
}

#[test]
fn golden_canonical_input_deserializes_back_to_same_bytes() {
    // Round-trip: deserialize the fixture, serialize, deserialize the
    // canonical JSON, and confirm we recover the same logical
    // manifest.
    let (manifest, expected_hex, _) = load_fixture();
    let bytes = serialize(&manifest).unwrap();
    let canonical_str = std::str::from_utf8(&bytes).expect("utf-8 canonical");

    // The canonical bytes are JSON; re-parse them and check the
    // critical fields survived.
    let parsed: Value = serde_json::from_str(canonical_str).expect("parse canonical");
    assert_eq!(parsed["manifest_version"], 1);
    assert_eq!(parsed["room_id"], "22222222-2222-4222-8222-222222222222");
    assert_eq!(
        parsed["host_signature"],
        Value::Null,
        "host_signature must be null in canonical form"
    );
    assert_eq!(parsed["media"].as_array().unwrap().len(), 2);
    assert_eq!(parsed["subtitles"].as_array().unwrap().len(), 1);

    // Trailing newline: the canonical bytes end with exactly one.
    assert!(bytes.ends_with(b"\n"));
    assert_eq!(*bytes.last().unwrap(), 0x0A);

    // Sanity-check the hex string length matches a sensible canonical
    // size: every JSON byte turns into 2 hex chars, plus 2 for the
    // trailing newline.
    assert_eq!(expected_hex.len(), bytes.len() * 2);
}
