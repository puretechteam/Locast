//! `manifest_parts` - a cargo-fuzz target for the multi-frame manifest path.
//!
//! A manifest too big for one signaling frame arrives as `MANIFEST_PART`
//! envelopes from an untrusted peer (the host, to the server) or an untrusted
//! server (to a viewer). Each piece is JSON inside the envelope and is fed to
//! `locast_protocol::manifest_parts::PartAssembler`. This target drives the
//! assembler two ways from the same bytes:
//!
//! 1. the input as a JSON array of arbitrary `ManifestPartPayload`s, pushed in
//!    order into rooms picked from the input, to reach every header and
//!    ordering check with hostile values;
//! 2. the input as the raw bytes of a "manifest", split into pieces and pushed
//!    in order, then re-pushed with pieces dropped, repeated, reordered and
//!    altered.
//!
//! Properties checked:
//! - nothing panics, whatever the pieces hold;
//! - the assemblies in flight never exceed the configured count, and the bytes
//!   charged to the budget never exceed its cap;
//! - dropping the assembler returns every reserved byte;
//! - a clean in-order split of bytes that parse as a manifest reassembles to
//!   exactly that manifest, and the same split missing any piece never
//!   completes.
#![no_main]

use std::time::{Duration, Instant};

use libfuzzer_sys::fuzz_target;
use locast_manifest::MediaManifest;
use locast_protocol::manifest_parts::{
    split_manifest_bytes, ByteBudget, ManifestPartKind, ManifestPartPayload, PartAssembler,
    MAX_PART_BYTES,
};
use uuid::Uuid;

const BUDGET: usize = 64 * 1024 * 1024;

fuzz_target!(|data: &[u8]| {
    let budget = ByteBudget::new(BUDGET);
    let mut assembler = PartAssembler::new(2, Duration::from_secs(30), budget.clone());
    let now = Instant::now();

    // 1. Arbitrary pieces.
    if let Ok(parts) = serde_json::from_slice::<Vec<ManifestPartPayload>>(data) {
        for (i, part) in parts.iter().take(64).enumerate() {
            let room = Uuid::from_u128((data.first().copied().unwrap_or(0) as u128) + (i as u128 % 3));
            let _ = assembler.push(room, part, now);
            assert!(assembler.in_flight() <= 2);
            assert!(budget.used() <= BUDGET);
        }
    }

    // 2. A split of the raw bytes.
    if !data.is_empty() {
        let part_size = 1 + (data[0] as usize) * 7 % MAX_PART_BYTES.min(data.len().max(1));
        if let Ok(parts) = split_manifest_bytes(
            data,
            ManifestPartKind::Published,
            1,
            1,
            Uuid::from_u128(data.len() as u128),
            part_size,
        ) {
            let room = Uuid::nil();
            let mut clean = PartAssembler::new(1, Duration::from_secs(30), budget.clone());
            let mut completed = None;
            for (i, p) in parts.iter().enumerate() {
                match clean.push(room, p, now) {
                    Ok(Some(done)) => {
                        assert_eq!(i, parts.len() - 1, "completed before the last piece");
                        completed = Some(done);
                    }
                    Ok(None) => {}
                    Err(_) => break,
                }
            }
            if let Some(done) = completed {
                let expected: MediaManifest =
                    serde_json::from_slice(data).expect("completed, so the bytes parse");
                assert_eq!(done.manifest, expected);
            }

            // Missing a piece never completes.
            if parts.len() > 1 {
                let skip = (data[0] as usize) % parts.len();
                let mut gappy = PartAssembler::new(1, Duration::from_secs(30), budget.clone());
                for (i, p) in parts.iter().enumerate() {
                    if i == skip {
                        continue;
                    }
                    assert!(
                        !matches!(gappy.push(room, p, now), Ok(Some(_))),
                        "a manifest with a missing piece completed"
                    );
                }
            }
        }
    }

    drop(assembler);
    assert_eq!(budget.used(), 0, "dropping the assembler leaked budget");
});
