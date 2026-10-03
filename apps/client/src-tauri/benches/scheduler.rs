//! P8-T06 bench: chunk request scheduler (ARCHITECTURE 28.10 hot path 4).
//!
//! The scheduler's async slot acquisition sleeps by design, so the pure
//! decision path is what is measured: plan a 1 MiB download (4 chunks),
//! pick a source among 4 peers for every chunk, and take a rate-limit
//! token for every request. Gate from the architecture: schedule 1 MiB
//! of requests in < 1 ms.
//!
//! One iteration schedules `REPEATS` x 1 MiB so the timed region is long
//! enough (hundreds of microseconds) for a 10 percent gate to be meaningful;
//! divide the reported time by `REPEATS` for the per-MiB figure.

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::time::Instant;

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use locast_client_lib::transfer::multi_source::{SourceHandle, SourceSelector};
use locast_client_lib::transfer::plan::plan_download;
use locast_client_lib::transfer::scheduler::{Scheduler, TokenBucket};
use locast_client_lib::transfer::transport::{loopback_pair, Transport};
use locast_manifest::{MediaEntry, Source};
use tokio_util::sync::CancellationToken;

const CHUNK_SIZE: u32 = 262_144;
const ONE_MIB: u64 = 1024 * 1024;
const PEERS: usize = 4;
const REPEATS: usize = 256;

fn handle(index: usize) -> SourceHandle {
    let (_a, b) = loopback_pair(0, 0);
    let transport: Arc<dyn Transport> = Arc::new(b);
    let cancel = CancellationToken::new();
    SourceHandle {
        peer_id: format!("{index:064x}"),
        sched: Arc::new(Scheduler::new(transport.clone(), cancel.clone())),
        transport,
        priority: i32::try_from(index).expect("small index"),
        demotion_count: 0,
        unavailable: false,
        unavailable_since: None,
        cancel,
        rtt_samples: VecDeque::new(),
    }
}

fn one_mib_entry(peer_id: &str) -> MediaEntry {
    let total_chunks = (ONE_MIB / u64::from(CHUNK_SIZE)) as u32;
    MediaEntry {
        id: "media-0".to_string(),
        filename: "one-mib.mkv".to_string(),
        sha256: "a".repeat(64),
        blake3: "b".repeat(64),
        size_bytes: ONE_MIB,
        mime: "video/x-matroska".to_string(),
        duration_ms: 1_000,
        dimensions: None,
        codecs: None,
        sources: vec![Source {
            peer_id: peer_id.to_string(),
            url_hint: None,
            priority: 0,
            chunk_size: CHUNK_SIZE,
            total_chunks,
            chunk_hashes: (0..total_chunks).map(|i| format!("{i:064x}")).collect(),
        }],
    }
}

fn bench_scheduler(c: &mut Criterion) {
    // `Scheduler::new` and the loopback transport need a tokio context.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let _guard = rt.enter();

    let handles: Vec<SourceHandle> = (0..PEERS).map(handle).collect();
    let peer_id = format!("{:064x}", 0);
    let entry = one_mib_entry(&peer_id);
    let tried = HashSet::new();

    let mut group = c.benchmark_group("scheduler");
    group.sample_size(100);
    group.measurement_time(std::time::Duration::from_secs(5));

    group.bench_function("schedule_256x1mib_4_peers", |b| {
        b.iter(|| {
            let mut scheduled = 0;
            for _ in 0..REPEATS {
                let plan =
                    plan_download("dl-0", "media-0", 1, black_box(&entry), &peer_id).expect("plan");
                let mut bucket = TokenBucket::new(plan.chunks.len() as u32, 16.0);
                let now = Instant::now();
                for chunk in &plan.chunks {
                    let picked = SourceSelector::pick(&handles, chunk.index, &tried, now);
                    black_box(picked.map(|h| h.peer_id.as_str()));
                    black_box(bucket.try_consume());
                    scheduled += 1;
                }
            }
            black_box(scheduled)
        });
    });

    group.finish();
}

criterion_group!(benches, bench_scheduler);
criterion_main!(benches);
