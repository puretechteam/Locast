//! P8-T06 bench: manifest serialization (ARCHITECTURE 28.10 hot path 3).
//!
//! A synthetic manifest describing a 10 GiB file (40 960 chunk hashes of
//! 256 KiB) is serialized to canonical bytes and committed with blake3.
//! Gate from the architecture: serialize + blake3 < 200 ms.

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use locast_manifest::{commit, serialize, MediaEntry, MediaManifest, Source};

const CHUNK_SIZE: u32 = 262_144;
const TEN_GIB: u64 = 10 * 1024 * 1024 * 1024;

fn synthetic_manifest() -> MediaManifest {
    let total_chunks = (TEN_GIB / u64::from(CHUNK_SIZE)) as u32;
    let chunk_hashes = (0..total_chunks).map(|i| format!("{i:064x}")).collect();
    MediaManifest {
        manifest_version: 1,
        room_id: "bench-room".to_string(),
        media: vec![MediaEntry {
            id: "media-0".to_string(),
            filename: "ten-gib.mkv".to_string(),
            sha256: "a".repeat(64),
            blake3: "b".repeat(64),
            size_bytes: TEN_GIB,
            mime: "video/x-matroska".to_string(),
            duration_ms: 7_200_000,
            dimensions: None,
            codecs: None,
            sources: vec![Source {
                peer_id: "c".repeat(64),
                url_hint: None,
                priority: 0,
                chunk_size: CHUNK_SIZE,
                total_chunks,
                chunk_hashes,
            }],
        }],
        subtitles: Vec::new(),
        created_at: 1_700_000_000_000,
        host_signature: None,
    }
}

fn bench_manifest(c: &mut Criterion) {
    let manifest = synthetic_manifest();

    let mut group = c.benchmark_group("manifest");
    group.sample_size(20);

    group.bench_function("serialize_commit_10gib", |b| {
        b.iter(|| {
            let bytes = serialize(black_box(&manifest)).expect("serialize");
            black_box(commit(&bytes))
        });
    });

    group.finish();
}

criterion_group!(benches, bench_manifest);
criterion_main!(benches);
