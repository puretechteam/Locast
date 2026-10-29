//! Regression tests: two overlapping `download_open` calls for the same
//! media must collapse onto ONE downloads row and ONE transfer.
//!
//! The bug these pin: `open_download_inner` checks
//! `TransferRegistry::is_active` for the shared downloads row, then
//! awaits several times (participant lookup, DataChannel lookup,
//! `MultiSourceReceiver` setup) before calling
//! `TransferRegistry::register`. Two concurrent opens (in the app: the
//! `SharedMediaBridge` auto-open plus a second caller) both pass the
//! check and both spawn `run_multi_source` on the same row. The second
//! `register` cancels the first run; the loser then writes a terminal
//! state to the SHARED row, emits it as a `download://state` event,
//! deletes the shared scratch dir, and its `RegisterGuard` drop removes
//! the winner's registry entry. The winner later fails (for example
//! "invalid download state transition: cannot transition from Cancelled
//! to Verifying"), so a fully received file is discarded.
//!
//! Setup (mirrors `tests/smoke_host_viewer.rs`): the real
//! `locast-server` router in-process on an ephemeral port, a HOST and a
//! VIEWER rig that share nothing but that server, a real WebRTC
//! PeerConnection with the `files` DataChannel, and the host sender
//! dispatch. The host publishes a signed manifest for a 4 MiB fixture.
//! Once the viewer holds the verified manifest and the host's `files`
//! DataChannel is open, each test fires two `open_download_inner` calls
//! (each with a fresh download id, exactly like the `download_open`
//! command) through `tokio::join!`, so both futures are polled by the
//! same task and interleave at every await.
//!
//! Determinism: the viewer's room carries `PAST_GUESTS` participant
//! rows of guests who already left (a long-lived room's history).
//! `open_download_inner` resolves a pubkey for every participant with
//! one query each, AFTER `is_active` and BEFORE `register`, so the
//! first call's check-to-register window spans hundreds of awaits while
//! the second call needs only a handful to reach its own `is_active`.
//!
//! - `retry_double_open_runs_one_transfer`: the media was opened once
//!   before the source DataChannel was up, so a `pending` row already
//!   exists (the state the command's "no source transport yet" return
//!   leaves; reproduced by pointing that first open at a peer id nobody
//!   owns). Both concurrent opens find that row, so this is purely the
//!   `is_active` vs `register` race.
//! - `first_double_open_runs_one_transfer`: no row exists yet; the two
//!   opens also race to create the row.
//!
//! Orchestrator count is proven from the tracing output: every spawned
//! orchestrator logs exactly one "download complete" or "download
//! failed" line carrying its download id (`commands::download`).
//!
//! Real peers are needed, so the tests are gated by
//! `RUN_WEBRTC_TESTS=1` (the convention of `tests/webrtc_signaling.rs`
//! and `tests/webrtc_basic.rs`). Default `cargo test` skips them.
//!
//! ```text
//! $env:RUN_WEBRTC_TESTS = "1"
//! cargo test -p locast-client --test download_open_concurrent -- --nocapture
//! ```

use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::{Duration, Instant};

use base64::Engine as _;
use locast_client_lib::commands::download::{open_download_inner, DownloadSessionIpc};
use locast_client_lib::commands::error::AppError;
use locast_client_lib::commands::room::invite_anchor_for;
use locast_client_lib::core::paths;
use locast_client_lib::identity::keystore::{IdentityKeyring, IdentityService, MockKeyring};
use locast_client_lib::net::config::SignalingConfig;
use locast_client_lib::net::room::RoomClient;
use locast_client_lib::net::signaling::SignalingClient;
use locast_client_lib::net::state::ConnPhase;
use locast_client_lib::net::webrtc::WebRtcManager;
use locast_client_lib::room::host::{build_invite_url, build_sign_and_publish_selected};
use locast_client_lib::storage::Storage;
use locast_client_lib::transfer::events::{DownloadEventEmitter, RecordingSink};
use locast_client_lib::transfer::state::{DownloadState, DownloadStore};
use locast_client_lib::transfer::{HostDispatchContext, HostSenderDispatcher, TransferRegistry};
use locast_manifest::MediaManifest;
use locast_protocol::handshake::Platform;
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tokio::net::TcpListener;
use uuid::Uuid;

/// 16 chunks of 256 KiB: big enough that the transfer outlives the
/// second `register`, small enough to finish in a few seconds.
const FIXTURE_SIZE: usize = 4 * 1024 * 1024;
const FILENAME: &str = "concurrent.bin";
/// Participants who already left the room; see the module docs.
const PAST_GUESTS: usize = 400;
const TRANSFER_BUDGET: Duration = Duration::from_secs(45);
/// How long a non-complete run waits for a second orchestrator to end.
const STRAGGLER_BUDGET: Duration = Duration::from_secs(20);
/// Canonical (64 lowercase hex) peer id that no participant derives.
const NOBODY_PEER_ID: &str = "0000000000000000000000000000000000000000000000000000000000000000";

fn webrtc_tests_enabled() -> bool {
    std::env::var("RUN_WEBRTC_TESTS").ok().as_deref() == Some("1")
}

// ---------------------------------------------------------------------------
// process-wide capture (tracing output + download://state events)
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct LogBuf(Arc<StdMutex<Vec<u8>>>);

impl std::io::Write for LogBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("log lock").extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl LogBuf {
    /// Lines that record an orchestrator's final outcome for `id`.
    fn outcomes(&self, id: &str) -> Vec<String> {
        String::from_utf8_lossy(&self.0.lock().expect("log lock"))
            .lines()
            .filter(|l| l.contains(id))
            .filter(|l| {
                l.contains("download complete")
                    || l.contains("download failed")
                    || l.contains("download task panicked")
            })
            .map(|l| l.trim().to_string())
            .collect()
    }
}

struct Capture {
    logs: LogBuf,
    events: Arc<RecordingSink>,
}

/// Both sinks are process globals, installed once and shared by the
/// tests in this binary; every check filters by download id.
fn capture() -> &'static Capture {
    static CAPTURE: OnceLock<Capture> = OnceLock::new();
    CAPTURE.get_or_init(|| {
        let logs = LogBuf::default();
        let writer = logs.clone();
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::new("locast_client_lib=info"))
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .try_init();
        // The orchestrator's `DownloadEventEmitter` inherits this sink.
        let events = Arc::new(RecordingSink::default());
        locast_client_lib::install_download_event_emitter(Arc::new(DownloadEventEmitter::new(
            events.clone(),
        )));
        Capture { logs, events }
    })
}

// ---------------------------------------------------------------------------
// server + rigs (same shape as tests/smoke_host_viewer.rs)
// ---------------------------------------------------------------------------

async fn start_in_process_server() -> (String, Arc<tokio::sync::Notify>) {
    use locast_server::{
        AppState, Clock, Config, Db, Metrics, RoomRegistry, RoomRegistryConfig, SystemClock,
    };
    let config = Config::from_env().expect("config");
    let db = Db::open(&config).await.expect("open db");
    let rooms = Arc::new(RoomRegistry::new(RoomRegistryConfig::from_config(&config)));
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let state = AppState {
        config: Arc::new(config),
        metrics: Metrics::new(),
        db,
        rooms,
        clock,
        signal_relay: Default::default(),
        epoch_counter: Arc::new(std::sync::Mutex::new(
            locast_server::auth::EpochCounter::default(),
        )),
    };
    let app: axum::Router = locast_server::router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    let cancel = Arc::new(tokio::sync::Notify::new());
    let cancel_for_task = cancel.clone();
    tokio::spawn(async move {
        let server = axum::serve(listener, app).with_graceful_shutdown(async move {
            cancel_for_task.notified().await;
        });
        let _ = server.await;
    });
    (format!("ws://{addr}/ws"), cancel)
}

struct ClientRig {
    _dir: TempDir,
    storage: Storage,
    library_root: PathBuf,
    identity: Arc<IdentityService>,
    signaling: Arc<SignalingClient>,
    room: Arc<RoomClient>,
    webrtc: Arc<WebRtcManager>,
    registry: Arc<TransferRegistry>,
    pubkey: [u8; 32],
    user_id: String,
}

async fn build_rig(server_url: &str) -> ClientRig {
    let dir = TempDir::new().expect("tempdir");
    let storage = Storage::open(&dir.path().join("index.sqlite"))
        .await
        .expect("storage open");
    let library_root = dir.path().join("library");
    std::fs::create_dir_all(&library_root).expect("library root");
    let keyring: Arc<dyn IdentityKeyring> = Arc::new(MockKeyring::new());
    let identity = Arc::new(IdentityService::with_keyring(keyring, storage.clone()));
    identity
        .get_or_create("concurrent-user")
        .await
        .expect("identity get_or_create");
    let kp = identity.load_keypair().await.expect("load_keypair");
    let pubkey = kp.signing.verifying_key().to_bytes();
    let user_id = identity.ensure_user_row().await.expect("ensure_user_row");
    let config = SignalingConfig::new_for_test(
        server_url.to_string(),
        Duration::from_secs(15),
        1024 * 1024,
        Platform::Linux,
    );
    let signaling = Arc::new(SignalingClient::new(config, identity.clone()));
    let room = Arc::new(RoomClient::new(signaling.clone()));
    room.set_storage_pool(storage.pool());
    let webrtc = Arc::new(WebRtcManager::new(
        signaling.clone(),
        identity.clone(),
        room.clone(),
    ));
    ClientRig {
        _dir: dir,
        storage,
        library_root,
        identity,
        signaling,
        room,
        webrtc,
        registry: Arc::new(TransferRegistry::new()),
        pubkey,
        user_id,
    }
}

async fn wait_for_phase(client: &SignalingClient, target: ConnPhase, timeout: Duration) {
    let start = Instant::now();
    loop {
        let s = client.snapshot().await;
        if s.phase == target {
            return;
        }
        assert!(
            start.elapsed() < timeout,
            "timed out waiting for {target:?}; last = {s:?}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Seed a deterministic fixture at the canonical content-addressed
/// path plus a `permanent` `media_items` row on `rig`. Returns
/// `(sha256_hex, media_id)`.
async fn seed_fixture(rig: &ClientRig, seed: u64) -> (String, String) {
    let mut state: u64 = seed | 1;
    let mut bytes = Vec::with_capacity(FIXTURE_SIZE + 8);
    while bytes.len() < FIXTURE_SIZE {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        bytes.extend_from_slice(&state.to_le_bytes());
    }
    bytes.truncate(FIXTURE_SIZE);
    let sha_hex = hex::encode(Sha256::digest(&bytes));
    let blake_hex = blake3::hash(&bytes).to_hex().to_string();

    let cap = paths::content_addressed_path(&rig.library_root, &sha_hex, FILENAME)
        .expect("content_addressed_path");
    tokio::fs::create_dir_all(cap.parent().expect("cap parent"))
        .await
        .expect("create cap parent");
    tokio::fs::write(&cap, &bytes).await.expect("write fixture");
    let rel = format!(
        "library/{}/{}/{}/{}",
        &sha_hex[0..2],
        &sha_hex[2..4],
        sha_hex,
        FILENAME
    );
    let media_id = Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO media_items (\
            id, sha256, blake3, size_bytes, filename, relative_path, mime, \
            duration_ms, width, height, video_codec, audio_codec, container, \
            status, created_at, last_seen_at, last_room_id, source_url, provenance\
         ) VALUES (\
            ?1, ?2, ?3, ?4, ?5, ?6, 'application/octet-stream', \
            NULL, NULL, NULL, NULL, NULL, NULL, \
            'permanent', 1, 1, NULL, NULL, '{}'\
         )",
    )
    .bind(&media_id)
    .bind(&sha_hex)
    .bind(&blake_hex)
    .bind(FIXTURE_SIZE as i64)
    .bind(FILENAME)
    .bind(&rel)
    .execute(&rig.storage.pool())
    .await
    .expect("insert media_items");
    (sha_hex, media_id)
}

/// Record `count` guests who joined the viewer's room earlier and left.
async fn add_past_guests(storage: &Storage, room_id: Uuid, count: usize) {
    let pool = storage.pool();
    let mut tx = pool.begin().await.expect("begin");
    for i in 0..count {
        let user_id = Uuid::new_v4().to_string();
        let pubkey = Sha256::digest(format!("past-guest-{room_id}-{i}").as_bytes());
        sqlx::query(
            "INSERT INTO user_identities (id, public_key, display_name, created_at, last_seen) \
             VALUES (?1, ?2, ?3, 1, 1)",
        )
        .bind(&user_id)
        .bind(base64::engine::general_purpose::STANDARD.encode(pubkey))
        .bind(format!("guest-{i}"))
        .execute(&mut *tx)
        .await
        .expect("insert past guest identity");
        sqlx::query(
            "INSERT INTO room_participants \
                 (id, room_id, user_id, display_name, role, joined_at, left_at, connection_state) \
             VALUES (?1, ?2, ?3, ?4, 'guest', 1, 2, 'left')",
        )
        .bind(Uuid::new_v4().to_string())
        .bind(room_id.to_string())
        .bind(&user_id)
        .bind(format!("guest-{i}"))
        .execute(&mut *tx)
        .await
        .expect("insert past guest participant");
    }
    tx.commit().await.expect("commit");
}

/// A host and a viewer in one room, the viewer holding the verified
/// manifest for one fixture with the host's `files` DataChannel open.
struct Harness {
    server_cancel: Arc<tokio::sync::Notify>,
    _host: ClientRig,
    viewer: ClientRig,
    manifest: MediaManifest,
    room_id: Uuid,
    host_server_id: String,
    sha256: String,
    media_id: String,
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.server_cancel.notify_waiters();
    }
}

async fn setup(seed: u64) -> Harness {
    let (server_url, server_cancel) = start_in_process_server().await;
    let host = build_rig(&server_url).await;
    let viewer = build_rig(&server_url).await;
    assert_ne!(host.user_id, viewer.user_id, "rigs must be distinct");

    let dispatch = HostSenderDispatcher::new(HostDispatchContext::new(
        host.storage.clone(),
        host.library_root.clone(),
        host.room.clone(),
        host.pubkey,
        host.webrtc.cancel_token().clone(),
    ));
    host.webrtc.set_host_dispatch(dispatch);

    for rig in [&host, &viewer] {
        rig.signaling.start().await.expect("signaling start");
    }
    for rig in [&host, &viewer] {
        wait_for_phase(
            &rig.signaling,
            ConnPhase::Authenticated,
            Duration::from_secs(10),
        )
        .await;
        rig.room.init().await;
        let room = rig.room.clone();
        tokio::spawn(async move { room.run_inbound().await });
        rig.webrtc.clone().start_with_room_client(rig.room.clone());
    }

    let (sha256, media_id) = seed_fixture(&host, seed).await;
    let summary = host
        .room
        .room_create("concurrent-room".into(), false)
        .await
        .expect("room_create");
    let room_id = Uuid::parse_str(&summary.id).expect("room id");
    let invite = build_invite_url("locast", &summary.code, host.pubkey).expect("invite url");
    let anchor = invite_anchor_for(&summary.code, Some(&invite))
        .expect("invite anchor")
        .expect("invite carries the host key");
    viewer.room.set_expected_host_pubkey(anchor);
    viewer
        .room
        .room_join(summary.code.clone(), "concurrent-viewer".into())
        .await
        .expect("room_join");
    build_sign_and_publish_selected(
        host.identity.clone(),
        host.signaling.clone(),
        host.room.clone(),
        host.storage.pool(),
        host.library_root.clone(),
        room_id,
        Some(vec![media_id.clone()]),
    )
    .await
    .expect("publish manifest");

    // The viewer holds the verified manifest ...
    let deadline = Instant::now() + Duration::from_secs(15);
    let manifest = loop {
        if let Some(m) = viewer.room.verified_manifest(room_id) {
            break m;
        }
        assert!(Instant::now() < deadline, "manifest never verified");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(manifest.media.len(), 1, "manifest carries the fixture");
    assert_eq!(manifest.media[0].id, media_id);
    let source_peer = manifest.media[0].sources[0].peer_id.clone();

    // ... and the host's `files` DataChannel is open, so the concurrent
    // opens do not take the "no source transport yet" early return.
    let host_pubkey = host.pubkey;
    let deadline = Instant::now() + Duration::from_secs(20);
    while viewer
        .webrtc
        .lookup_dc_by_peer_id(&source_peer, move |_| Some(host_pubkey))
        .await
        .is_none()
    {
        assert!(Instant::now() < deadline, "files DataChannel never opened");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let host_server_id = viewer
        .room
        .state()
        .await
        .map(|s| s.host_user_id)
        .expect("viewer room state");
    add_past_guests(&viewer.storage, room_id, PAST_GUESTS).await;

    Harness {
        server_cancel,
        _host: host,
        viewer,
        manifest,
        room_id,
        host_server_id,
        sha256,
        media_id,
    }
}

impl Harness {
    /// One `download_open`, with a fresh download id like the command
    /// mints.
    async fn open(&self, manifest: MediaManifest) -> Result<DownloadSessionIpc, AppError> {
        let download_id = Uuid::new_v4().to_string();
        open_download_inner(
            manifest,
            self.room_id,
            &self.host_server_id,
            &self.viewer.user_id,
            &self.viewer.storage,
            &self.viewer.library_root,
            &self.media_id,
            &download_id,
            &self.viewer.webrtc,
            &self.viewer.registry,
            self.viewer.identity.clone(),
        )
        .await
    }

    /// Fire two overlapping opens, let the transfer finish, and return
    /// every violated expectation.
    async fn double_open(&self, label: &str) -> Vec<String> {
        let cap = capture();
        let mut failures: Vec<String> = Vec::new();
        let (a, b) = tokio::join!(
            self.open(self.manifest.clone()),
            self.open(self.manifest.clone())
        );
        eprintln!("{label}: open A = {a:?}");
        eprintln!("{label}: open B = {b:?}");
        for (name, r) in [("A", &a), ("B", &b)] {
            if let Err(e) = r {
                failures.push(format!("download_open {name} returned Err: {e}"));
            }
        }
        let ok: Vec<&DownloadSessionIpc> = [&a, &b].into_iter().flatten().collect();
        let Some(first) = ok.first() else {
            return failures;
        };
        let id = first.download_id.clone();
        if ok.iter().any(|r| r.download_id != id) {
            failures.push(format!(
                "the opens returned different download ids: {:?}",
                ok.iter().map(|r| &r.download_id).collect::<Vec<_>>()
            ));
        }
        if ok.iter().any(|r| r.dedup_hit) || !ok.iter().any(|r| r.transfer_started) {
            failures.push(format!(
                "expected a started transfer and no dedup hit: {ok:?}"
            ));
        }

        // Wait for the shared row to end. On a non-complete end, give a
        // second orchestrator time to end as well so its outcome shows.
        let store = DownloadStore::new(self.viewer.storage.pool());
        let start = Instant::now();
        let mut ended_at: Option<Instant> = None;
        loop {
            let state = store.fetch(&id).await.expect("downloads row").state;
            let terminal = matches!(
                state,
                DownloadState::Complete | DownloadState::Failed | DownloadState::Cancelled
            );
            if terminal && ended_at.is_none() {
                ended_at = Some(Instant::now());
            }
            let settled = match ended_at {
                Some(t) if state == DownloadState::Complete => t.elapsed() > Duration::from_secs(1),
                Some(t) => cap.logs.outcomes(&id).len() >= 2 || t.elapsed() > STRAGGLER_BUDGET,
                None => false,
            };
            if settled || start.elapsed() > TRANSFER_BUDGET {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        // The RegisterGuard unregisters from a spawned task.
        let start = Instant::now();
        while self.viewer.registry.is_active(&id).await && start.elapsed() < Duration::from_secs(5)
        {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        let cancelled_rows: i64 =
            sqlx::query_as::<_, (i64,)>("SELECT COUNT(*) FROM downloads WHERE state = 'cancelled'")
                .fetch_one(&self.viewer.storage.pool())
                .await
                .expect("count cancelled")
                .0;
        if cancelled_rows != 0 {
            failures.push(format!(
                "{cancelled_rows} downloads row(s) reached 'cancelled'"
            ));
        }

        let cancelled_events = cap
            .events
            .states
            .lock()
            .expect("states lock")
            .iter()
            .filter(|e| e.id == id && e.state == "cancelled")
            .count();
        if cancelled_events != 0 {
            failures.push(format!(
                "{cancelled_events} download://state 'cancelled' event(s) emitted"
            ));
        }

        let final_row: Option<(String, Option<String>, i64)> = sqlx::query_as(
            "SELECT state, last_error, transferred_bytes FROM downloads WHERE id = ?1",
        )
        .bind(&id)
        .fetch_optional(&self.viewer.storage.pool())
        .await
        .expect("final row");
        match &final_row {
            Some((state, _, _)) if state == "complete" => {}
            other => failures.push(format!(
                "final row is not complete: (state, last_error, transferred) = {other:?}"
            )),
        }

        let outcomes = cap.logs.outcomes(&id);
        let completes = outcomes
            .iter()
            .filter(|l| l.contains("download complete"))
            .count();
        if outcomes.len() != 1 || completes != 1 {
            failures.push(format!(
                "expected exactly one orchestrator ending in 'download complete', got {}:\n    {}",
                outcomes.len(),
                outcomes.join("\n    ")
            ));
        }

        let on_disk =
            paths::content_addressed_path(&self.viewer.library_root, &self.sha256, FILENAME)
                .expect("content_addressed_path");
        match tokio::fs::read(&on_disk).await {
            Ok(bytes) => {
                let got = hex::encode(Sha256::digest(&bytes));
                if got != self.sha256 {
                    failures.push(format!("installed sha256 {got} != source {}", self.sha256));
                }
            }
            Err(e) => failures.push(format!(
                "installed file missing at {}: {e}",
                on_disk.display()
            )),
        }

        if self.viewer.registry.is_active(&id).await {
            failures.push("registry still reports an active transfer".to_string());
        }
        failures
    }
}

fn assert_no_violations(label: &str, failures: &[String]) {
    assert!(
        failures.is_empty(),
        "{label}: concurrent download_open regression ({} violation(s)):\n- {}",
        failures.len(),
        failures.join("\n- ")
    );
}

// ---------------------------------------------------------------------------
// the tests
// ---------------------------------------------------------------------------

/// The media was opened once while no source DataChannel was up, which
/// left a `pending` row. Two concurrent retries must start ONE transfer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retry_double_open_runs_one_transfer() {
    if !webrtc_tests_enabled() {
        eprintln!("skipping (RUN_WEBRTC_TESTS != 1)");
        return;
    }
    capture();
    let h = setup(0x9E37_79B9_7F4A_7C15).await;

    let mut unreachable = h.manifest.clone();
    for m in &mut unreachable.media {
        for s in &mut m.sources {
            s.peer_id = NOBODY_PEER_ID.to_string();
        }
    }
    let earlier = h.open(unreachable).await.expect("earlier open");
    assert!(
        earlier.state == "pending" && !earlier.transfer_started && !earlier.dedup_hit,
        "the earlier open should leave a pending row and no transfer: {earlier:?}"
    );

    let failures = h.double_open("retry").await;
    assert_no_violations("retry", &failures);
}

/// Nothing was opened before: the two concurrent opens also race to
/// create the downloads row. They must still share ONE row and ONE
/// transfer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn first_double_open_runs_one_transfer() {
    if !webrtc_tests_enabled() {
        eprintln!("skipping (RUN_WEBRTC_TESTS != 1)");
        return;
    }
    capture();
    let h = setup(0xC2B2_AE3D_27D4_EB4F).await;
    let failures = h.double_open("first").await;
    assert_no_violations("first", &failures);
}
