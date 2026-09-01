//! P3-T09 end-to-end multi-source failover acceptance test.
//!
//! The roadmap acceptance criterion for P3-T09 is:
//!
//! > an integration test that proves: A is actually selected
//! > first; A really drops requests; the system detects
//! > repeated failure; B is actually selected after failover;
//! > chunks successfully received from B become part of the
//! > same logical bitmap; the completed file is correct.
//!
//! Scenario:
//! - one viewer (the orchestrator)
//! - two available sources (peer A and peer B)
//! - source A has lower priority (priority 0)
//! - source A silently drops Request frames for chunks 0
//!   and 2 (deterministic rotation trigger)
//! - source B remains healthy (0% loss)
//! - 3-NAK threshold + retries trigger source rotation
//! - the two dropped chunks are successfully retrieved
//!   from B
//! - the completed file passes integrity verification
//! - completion bitmap contains every expected chunk
//!
//! The test is a sibling of `transfer_e2e.rs`; helpers
//! (`write_fixture`, `blake3_of_file`, `per_chunk_sha256`,
//! `build_plan`, `open_storage_in`, `seed_fk_deps`,
//! `create_download`) are duplicated here. The duplication is
//! deliberate and matches the codebase's `download_events.rs`
//! pattern: integration-test files do not share helpers across
//! binaries.

#![allow(clippy::needless_range_loop)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use locast_client_lib::core::hashing::{Blake3Hasher, Sha256Hasher, CHUNK_SIZE};
use locast_client_lib::room::peer_id::derive_peer_id;
use locast_client_lib::storage::Storage;
use locast_client_lib::transfer::multi_source::{
    run_multi_source, MultiSourceReceiver, SourceHandle,
};
use locast_client_lib::transfer::plan::{plan_download, DownloadPlan};
use locast_client_lib::transfer::scheduler::Scheduler;
use locast_client_lib::transfer::state::{DownloadState, DownloadStore};
use locast_client_lib::transfer::transport::{
    loopback_pair as transport_loopback_pair, Transport, TransportError,
};
use locast_client_lib::transfer::verify::verify_full_blake3;
use locast_client_lib::transfer::{SenderSession, WINDOW_SIZE};

use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// 1 MiB. Smaller than the P3-T06 50 MiB fixture; this test
/// cares about rotation, not throughput, and the smaller
/// fixture keeps the loopback round-trip count down so the
/// test runs in a few seconds.
const TOTAL_SIZE: usize = 1024 * 1024;

/// 64 KiB scratch buffer. Keeps peak memory low while
/// streaming the fixture to disk.
const FIXTURE_SCRATCH: usize = 64 * 1024;

/// Local pubkey for the host side. In this test the same
/// pubkey is used by the orchestrator and by both sources
/// (matching the existing single-source `transfer_e2e.rs`
/// convention). The `SenderSession::run` check is
/// `hello.peer_id == plan.source.peer_id`; since every side
/// derives from this pubkey, both senders accept the
/// orchestrator's Hello.
const HOST_PUBKEY: [u8; 32] = [0xAAu8; 32];

/// Viewer's local pubkey. Same as the host pubkey for this
/// degenerate "shared key" test scenario.
const VIEWER_PUBKEY: [u8; 32] = HOST_PUBKEY;

/// Deterministic SplitMix64 PRNG. Same algorithm as the
/// existing `transfer_e2e.rs` fixture.
struct SplitMix64(u64);
impl SplitMix64 {
    fn new(seed: u64) -> Self {
        Self(seed)
    }
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }
    fn fill(&mut self, buf: &mut [u8]) {
        let mut i = 0;
        while i + 8 <= buf.len() {
            let v = self.next().to_le_bytes();
            buf[i..i + 8].copy_from_slice(&v);
            i += 8;
        }
        if i < buf.len() {
            let v = self.next().to_le_bytes();
            let remaining = buf.len() - i;
            buf[i..].copy_from_slice(&v[..remaining]);
        }
    }
}

async fn write_fixture(path: &std::path::Path) {
    let mut f = tokio::fs::File::create(path).await.expect("create");
    let mut scratch = vec![0u8; FIXTURE_SCRATCH];
    let mut rng = SplitMix64::new(0xCAFEBABE_DEADBEEFu64);
    let mut written = 0usize;
    while written < TOTAL_SIZE {
        let n = FIXTURE_SCRATCH.min(TOTAL_SIZE - written);
        rng.fill(&mut scratch[..n]);
        f.write_all(&scratch[..n]).await.expect("write");
        written += n;
    }
    f.flush().await.expect("flush");
}

async fn blake3_of_file(path: &std::path::Path) -> String {
    let mut f = tokio::fs::File::open(path).await.expect("open");
    let mut h = Blake3Hasher::new();
    let mut scratch = vec![0u8; FIXTURE_SCRATCH];
    loop {
        let n = f.read(&mut scratch).await.expect("read");
        if n == 0 {
            break;
        }
        h.update(&scratch[..n]);
    }
    h.finalize_hex()
}

async fn per_chunk_sha256(path: &std::path::Path) -> Vec<String> {
    let mut f = tokio::fs::File::open(path).await.expect("open");
    let mut out = Vec::new();
    let mut scratch = vec![0u8; CHUNK_SIZE];
    loop {
        let n_read = f.read(&mut scratch).await.expect("read");
        if n_read == 0 {
            break;
        }
        let mut sha = Sha256Hasher::new();
        sha.update(&scratch[..n_read]);
        out.push(sha.finalize_hex());
    }
    out
}

async fn build_plan(download_id: &str, src_path: &std::path::Path) -> (DownloadPlan, String) {
    use locast_manifest::{MediaEntry, Source};
    let chunk_hashes = per_chunk_sha256(src_path).await;
    let blake3 = blake3_of_file(src_path).await;
    let mut sha_hasher = Sha256Hasher::new();
    let mut f = tokio::fs::File::open(src_path).await.expect("open");
    let mut scratch = vec![0u8; FIXTURE_SCRATCH];
    loop {
        let n = f.read(&mut scratch).await.expect("read");
        if n == 0 {
            break;
        }
        sha_hasher.update(&scratch[..n]);
    }
    let sha = sha_hasher.finalize_hex();
    let peer_id = derive_peer_id(HOST_PUBKEY);
    let total_chunks = (TOTAL_SIZE / CHUNK_SIZE) as u32;
    let entry = MediaEntry {
        id: "media-uuid".into(),
        filename: "fixture.bin".into(),
        sha256: sha.clone(),
        blake3: blake3.clone(),
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
            total_chunks,
            chunk_hashes: chunk_hashes.clone(),
        }],
    };
    let plan = plan_download(download_id, "media-uuid", 1, &entry, &peer_id).expect("plan");
    (plan, blake3)
}

async fn open_storage_in(dir: &std::path::Path) -> Storage {
    let db = dir.join("index.sqlite");
    Storage::open(&db).await.expect("storage open")
}

async fn seed_fk_deps(store: &DownloadStore, user_id: &str, media_id: &str) {
    sqlx::query(
        "INSERT OR IGNORE INTO user_identities
         (id, public_key, display_name, created_at, last_seen)
         VALUES (?, 'pk', 'tester', 0, 0)",
    )
    .bind(user_id)
    .execute(store.pool())
    .await
    .expect("seed user");
    sqlx::query(
        "INSERT OR IGNORE INTO media_items
         (id, sha256, blake3, size_bytes, filename, relative_path, mime,
          status, created_at, last_seen_at, provenance)
         VALUES (?, 'aa', 'bb', 0, 'fixture.bin', 'fixture.bin', 'application/octet-stream',
                 'temporary', 0, 0, '{}')",
    )
    .bind(media_id)
    .execute(store.pool())
    .await
    .expect("seed media");
}

async fn create_download(store: &DownloadStore, plan: &DownloadPlan) {
    let n = plan;
    let mut tx = store.pool().begin().await.expect("tx");
    sqlx::query(
        "INSERT INTO downloads
         (id, media_id, room_id, user_id, state, total_bytes, transferred_bytes,
          started_at, source_peer_id, chunk_size_bytes, manifest_version, last_error)
         VALUES (?, ?, NULL, ?, 'pending', ?, 0, ?, ?, ?, ?, NULL)",
    )
    .bind(&n.download_id)
    .bind(&n.media_id)
    .bind("u-1")
    .bind(n.size_bytes as i64)
    .bind(0i64)
    .bind(&n.source.peer_id)
    .bind(CHUNK_SIZE as i64)
    .bind(n.manifest_version)
    .execute(&mut *tx)
    .await
    .expect("insert downloads");
    for chunk in &n.chunks {
        let id = uuid::Uuid::now_v7().to_string();
        sqlx::query(
            "INSERT INTO download_chunks
             (id, download_id, \"index\", offset, length, sha256, state)
             VALUES (?, ?, ?, ?, ?, ?, 'pending')",
        )
        .bind(id)
        .bind(&n.download_id)
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

/// A `Transport` wrapper that drops Request frames for a
/// specific set of chunk indices and passes every other
/// frame through untouched. Used to simulate peer A's
/// selective-loss pattern in the P3-T09 acceptance test:
/// drop Requests for chunk 0 and chunk 2 so the
/// orchestrator is forced to rotate those two chunks to
/// peer B.
struct LossyRequestsTransport {
    inner: Arc<dyn Transport>,
    drop_indices: std::collections::HashSet<u32>,
}

impl LossyRequestsTransport {
    fn new(inner: Arc<dyn Transport>, drop_indices: std::collections::HashSet<u32>) -> Self {
        Self {
            inner,
            drop_indices,
        }
    }

    /// Parse a length-prefixed frame just enough to extract
    /// the JSON `kind` and (for Request frames) the
    /// `chunk_index`. Cheap string scan on the JSON
    /// payload; we do not need full validation here because
    /// the receiver already validates the parsed frame.
    fn should_drop(&self, bytes: &[u8]) -> bool {
        if bytes.len() < 5 {
            return false;
        }
        let len = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
        if bytes.len() < 4 + len {
            return false;
        }
        let payload = &bytes[4..4 + len];
        let payload_str = match std::str::from_utf8(payload) {
            Ok(s) => s,
            Err(_) => return false,
        };
        if !payload_str.contains("\"kind\":\"request\"") {
            return false;
        }
        // Extract "chunk_index":NNN. We scan for the
        // substring rather than full JSON parsing to keep
        // this lightweight.
        let key = "\"chunk_index\":";
        if let Some(start) = payload_str.find(key) {
            let after = &payload_str[start + key.len()..];
            let digits: String = after
                .chars()
                .skip_while(|c| c.is_whitespace())
                .take_while(|c| c.is_ascii_digit())
                .collect();
            if let Ok(n) = digits.parse::<u32>() {
                return self.drop_indices.contains(&n);
            }
        }
        false
    }
}

#[async_trait]
impl Transport for LossyRequestsTransport {
    async fn send(&self, frame_bytes: Vec<u8>) -> Result<(), TransportError> {
        if self.should_drop(&frame_bytes) {
            return Ok(());
        }
        self.inner.send(frame_bytes).await
    }
    async fn recv(&self) -> Result<Option<Vec<u8>>, TransportError> {
        self.inner.recv().await
    }
    async fn close(&self) {
        self.inner.close().await
    }
}

/// The headline acceptance test for P3-T09: peer A drops
/// Request frames for chunks 0 and 2, peer B is healthy,
/// the orchestrator rotates those two chunks from A to B
/// after the NAK threshold, the final file passes
/// integrity verification, and the verified-chunk bitmap
/// is full. Both sources end up serving chunks; chunk
/// attribution is observable via the test-only
/// `verified_sources_snapshot`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multi_source_failover_rotates_from_lossy_a_to_healthy_b() {
    // 1. Two tempdirs: one for the host library, one for the
    //    receiver library.
    let host_tmp = tempfile::tempdir().expect("host tmpdir");
    let recv_tmp = tempfile::tempdir().expect("recv tmpdir");
    let host_lib_root: PathBuf = host_tmp.path().to_path_buf();
    let recv_lib_root: PathBuf = recv_tmp.path().to_path_buf();

    // 2. Write the fixture; both senders read from the same
    //    `host_lib_root/<sha>` file.
    let fixture_staging = host_lib_root.join("staging-fixture.bin");
    write_fixture(&fixture_staging).await;
    let (plan, expected_blake3) =
        build_plan("01234567-89ab-cdef-0123-456789abcd09", &fixture_staging).await;
    let host_source = host_lib_root.join(&plan.sha256);
    tokio::fs::rename(&fixture_staging, &host_source)
        .await
        .expect("rename fixture to host source");

    // 3. Receiver storage + downloads row.
    let storage = open_storage_in(&recv_lib_root).await;
    let store = DownloadStore::new(storage.pool().clone());
    seed_fk_deps(&store, "u-1", "media-uuid").await;
    create_download(&store, &plan).await;

    // 4. Two loopback transport pairs: one per source. The
    //    orchestrator-side transport for peer A is wrapped
    //    in `LossyRequestsTransport` so the orchestrator's
    //    OUTGOING Request frames (sent on recv_a) are
    //    filtered; the wrapper's `send` drops Requests
    //    for the configured chunk indices. Peer B's
    //    transport stays healthy.
    let (host_a_side, recv_a_side) = transport_loopback_pair(0, 0);
    let (host_b_side, recv_b_side) = transport_loopback_pair(0, 0);
    let host_a: Arc<dyn Transport> = Arc::new(host_a_side);
    let host_b: Arc<dyn Transport> = Arc::new(host_b_side);
    let recv_a_inner: Arc<dyn Transport> = Arc::new(recv_a_side);
    let mut drop_a = std::collections::HashSet::new();
    drop_a.insert(0u32);
    drop_a.insert(2u32);
    let recv_a: Arc<dyn Transport> =
        Arc::new(LossyRequestsTransport::new(recv_a_inner.clone(), drop_a));
    let recv_b: Arc<dyn Transport> = Arc::new(recv_b_side);

    // 5. Build the orchestrator's source ring. Source A is
    //    priority 0 (preferred); source B is priority 1.
    //    Distinct internal labels (peer_id) so the
    //    orchestrator's DuplicatePeerId check passes.
    let cancel = tokio_util::sync::CancellationToken::new();
    let sched_a = Arc::new(Scheduler::new(recv_a.clone(), cancel.clone()));
    let sched_b = Arc::new(Scheduler::new(recv_b.clone(), cancel.clone()));
    let label_a = format!("A-{}", derive_peer_id(HOST_PUBKEY));
    let label_b = format!("B-{}", derive_peer_id(HOST_PUBKEY));
    let sources = vec![
        SourceHandle {
            peer_id: label_a.clone(),
            transport: recv_a.clone(),
            priority: 0,
            sched: sched_a,
            demotion_count: 0,
            unavailable: false,
            unavailable_since: None,
            cancel: cancel.clone(),
            rtt_samples: std::collections::VecDeque::new(),
        },
        SourceHandle {
            peer_id: label_b.clone(),
            transport: recv_b.clone(),
            priority: 1,
            sched: sched_b,
            demotion_count: 0,
            unavailable: false,
            unavailable_since: None,
            cancel: cancel.clone(),
            rtt_samples: std::collections::VecDeque::new(),
        },
    ];

    // 6. Build the receiver.
    let receiver = Arc::new(
        MultiSourceReceiver::new(
            Arc::new(plan.clone()),
            store.clone(),
            recv_lib_root.clone(),
            VIEWER_PUBKEY,
            sources,
        )
        .expect("multi-source receiver"),
    );

    // 7. Run the two senders concurrently. They each
    //    `await hello` from the orchestrator.
    let sender_plan_a = plan.clone();
    let sender_lib_root_a = host_lib_root.clone();
    let host_a_for_sender = host_a.clone();
    let sender_handle_a = tokio::spawn(async move {
        let session = SenderSession::new(&sender_plan_a, host_a_for_sender, sender_lib_root_a);
        let _ = tokio::time::timeout(
            Duration::from_secs(60),
            session.run("fixture.bin".to_string()),
        )
        .await;
    });
    let sender_plan_b = plan.clone();
    let sender_lib_root_b = host_lib_root.clone();
    let host_b_for_sender = host_b.clone();
    let sender_handle_b = tokio::spawn(async move {
        let session = SenderSession::new(&sender_plan_b, host_b_for_sender, sender_lib_root_b);
        let _ = tokio::time::timeout(
            Duration::from_secs(60),
            session.run("fixture.bin".to_string()),
        )
        .await;
    });

    // 8. Run the orchestrator.
    let recv_lib_root_for_run = recv_lib_root.clone();
    let recv_receiver = receiver.clone();
    let orchestrator_handle = tokio::spawn(async move {
        let r = run_multi_source(recv_receiver, "fixture.bin".to_string()).await;
        r
    });
    let _ = recv_lib_root_for_run;

    // 9. Await the orchestrator. Bound it with a generous
    //    timeout (the orchestrator's WINDOW_SIZE cap means
    //    this test should finish well under 30 s on any
    //    hardware).
    let recv_res = tokio::time::timeout(Duration::from_secs(60), orchestrator_handle)
        .await
        .expect("orchestrator timeout")
        .expect("orchestrator join");
    let final_state = recv_res.expect("orchestrator ok");

    // 10. Senders are allowed to wind down naturally.
    host_a.close().await;
    host_b.close().await;
    let _ = sender_handle_a.await;
    let _ = sender_handle_b.await;

    // 11. ACCEPTANCE ASSERTIONS.
    assert_eq!(
        final_state,
        DownloadState::Complete,
        "orchestrator must report Complete"
    );

    // a. Final state in the store.
    let rec = store.fetch(&plan.download_id).await.expect("fetch");
    assert_eq!(rec.state, DownloadState::Complete);

    // b. All chunks verified in the bitmap.
    let verified_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM download_chunks
         WHERE download_id = ? AND state = 'verified'",
    )
    .bind(&plan.download_id)
    .fetch_one(store.pool())
    .await
    .expect("verified count");
    assert_eq!(
        verified_count as usize,
        plan.chunks.len(),
        "every chunk must be verified"
    );

    // c. The final file matches BLAKE3.
    let final_path = locast_client_lib::core::paths::content_addressed_path(
        &recv_lib_root,
        &plan.sha256,
        "fixture.bin",
    )
    .expect("content path");
    assert!(
        final_path.exists(),
        "final file not found at {final_path:?}"
    );
    let final_bytes = tokio::fs::read(&final_path).await.expect("read final");
    assert_eq!(final_bytes.len(), TOTAL_SIZE);
    let mut blake = Blake3Hasher::new();
    blake.update(&final_bytes);
    let actual_blake = blake.finalize_hex();
    assert_eq!(actual_blake, expected_blake3, "BLAKE3 mismatch");
    verify_full_blake3(&final_bytes, TOTAL_SIZE as u64, &expected_blake3)
        .expect("verify_full_blake3");

    // d. Per-chunk SHA-256 of the assembled file matches the
    //    planner's expectation. This is the bitmap-merge
    //    correctness gate: every chunk the planner expected
    //    is present, regardless of which source delivered
    //    it.
    for chunk in &plan.chunks {
        let start = chunk.offset as usize;
        let end = start + chunk.length as usize;
        let bytes = &final_bytes[start..end];
        let mut h = Sha256Hasher::new();
        h.update(bytes);
        let sha = h.finalize_hex();
        assert_eq!(sha, chunk.sha256, "chunk {} sha256 mismatch", chunk.index);
    }

    // e. The verified_sources snapshot proves BOTH sources
    //    served at least one chunk. Source A's labels with
    //    "A-" prefix, source B's with "B-". Per the
    //    rotation contract, peer B must serve the two
    //    dropped chunks (0 and 2) and peer A must serve at
    //    least one of the remaining chunks.
    let verified_snapshot = receiver.verified_sources_snapshot().await;
    let mut used_a = false;
    let mut used_b = false;
    let mut b_served_indices: Vec<u32> = Vec::new();
    for (idx, label) in &verified_snapshot {
        if label.starts_with("A-") {
            used_a = true;
        }
        if label.starts_with("B-") {
            used_b = true;
            b_served_indices.push(*idx);
        }
    }
    assert!(
        used_a,
        "source A must have served at least one chunk (proves A was selected first); snapshot: {verified_snapshot:?}"
    );
    assert!(
        used_b,
        "source B must have served at least one chunk (proves B was selected after failover); snapshot: {verified_snapshot:?}"
    );
    // B must serve the two chunks A dropped.
    let mut b_set: std::collections::HashSet<u32> = b_served_indices.into_iter().collect();
    assert!(
        b_set.contains(&0),
        "source B did not serve chunk 0 (rotation did not happen); snapshot: {verified_snapshot:?}"
    );
    assert!(
        b_set.contains(&2),
        "source B did not serve chunk 2 (rotation did not happen); snapshot: {verified_snapshot:?}"
    );
    b_set.clear();

    // f. Peer A's demotion_count must be > 0 (proves the
    //    NAK demotion threshold fired at least once for
    //    the dropped chunks 0 and 2).
    let snap = receiver.sources_snapshot().await;
    let a_demotion = snap
        .iter()
        .find(|(label, _p, _u, _d)| label.starts_with("A-"))
        .map(|(_, _, _, d)| *d)
        .expect("source A in snapshot");
    assert!(
        a_demotion > 0,
        "source A must have been demoted at least once; sources_snapshot: {snap:?}"
    );
}

/// WINDOW_SIZE pin so the test catches a future break of the
/// architecture's 16-slot ceiling.
#[test]
fn window_size_pin() {
    assert_eq!(WINDOW_SIZE, 16);
}

// Lint pin: ensure unused imports stay in scope.
#[allow(dead_code)]
fn _imports_pinned() {
    let _: HashMap<u32, String> = HashMap::new();
}
