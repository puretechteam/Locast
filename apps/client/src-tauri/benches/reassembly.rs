//! P8-T06 bench: chunk reassembly (ARCHITECTURE 28.10 hot path 5).
//!
//! 16 MiB of chunk files (64 x 256 KiB) are staged in a fresh library
//! root and `assemble_and_finalize` stitches them together, verifies the
//! blake3 root, and atomically moves the result into the library. Staging
//! is outside the timed region. The architecture's 1 GiB random-arrival
//! scenario is scaled down so the bench stays quick; the function reads
//! chunks in index order, so arrival order is not a factor here.

use std::time::{Duration, Instant};

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use locast_client_lib::core::paths::incomplete_chunk_path;
use locast_client_lib::transfer::assemble::assemble_and_finalize;
use sha2::{Digest, Sha256};
use tempfile::TempDir;

const CHUNK: usize = 262_144;
const CHUNKS: u32 = 64;
const DOWNLOAD_ID: &str = "01234567-89ab-cdef-0123-456789abcdef";

/// Deterministic, incompressible-ish bytes (xorshift64).
fn fixture_bytes() -> Vec<u8> {
    let mut state = 0x9E37_79B9_7F4A_7C15_u64;
    let mut out = Vec::with_capacity(CHUNK * CHUNKS as usize);
    while out.len() < CHUNK * CHUNKS as usize {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.extend_from_slice(&state.to_le_bytes());
    }
    out
}

fn stage_chunks(root: &std::path::Path, data: &[u8]) {
    for (i, chunk) in data.chunks(CHUNK).enumerate() {
        let path = incomplete_chunk_path(root, DOWNLOAD_ID, i as u32).expect("chunk path");
        std::fs::create_dir_all(path.parent().expect("chunk parent")).expect("mkdir chunk dir");
        std::fs::write(path, chunk).expect("write chunk");
    }
}

fn bench_reassembly(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime");

    let data = fixture_bytes();
    let sha256 = hex::encode(Sha256::digest(&data));
    let blake3 = blake3::hash(&data).to_hex().to_string();
    let lengths: Vec<(u32, u32)> = (0..CHUNKS).map(|i| (i, CHUNK as u32)).collect();

    let mut group = c.benchmark_group("reassembly");
    group.sample_size(10);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(10));
    group.throughput(Throughput::Bytes(data.len() as u64));

    group.bench_function("assemble_16mib", |b| {
        b.to_async(&rt).iter_custom(|iters| {
            let (data, sha256, blake3, lengths) = (&data, &sha256, &blake3, &lengths);
            async move {
                let mut total = Duration::ZERO;
                for _ in 0..iters {
                    let root = TempDir::new().expect("library root");
                    stage_chunks(root.path(), data);
                    let start = Instant::now();
                    let res = assemble_and_finalize(
                        root.path(),
                        DOWNLOAD_ID,
                        sha256,
                        "bench.mkv",
                        blake3,
                        lengths,
                        data.len() as u64,
                    )
                    .await
                    .expect("assemble");
                    total += start.elapsed();
                    assert_eq!(&res.blake3, blake3);
                }
                total
            }
        });
    });

    group.finish();
}

criterion_group!(benches, bench_reassembly);
criterion_main!(benches);
