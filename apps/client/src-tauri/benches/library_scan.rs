//! P8-T06 bench: library scan (ARCHITECTURE 28.10 hot path 1).
//!
//! A fixture of 1000 small content-addressed files is built once. Two
//! benches run against it:
//!
//! * `library_scan/full_1000_files` - first scan into a fresh, empty
//!   database (hash every file, insert every row).
//! * `library_scan/incremental_1000_files` - re-scan of a library whose
//!   rows already exist (the idempotent no-op branch).
//!
//! The database is opened outside the timed region; only `scan` is timed.
//! Gate from the architecture: full < 30 s, incremental < 2 s.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use criterion::{criterion_group, criterion_main, Criterion};
use locast_client_lib::library::scan::scan;
use locast_client_lib::storage::Storage;
use sha2::{Digest, Sha256};
use tempfile::TempDir;

const FILE_COUNT: u32 = 1000;
const FILE_BYTES: usize = 1024;

/// Write `FILE_COUNT` files under `<root>/library/<aa>/<bb>/<sha>/<name>`.
fn build_library(root: &Path) {
    for i in 0..FILE_COUNT {
        let mut bytes = Vec::with_capacity(FILE_BYTES);
        bytes.extend_from_slice(&i.to_le_bytes());
        for j in 4..FILE_BYTES as u32 {
            bytes.push(((i.wrapping_mul(7) ^ j) & 0xFF) as u8);
        }
        let sha = hex::encode(Sha256::digest(&bytes));
        let dir = root
            .join("library")
            .join(&sha[0..2])
            .join(&sha[2..4])
            .join(&sha);
        std::fs::create_dir_all(&dir).expect("mkdir fixture dir");
        std::fs::write(dir.join(format!("Movie{i:04}.mkv")), &bytes).expect("write fixture");
    }
}

async fn fresh_storage(dir: &TempDir) -> Storage {
    let db: PathBuf = dir.path().join(format!("index-{}.sqlite", uuid_like()));
    Storage::open(&db).await.expect("storage opens")
}

/// Unique-enough suffix so every iteration gets its own database file.
fn uuid_like() -> u128 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    u128::from(N.fetch_add(1, Ordering::Relaxed))
}

fn bench_scan(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime");

    let library = TempDir::new().expect("library tempdir");
    build_library(library.path());
    let db_dir = TempDir::new().expect("db tempdir");

    let mut group = c.benchmark_group("library_scan");
    group.sample_size(10);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(10));

    group.bench_function("full_1000_files", |b| {
        b.to_async(&rt).iter_custom(|iters| {
            let root = library.path().to_path_buf();
            let db_dir = &db_dir;
            async move {
                let mut total = Duration::ZERO;
                for _ in 0..iters {
                    let storage = fresh_storage(db_dir).await;
                    let start = Instant::now();
                    let res = scan(&storage, &root).await.expect("scan");
                    total += start.elapsed();
                    assert_eq!(res.files_scanned, u64::from(FILE_COUNT));
                }
                total
            }
        });
    });

    let warm = rt.block_on(async {
        let storage = fresh_storage(&db_dir).await;
        scan(&storage, library.path()).await.expect("warm scan");
        storage
    });
    group.bench_function("incremental_1000_files", |b| {
        b.to_async(&rt).iter(|| async {
            let res = scan(&warm, library.path()).await.expect("rescan");
            assert_eq!(res.files_upserted, 0);
        });
    });

    group.finish();
}

criterion_group!(benches, bench_scan);
criterion_main!(benches);
