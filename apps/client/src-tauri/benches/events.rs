//! P8-T06 bench: download event throughput (ARCHITECTURE 28.10 hot path 8).
//!
//! 1000 progress events are pushed through the `DownloadEventEmitter`
//! coalescer into a recording sink, which is the Rust half of the Tauri
//! event path (the webview half is out of scope until P8-T07). Gate from
//! the architecture: no drops, p99 dispatch < 1 ms.

use std::sync::Arc;

use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use locast_client_lib::transfer::events::{
    DownloadEventEmitter, DownloadProgressEvent, RecordingSink,
};

const EVENTS: u64 = 1000;

fn progress(i: u64) -> DownloadProgressEvent {
    DownloadProgressEvent {
        v: 1,
        id: "dl-0".to_string(),
        state: "downloading".to_string(),
        transferred_bytes: i * 262_144,
        total_bytes: EVENTS * 262_144,
        bytes_per_sec_ema: 50_000_000.0,
        eta_seconds: Some(10),
    }
}

fn bench_events(c: &mut Criterion) {
    let mut group = c.benchmark_group("events");
    group.sample_size(50);
    group.throughput(Throughput::Elements(EVENTS));

    group.bench_function("record_progress_1000", |b| {
        b.iter(|| {
            let sink = Arc::new(RecordingSink::default());
            let emitter = DownloadEventEmitter::new(sink);
            for i in 0..EVENTS {
                emitter.record_progress(black_box(progress(i)));
            }
        });
    });

    group.finish();
}

criterion_group!(benches, bench_events);
criterion_main!(benches);
