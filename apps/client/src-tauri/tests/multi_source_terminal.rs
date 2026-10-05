//! `run_multi_source` is the path real downloads take (`download_open` spawns
//! it), and its caller only logs an `Err`. So every way the download can end
//! must itself record a terminal state in the store and emit it to the UI:
//! otherwise a download that loses its source, hits an error or is cancelled by
//! leaving the room stays at `transferring` for good, and the modal that blocks
//! the app while a download runs has nothing to dismiss.
//!
//! These tests drive `run_multi_source` through two routes that used to exit
//! with no terminal state: the cancellation token firing (what `room_leave`
//! does through the transfer registry) and every source going away.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use locast_client_lib::core::hashing::{Blake3Hasher, Sha256Hasher, CHUNK_SIZE};
use locast_client_lib::room::peer_id::derive_peer_id;
use locast_client_lib::storage::Storage;
use locast_client_lib::transfer::events::{DownloadEventEmitter, RecordingSink};
use locast_client_lib::transfer::multi_source::{
    run_multi_source, MultiSourceReceiver, SourceHandle,
};
use locast_client_lib::transfer::plan::{plan_download, DownloadPlan};
use locast_client_lib::transfer::scheduler::Scheduler;
use locast_client_lib::transfer::state::DownloadStore;
use locast_client_lib::transfer::transport::{loopback_pair, Transport};
use tokio_util::sync::CancellationToken;

const TOTAL_SIZE: usize = 2 * CHUNK_SIZE;
const HOST_PUBKEY: [u8; 32] = [0xAAu8; 32];

/// The process-global emitter is a `OnceLock`, so every test in this binary
/// shares one recording sink and filters by its own download id.
fn sink() -> Arc<RecordingSink> {
    static SINK: OnceLock<Arc<RecordingSink>> = OnceLock::new();
    SINK.get_or_init(|| {
        let sink = Arc::new(RecordingSink::default());
        locast_client_lib::install_download_event_emitter(Arc::new(DownloadEventEmitter::new(
            sink.clone(),
        )));
        sink
    })
    .clone()
}

fn states_for(sink: &RecordingSink, download_id: &str) -> Vec<(String, Option<String>)> {
    sink.states
        .lock()
        .unwrap()
        .iter()
        .filter(|e| e.id == download_id)
        .map(|e| (e.state.clone(), e.error_message.clone()))
        .collect()
}

async fn write_fixture(path: &Path) {
    // Deterministic, non-repeating enough to give each chunk its own hash.
    let bytes: Vec<u8> = (0..TOTAL_SIZE)
        .map(|i| (i.wrapping_mul(31).wrapping_add(i >> 8)) as u8)
        .collect();
    tokio::fs::write(path, bytes).await.expect("write fixture");
}

async fn build_plan(download_id: &str, path: &Path) -> DownloadPlan {
    use locast_manifest::{MediaEntry, Source};
    let bytes = tokio::fs::read(path).await.expect("read fixture");
    let chunk_hashes: Vec<String> = bytes
        .chunks(CHUNK_SIZE)
        .map(|c| {
            let mut h = Sha256Hasher::new();
            h.update(c);
            h.finalize_hex()
        })
        .collect();
    let mut sha = Sha256Hasher::new();
    sha.update(&bytes);
    let mut blake = Blake3Hasher::new();
    blake.update(&bytes);
    let peer_id = derive_peer_id(HOST_PUBKEY);
    let entry = MediaEntry {
        id: "media-uuid".into(),
        filename: "fixture.bin".into(),
        sha256: sha.finalize_hex(),
        blake3: blake.finalize_hex(),
        size_bytes: TOTAL_SIZE as u64,
        mime: "application/octet-stream".into(),
        duration_ms: 0,
        dimensions: None,
        codecs: None,
        sources: vec![Source {
            peer_id: peer_id.clone(),
            url_hint: None,
            priority: 0,
            chunk_size: CHUNK_SIZE as u32,
            total_chunks: chunk_hashes.len() as u32,
            chunk_hashes,
        }],
    };
    plan_download(download_id, "media-uuid", 1, &entry, &peer_id).expect("plan")
}

async fn seed_download(store: &DownloadStore, plan: &DownloadPlan) {
    sqlx::query(
        "INSERT OR IGNORE INTO user_identities
         (id, public_key, display_name, created_at, last_seen)
         VALUES ('u-1', 'pk', 'tester', 0, 0)",
    )
    .execute(store.pool())
    .await
    .expect("seed user");
    sqlx::query(
        "INSERT OR IGNORE INTO media_items
         (id, sha256, blake3, size_bytes, filename, relative_path, mime,
          status, created_at, last_seen_at, provenance)
         VALUES ('media-uuid', 'aa', 'bb', 0, 'fixture.bin', 'fixture.bin',
                 'application/octet-stream', 'temporary', 0, 0, '{}')",
    )
    .execute(store.pool())
    .await
    .expect("seed media");
    let mut tx = store.pool().begin().await.expect("tx");
    sqlx::query(
        "INSERT INTO downloads
         (id, media_id, room_id, user_id, state, total_bytes, transferred_bytes,
          started_at, source_peer_id, chunk_size_bytes, manifest_version, last_error)
         VALUES (?, ?, NULL, 'u-1', 'pending', ?, 0, 0, ?, ?, ?, NULL)",
    )
    .bind(&plan.download_id)
    .bind(&plan.media_id)
    .bind(plan.size_bytes as i64)
    .bind(&plan.source.peer_id)
    .bind(CHUNK_SIZE as i64)
    .bind(plan.manifest_version)
    .execute(&mut *tx)
    .await
    .expect("insert download");
    for chunk in &plan.chunks {
        sqlx::query(
            "INSERT INTO download_chunks
             (id, download_id, \"index\", offset, length, sha256, state)
             VALUES (?, ?, ?, ?, ?, ?, 'pending')",
        )
        .bind(uuid::Uuid::now_v7().to_string())
        .bind(&plan.download_id)
        .bind(chunk.index as i64)
        .bind(chunk.offset as i64)
        .bind(chunk.length as i64)
        .bind(&chunk.sha256)
        .execute(&mut *tx)
        .await
        .expect("insert chunk");
    }
    tx.commit().await.expect("commit");
}

struct Fixture {
    _tmp: tempfile::TempDir,
    store: DownloadStore,
    plan: Arc<DownloadPlan>,
    lib_root: PathBuf,
}

async fn fixture(download_id: &str) -> Fixture {
    let tmp = tempfile::tempdir().expect("tempdir");
    let staging = tmp.path().join("fixture.bin");
    write_fixture(&staging).await;
    let plan = build_plan(download_id, &staging).await;
    let storage = Storage::open(&tmp.path().join("index.sqlite"))
        .await
        .expect("open storage");
    let store = DownloadStore::new(storage.pool().clone());
    seed_download(&store, &plan).await;
    Fixture {
        lib_root: tmp.path().to_path_buf(),
        _tmp: tmp,
        store,
        plan: Arc::new(plan),
    }
}

/// One source whose host end is returned so a test can keep it open or close it.
fn source() -> (SourceHandle, Arc<dyn Transport>) {
    let (host_side, recv_side) = loopback_pair(0, 0);
    let host: Arc<dyn Transport> = Arc::new(host_side);
    let transport: Arc<dyn Transport> = Arc::new(recv_side);
    let cancel = CancellationToken::new();
    let handle = SourceHandle {
        peer_id: derive_peer_id(HOST_PUBKEY),
        transport: transport.clone(),
        priority: 0,
        sched: Arc::new(Scheduler::new(transport, cancel.clone())),
        demotion_count: 0,
        unavailable: false,
        unavailable_since: None,
        cancel,
        rtt_samples: VecDeque::new(),
    };
    (handle, host)
}

/// Leave bytes where a half-finished download would have: a verified chunk
/// under `tmp/incomplete/<id>/` and a partial under `tmp/staging/<id>/`.
fn leave_scratch(lib_root: &Path, id: &str) -> (PathBuf, PathBuf) {
    let incomplete = lib_root.join("tmp").join("incomplete").join(id);
    let staging = lib_root.join("tmp").join("staging").join(id);
    std::fs::create_dir_all(&incomplete).expect("incomplete dir");
    std::fs::create_dir_all(&staging).expect("staging dir");
    std::fs::write(incomplete.join("00000000.chunk"), vec![1u8; 4096]).expect("chunk");
    std::fs::write(staging.join("partial.partial"), vec![2u8; 4096]).expect("partial");
    (incomplete, staging)
}

async fn row_state(store: &DownloadStore, id: &str) -> String {
    store
        .fetch(id)
        .await
        .expect("row")
        .state
        .as_str()
        .to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_the_download_records_cancelled() {
    let sink = sink();
    let f = fixture("01234567-89ab-cdef-0123-456789abcf01").await;
    let (handle, _host_end) = source();
    let receiver = Arc::new(
        MultiSourceReceiver::new(
            f.plan.clone(),
            f.store.clone(),
            f.lib_root.clone(),
            HOST_PUBKEY,
            vec![handle],
        )
        .expect("receiver"),
    );
    // What leaving the room does through the transfer registry.
    receiver.cancel_handle().cancel();

    let res = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        run_multi_source(receiver, "fixture.bin".into()),
    )
    .await
    .expect("must return, not hang");
    assert!(res.is_err(), "a cancelled download is reported as an error");

    let states = states_for(&sink, &f.plan.download_id);
    assert_eq!(
        states.last().map(|(s, _)| s.as_str()),
        Some("cancelled"),
        "the UI must be told the download ended, got {states:?}"
    );
    assert_eq!(states.iter().filter(|(s, _)| s == "cancelled").count(), 1);
    assert_eq!(row_state(&f.store, &f.plan.download_id).await, "cancelled");
}

/// A cancelled download can never be resumed (`Cancelled` is terminal), and
/// its scratch bytes count against the quota, so they must not outlive it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancelled_download_leaves_no_scratch_files() {
    let f = fixture("01234567-89ab-cdef-0123-456789abcf03").await;
    let (incomplete, staging) = leave_scratch(&f.lib_root, &f.plan.download_id);
    let (handle, _host_end) = source();
    let receiver = Arc::new(
        MultiSourceReceiver::new(
            f.plan.clone(),
            f.store.clone(),
            f.lib_root.clone(),
            HOST_PUBKEY,
            vec![handle],
        )
        .expect("receiver"),
    );
    receiver.cancel_handle().cancel();
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        run_multi_source(receiver, "fixture.bin".into()),
    )
    .await
    .expect("must return, not hang");
    assert_eq!(row_state(&f.store, &f.plan.download_id).await, "cancelled");
    assert!(!incomplete.exists(), "tmp/incomplete/<id> must be removed");
    assert!(!staging.exists(), "tmp/staging/<id> must be removed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn losing_every_source_records_failed_with_a_reason() {
    let sink = sink();
    let f = fixture("01234567-89ab-cdef-0123-456789abcf02").await;
    let (handle, host_end) = source();
    // The host disappears before sending anything.
    host_end.close().await;
    drop(host_end);
    let receiver = Arc::new(
        MultiSourceReceiver::new(
            f.plan.clone(),
            f.store.clone(),
            f.lib_root.clone(),
            HOST_PUBKEY,
            vec![handle],
        )
        .expect("receiver"),
    );

    let cancel = receiver.cancel_handle();
    assert!(!cancel.is_cancelled(), "precondition: not cancelled yet");
    let res = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        run_multi_source(receiver, "fixture.bin".into()),
    )
    .await
    .expect("must return, not hang");
    assert!(res.is_err(), "a download that lost its source is an error");
    assert!(
        cancel.is_cancelled(),
        "a finished run releases the tasks waiting on its cancel token"
    );

    let states = states_for(&sink, &f.plan.download_id);
    let (last, message) = states.last().cloned().expect("some state");
    assert!(
        last == "failed" || last == "cancelled",
        "the UI must be told the download ended, got {states:?}"
    );
    if last == "failed" {
        assert!(
            message.as_deref().is_some_and(|m| !m.is_empty()),
            "a failure carries a reason the modal can show, got {states:?}"
        );
    }
    assert_eq!(row_state(&f.store, &f.plan.download_id).await, last);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_download_leaves_no_scratch_files() {
    let f = fixture("01234567-89ab-cdef-0123-456789abcf04").await;
    let (incomplete, staging) = leave_scratch(&f.lib_root, &f.plan.download_id);
    let (handle, host_end) = source();
    host_end.close().await;
    drop(host_end);
    let receiver = Arc::new(
        MultiSourceReceiver::new(
            f.plan.clone(),
            f.store.clone(),
            f.lib_root.clone(),
            HOST_PUBKEY,
            vec![handle],
        )
        .expect("receiver"),
    );
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        run_multi_source(receiver, "fixture.bin".into()),
    )
    .await
    .expect("must return, not hang");
    let state = row_state(&f.store, &f.plan.download_id).await;
    assert!(state == "failed" || state == "cancelled", "got {state}");
    assert!(!incomplete.exists(), "tmp/incomplete/<id> must be removed");
    assert!(!staging.exists(), "tmp/staging/<id> must be removed");
}
