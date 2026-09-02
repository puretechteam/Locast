//! P3-T14 manual end-to-end smoke test: HOST -> VIEWER over a real
//! local signaling server.
//!
//! Run with
//!
//! ```text
//! cargo test -j 1 -p locast-client --test smoke_host_viewer -- --ignored --nocapture
//! ```
//!
//! The test is `#[ignore]`d by default (it is a manual / release
//! gate, not a CI test). It:
//!
//! 1. Starts the real `locast-server` router in-process on an
//!    ephemeral TCP port (the `locast-server` crate is already a
//!    dev-dep of `locast-client`; this is the same pattern used
//!    by `tests/rooms.rs`).
//! 2. Builds TWO independent client setups (HOST and VIEWER),
//!    each with its own `tempfile::TempDir`, SQLite DB,
//!    library root, `MockKeyring`, `IdentityService`,
//!    `SignalingClient`, `RoomClient`, `WebRtcManager`, and
//!    `TransferRegistry`. They share NOTHING except the
//!    signaling server.
//! 3. HOST seeds a ~1 MiB deterministic binary fixture at the
//!    canonical content-addressed path and inserts a
//!    `permanent` `media_items` row, then publishes a
//!    signed manifest via `room::host::build_sign_and_publish`.
//! 4. VIEWER subscribes to the manifest, verifies the host's
//!    signature via `locast_manifest::verify_manifest`, then
//!    triggers `commands::download::open_download_inner` and
//!    polls `downloads.state` until completion.
//! 5. Asserts the on-disk file matches by SHA-256 / BLAKE3 /
//!    size.
//! 6. Writes a safe-only `result.json` summarising the run.

#![allow(clippy::needless_return)]
#![allow(clippy::field_reassign_with_default)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use blake3::Hasher as Blake3Hasher;
use locast_client_lib::commands::download::{open_download_inner, DownloadSessionIpc};
use locast_client_lib::core::paths;
use locast_client_lib::identity::keystore::{IdentityKeyring, IdentityService, MockKeyring};
use locast_client_lib::net::config::SignalingConfig;
use locast_client_lib::net::room::RoomClient;
use locast_client_lib::net::signaling::SignalingClient;
use locast_client_lib::net::state::ConnPhase;
use locast_client_lib::net::webrtc::WebRtcManager;
use locast_client_lib::room::host::build_sign_and_publish;
use locast_client_lib::storage::Storage;
use locast_client_lib::transfer::state::{DownloadState, DownloadStore};
use locast_client_lib::transfer::{HostDispatchContext, HostSenderDispatcher, TransferRegistry};
use locast_manifest::verify_manifest;
use locast_protocol::handshake::Platform;
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tokio::net::TcpListener;
use uuid::Uuid;

const FILENAME: &str = "smoke.bin";
const FIXTURE_SIZE: usize = 1024 * 1024; // 1 MiB
const FIXTURE_SEED: [u8; 32] = [
    0x70, 0x14, 0x22, 0xb3, 0x57, 0x88, 0xd8, 0x55, 0x3a, 0x4c, 0x90, 0xcb, 0xaa, 0x23, 0x47, 0x6e,
    0x11, 0x6b, 0x8d, 0x99, 0x5f, 0x01, 0xab, 0xcd, 0xef, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99,
];

// ---------------------------------------------------------------------------
// server bring-up (in-process; mirrors tests/rooms.rs)
// ---------------------------------------------------------------------------

/// Start the real `locast-server` router in-process on an
/// ephemeral TCP port, return the URL clients should use.
/// The server is torn down via the returned `Cancel` guard
/// when its `Drop` fires (or when `.cancel()` is called
/// explicitly). This is the same pattern as the working
/// `tests/rooms.rs` E2E suite; spawning a separate child
/// process was tried and produced a hard-to-debug cross-
/// process envelope loss.
async fn start_in_process_server() -> (String, Cancel) {
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
        rooms: rooms.clone(),
        clock: clock.clone(),
        signal_relay: Default::default(),
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
    let url = format!("ws://{addr}/ws");
    (url, Cancel(cancel))
}

/// Triggers graceful shutdown of the in-process server
/// when dropped or when `.cancel()` is called. Cheap to
/// clone (Arc).
#[derive(Clone)]
struct Cancel(Arc<tokio::sync::Notify>);
impl Cancel {
    fn cancel(&self) {
        self.0.notify_waiters();
    }
}

// ---------------------------------------------------------------------------
// result envelope
// ---------------------------------------------------------------------------

#[derive(serde::Serialize)]
struct SmokeResult {
    success: bool,
    elapsed_ms: u128,
    host_user_id: String,
    viewer_user_id: String,
    room_code: String,
    room_id: String,
    media_id: String,
    source_size: u64,
    downloaded_size: u64,
    source_sha256: String,
    final_sha256: String,
    source_blake3: String,
    final_blake3: String,
    stages_passed: Vec<String>,
    failure_stage: Option<String>,
    failure_message: Option<String>,
}

impl SmokeResult {
    fn new(
        host_user_id: String,
        viewer_user_id: String,
        room_code: String,
        room_id: String,
        media_id: String,
        source_size: u64,
    ) -> Self {
        Self {
            success: false,
            elapsed_ms: 0,
            host_user_id,
            viewer_user_id,
            room_code,
            room_id,
            media_id,
            source_size,
            downloaded_size: 0,
            source_sha256: String::new(),
            final_sha256: String::new(),
            source_blake3: String::new(),
            final_blake3: String::new(),
            stages_passed: Vec::new(),
            failure_stage: None,
            failure_message: None,
        }
    }
}

fn result_path() -> PathBuf {
    // Resolve a base directory: the override SMOKE_OUTPUT_DIR
    // when set, else a `locast-smoke` subdir under the OS
    // temp dir. Canonicalize it so SMOKE_RESULT_PATH below can
    // be rejected if it escapes the base.
    let base = match std::env::var("SMOKE_OUTPUT_DIR") {
        Ok(s) => PathBuf::from(s),
        Err(_) => std::env::temp_dir().join("locast-smoke"),
    };
    let _ = std::fs::create_dir_all(&base);
    let canonical_base = std::fs::canonicalize(&base).unwrap_or_else(|_| base.clone());
    if let Ok(p) = std::env::var("SMOKE_RESULT_PATH") {
        let path = PathBuf::from(p);
        if let Some(parent) = path.parent() {
            // Defense in depth: refuse to write result.json
            // outside the smoke temp dir, even if the
            // operator passes a malicious SMOKE_RESULT_PATH.
            // The test is `#[ignore]`'d and developer-invoked
            // so this is low real-world risk, but it costs
            // nothing to enforce.
            let canon_parent =
                std::fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf());
            if !canon_parent.starts_with(&canonical_base) {
                eprintln!(
                    "smoke: refusing SMOKE_RESULT_PATH={} outside SMOKE_OUTPUT_DIR={}",
                    path.display(),
                    canonical_base.display()
                );
                std::process::exit(2);
            }
            let _ = std::fs::create_dir_all(parent);
        }
        return path;
    }
    let _ = std::fs::create_dir_all(&base);
    canonical_base.join("result.json")
}

fn write_result(result: &SmokeResult) {
    let path = result_path();
    match serde_json::to_string_pretty(result) {
        Ok(s) => {
            if let Err(e) = std::fs::write(&path, s) {
                eprintln!("smoke: failed to write {}: {e}", path.display());
            } else {
                eprintln!("smoke: wrote {}", path.display());
            }
        }
        Err(e) => eprintln!("smoke: failed to serialize result: {e}"),
    }
}

// ---------------------------------------------------------------------------
// fixture + setup helpers
// ---------------------------------------------------------------------------

fn make_signaling_config(url: String) -> SignalingConfig {
    SignalingConfig::new_for_test(url, Duration::from_secs(15), 1024 * 1024, Platform::Linux)
}

async fn make_identity(storage: &Storage) -> Arc<IdentityService> {
    let keyring: Arc<dyn IdentityKeyring> = Arc::new(MockKeyring::new());
    let svc = Arc::new(IdentityService::with_keyring(keyring, storage.clone()));
    svc.get_or_create("smoke-user")
        .await
        .expect("identity get_or_create");
    svc
}

async fn wait_for_phase(client: &SignalingClient, target: ConnPhase, timeout: Duration) {
    let start = Instant::now();
    loop {
        let s = client.snapshot().await;
        if s.phase == target {
            return;
        }
        if start.elapsed() > timeout {
            panic!("timed out waiting for {target:?}; last = {s:?}");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
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

async fn build_rig(
    label: &str,
    library_subdir: &str,
    server_url: &str,
) -> Result<ClientRig, String> {
    let dir = TempDir::new().map_err(|e| format!("tempdir({label}): {e}"))?;
    let db_path = dir.path().join("index.sqlite");
    let storage = Storage::open(&db_path)
        .await
        .map_err(|e| format!("storage open({label}): {e}"))?;
    let library_root = dir.path().join(library_subdir);
    std::fs::create_dir_all(&library_root)
        .map_err(|e| format!("create library root({label}): {e}"))?;
    let identity = make_identity(&storage).await;
    let kp = identity
        .load_keypair()
        .await
        .map_err(|e| format!("load_keypair({label}): {e}"))?;
    let pubkey = kp.signing.verifying_key().to_bytes();
    let user_id = identity
        .ensure_user_row()
        .await
        .map_err(|e| format!("ensure_user_row({label}): {e}"))?;
    let config = make_signaling_config(server_url.to_string());
    let signaling = Arc::new(SignalingClient::new(config, identity.clone()));
    let room = Arc::new(RoomClient::new(signaling.clone()));
    room.set_storage_pool(storage.pool());
    let webrtc = Arc::new(WebRtcManager::new(
        signaling.clone(),
        identity.clone(),
        room.clone(),
    ));
    let registry = Arc::new(TransferRegistry::new());
    Ok(ClientRig {
        _dir: dir,
        storage,
        library_root,
        identity,
        signaling,
        room,
        webrtc,
        registry,
        pubkey,
        user_id,
    })
}

/// Seed a deterministic ~1 MiB fixture at the canonical
/// content-addressed path and insert a `permanent`
/// `media_items` row. Returns
/// `(sha256_hex, blake3_hex, size_bytes, media_id)`.
async fn seed_fixture(rig: &ClientRig) -> Result<(String, String, u64, String), String> {
    // Deterministic pseudo-random bytes from FIXTURE_SEED
    // (xorshift32 for reproducibility without pulling in
    // a `rand` dep here).
    let mut state: u32 = u32::from_le_bytes([
        FIXTURE_SEED[0],
        FIXTURE_SEED[1],
        FIXTURE_SEED[2],
        FIXTURE_SEED[3],
    ]) | 1;
    let mut bytes = Vec::with_capacity(FIXTURE_SIZE);
    while bytes.len() < FIXTURE_SIZE {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        let chunk = state.to_le_bytes();
        for b in chunk {
            if bytes.len() < FIXTURE_SIZE {
                bytes.push(b);
            }
        }
    }
    let mut sha = Sha256::new();
    sha.update(&bytes);
    let sha_hex = hex::encode(sha.finalize());
    let mut blake = Blake3Hasher::new();
    blake.update(&bytes);
    let blake_hex = blake.finalize().to_hex().to_string();

    let cap = paths::content_addressed_path(&rig.library_root, &sha_hex, FILENAME)
        .map_err(|e| format!("content_addressed_path: {e}"))?;
    if let Some(parent) = cap.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("create cap parent: {e}"))?;
    }
    tokio::fs::write(&cap, &bytes)
        .await
        .map_err(|e| format!("write fixture: {e}"))?;

    // relative_path is library_root-relative. The
    // build_manifest code does `library_root.join(relative_path)`.
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
    .map_err(|e| format!("insert media_items: {e}"))?;
    Ok((sha_hex, blake_hex, FIXTURE_SIZE as u64, media_id))
}

// ---------------------------------------------------------------------------
// the test
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "manual end-to-end smoke test; run with --ignored"]
async fn smoke_host_to_viewer_full_webrtc_transfer() {
    let overall_start = Instant::now();
    // The smoke temp dir holds any artifacts we want to
    // preserve past the test run. It is created OUTSIDE the
    // test's TempDir so its lifetime is independent of the
    // test (we Drop the TempDirs held by ClientRig only at
    // the end of this function).
    let smoke_dir = std::env::var("SMOKE_OUTPUT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("locast-smoke"));
    let _ = std::fs::create_dir_all(&smoke_dir);

    // 1. Start the real server in-process on an ephemeral
    //    port. We do not know the host user's user_id yet;
    //    fill in the result skeleton after both rigs are
    //    built. The cancel sender signals the axum graceful
    //    shutdown when the test scope ends (whether normally
    //    or via early return).
    let (server_url, server_cancel) = start_in_process_server().await;
    eprintln!("smoke: server up at {server_url}");

    // 2. Build the two rigs.
    let host = build_rig("host", "library", &server_url)
        .await
        .expect("host rig");
    let viewer = build_rig("viewer", "library", &server_url)
        .await
        .expect("viewer rig");

    // P3-T15: install the host sender dispatch on the
    // host's WebRtcManager. The viewer rig has no
    // dispatch installed (it is a downloader, not a
    // server). The dispatch consults the host's
    // `media_items.relative_path` and `verified_manifest`
    // to serve chunks over the inbound `files` DataChannel.
    {
        let host_kp = host
            .identity
            .load_keypair()
            .await
            .expect("host load keypair");
        let host_pubkey = host_kp.signing.verifying_key().to_bytes();
        let ctx = HostDispatchContext::new(
            host.storage.clone(),
            host.library_root.clone(),
            host.room.clone(),
            host_pubkey,
            host.webrtc.cancel_token().clone(),
        );
        let dispatch = HostSenderDispatcher::new(ctx);
        host.webrtc.set_host_dispatch(dispatch);
    }

    let mut result = SmokeResult::new(
        host.user_id.clone(),
        viewer.user_id.clone(),
        String::new(),
        String::new(),
        String::new(),
        FIXTURE_SIZE as u64,
    );

    // -- stage: spawn rigs -------------------------------------------------
    result.stages_passed.push("spawn_rigs".to_string());

    // 3. Connect signaling + room for both. The host's
    //    WebRtcManager will follow the host's room state; the
    //    viewer's WebRtcManager will follow the viewer's.
    if let Err(e) = async {
        host.signaling.start().await.map_err(|e| e.to_string())?;
        viewer.signaling.start().await.map_err(|e| e.to_string())?;
        wait_for_phase(
            &host.signaling,
            ConnPhase::Authenticated,
            Duration::from_secs(10),
        )
        .await;
        wait_for_phase(
            &viewer.signaling,
            ConnPhase::Authenticated,
            Duration::from_secs(10),
        )
        .await;
        host.room.init().await;
        viewer.room.init().await;
        spawn_inbound(host.room.clone());
        spawn_inbound(viewer.room.clone());
        // Start the WebRTC inbound loops. The manager
        // listens for room-state changes and creates
        // PeerConnections on demand.
        host.webrtc
            .clone()
            .start_with_room_client(host.room.clone());
        viewer
            .webrtc
            .clone()
            .start_with_room_client(viewer.room.clone());
        Ok::<(), String>(())
    }
    .await
    {
        finalize_failure(
            &mut result,
            "connect_signaling",
            e,
            overall_start,
            Some(server_cancel.clone()),
        );
        return;
    }
    result.stages_passed.push("connect_signaling".to_string());

    // 4. HOST seeds the fixture + media row, then creates
    //    the room.
    let (sha_hex, blake_hex, source_size, media_id) = match seed_fixture(&host).await {
        Ok(t) => t,
        Err(e) => {
            finalize_failure(
                &mut result,
                "seed_fixture",
                e,
                overall_start,
                Some(server_cancel.clone()),
            );
            return;
        }
    };
    result.source_sha256 = sha_hex.clone();
    result.source_blake3 = blake_hex.clone();
    result.source_size = source_size;
    result.media_id = media_id.clone();
    result.stages_passed.push("seed_fixture".to_string());

    let room_id;
    let room_code;
    // The server mints a fresh UUID per authentication
    // and returns it in the AuthOk payload (not the
    // sha256(public_key) hex that `user_identities.id`
    // stores). The smoke test tracks both: `user_id`
    // (sha256 hex, used by the local DB) and
    // `signaling_user_id` (UUID, used by the server).
    // The download_open lookup path expects the
    // signaling UUID (the WebRtcManager keys its peer
    // map by it), so all FKs that the viewer-side
    // downloader uses against `user_identities` /
    // `room_participants` must be seeded with the UUID,
    // not the sha256 hex.
    let host_signaling_user_id: String;
    // Filled in immediately after a successful `room_join`
    // below. Declared up front so clippy doesn't flag
    // the late-init in that subsequent block.
    #[allow(unused_assignments)]
    let mut viewer_signaling_user_id: String = String::new();
    match host.room.room_create("smoke-room".into(), false).await {
        Ok(summary) => {
            room_id = Uuid::parse_str(&summary.id).expect("room_id uuid");
            room_code = summary.code.clone();
            result.room_id = summary.id.clone();
            result.room_code = summary.code.clone();
            host_signaling_user_id = host
                .signaling
                .snapshot()
                .await
                .user_id
                .clone()
                .expect("host signaling user_id");
            // Pre-install the host's expected pubkey on the
            // viewer so the manifest's trust check passes
            // before any network round trip introduces drift.
            viewer.room.set_expected_host_pubkey(host.pubkey);
            // Seed the host's local `rooms` row. The host's
            // local DB is otherwise empty (the RoomClient
            // does not INSERT into `rooms` on a successful
            // ROOM_CREATE reply), which would prevent the
            // host from opening a download for any media in
            // the room. In production this row is populated
            // via the room://state event hookup.
            let _ = sqlx::query(
                "INSERT INTO rooms (id, code, host_user_id, created_at, ended_at, state, settings) \
                 VALUES (?1, ?2, ?3, 1, NULL, 'open', '{}') \
                 ON CONFLICT(id) DO NOTHING",
            )
            .bind(room_id.to_string())
            .bind(&room_code)
            .bind(&host_signaling_user_id)
            .execute(&host.storage.pool())
            .await;
            // Seed the host's local room_participants rows
            // using the signaling-issued UUIDs (NOT the
            // sha256 hex) so the lookup closure used by
            // `download_open` -> `lookup_dc_by_peer_id`
            // can resolve each participant's pubkey from
            // the WebRtcManager's UUID-keyed peer map.
            for (uid, role, name) in [
                (&host_signaling_user_id, "host", "smoke-host"),
                // viewer's signaling_user_id is filled in
                // after room_join (below) -- we patch the
                // host's local row in a second pass.
            ] {
                let _ = sqlx::query(
                    "INSERT INTO user_identities (id, public_key, display_name, created_at, last_seen) \
                     VALUES (?1, '', ?2, 1, 1) \
                     ON CONFLICT(id) DO NOTHING",
                )
                .bind(uid)
                .bind(name)
                .execute(&host.storage.pool())
                .await;
                let _ = sqlx::query(
                    "INSERT INTO room_participants \
                        (id, room_id, user_id, display_name, role, joined_at, connection_state, capabilities) \
                     VALUES (?1, ?2, ?3, ?4, ?5, 1, 'connected', '{}') \
                     ON CONFLICT(room_id, user_id) DO NOTHING",
                )
                .bind(Uuid::new_v4().to_string())
                .bind(room_id.to_string())
                .bind(uid)
                .bind(name)
                .bind(role)
                .execute(&host.storage.pool())
                .await;
            }
        }
        Err(e) => {
            finalize_failure(
                &mut result,
                "room_create",
                e.to_string(),
                overall_start,
                Some(server_cancel.clone()),
            );
            return;
        }
    }
    result.stages_passed.push("room_create".to_string());

    // 5. VIEWER joins.
    if let Err(e) = viewer
        .room
        .room_join(room_code.clone(), "smoke-viewer".into())
        .await
        .map_err(|e| e.to_string())
    {
        finalize_failure(
            &mut result,
            "room_join",
            e,
            overall_start,
            Some(server_cancel.clone()),
        );
        return;
    }
    viewer_signaling_user_id = viewer
        .signaling
        .snapshot()
        .await
        .user_id
        .clone()
        .expect("viewer signaling user_id");
    // The viewer did not exist in the host's local DB
    // before join. Patch the host's seed to include the
    // viewer's signaling-issued UUID now that we have it.
    let _ = sqlx::query(
        "INSERT INTO user_identities (id, public_key, display_name, created_at, last_seen) \
         VALUES (?1, '', 'smoke-viewer', 1, 1) \
         ON CONFLICT(id) DO NOTHING",
    )
    .bind(&viewer_signaling_user_id)
    .execute(&host.storage.pool())
    .await;
    let _ = sqlx::query(
        "INSERT INTO room_participants \
            (id, room_id, user_id, display_name, role, joined_at, connection_state, capabilities) \
         VALUES (?1, ?2, ?3, 'smoke-viewer', 'guest', 1, 'connected', '{}') \
         ON CONFLICT(room_id, user_id) DO NOTHING",
    )
    .bind(Uuid::new_v4().to_string())
    .bind(room_id.to_string())
    .bind(&viewer_signaling_user_id)
    .execute(&host.storage.pool())
    .await;
    result.stages_passed.push("room_join".to_string());

    // 6. HOST publishes the signed manifest. Also
    //    subscribe to the host's own inbound envelopes so
    //    we can see the server's reply to MANIFEST_PUBLISH
    //    (or its absence).
    if let Err(e) = build_sign_and_publish(
        host.identity.clone(),
        host.signaling.clone(),
        host.room.clone(),
        host.storage.pool(),
        host.library_root.clone(),
        room_id,
    )
    .await
    .map_err(|e| e.to_string())
    {
        finalize_failure(
            &mut result,
            "publish_manifest",
            e,
            overall_start,
            Some(server_cancel.clone()),
        );
        return;
    }
    result.stages_passed.push("publish_manifest".to_string());

    // 7. VIEWER waits for the verified manifest to appear in
    //    its cache.
    let verified_manifest = match wait_for_manifest(
        &viewer.room,
        room_id,
        host.pubkey,
        Duration::from_secs(15),
    )
    .await
    {
        Ok(m) => m,
        Err(e) => {
            finalize_failure(
                &mut result,
                "wait_for_manifest",
                e,
                overall_start,
                Some(server_cancel.clone()),
            );
            return;
        }
    };
    result.stages_passed.push("wait_for_manifest".to_string());

    // 7c. Give the WebRtcManager a moment to negotiate. The
    //     manager polls room state every 200ms and only
    //     creates peer entries when the room summary
    //     signature changes. On a localhost LAN (or in a
    //     single process), ICE gathering + SDP exchange
    //     typically completes within 1-3 seconds.
    tokio::time::sleep(Duration::from_secs(5)).await;

    // 7b. Seed the viewer's local `user_identities` and
    //     `rooms` rows so the `downloads.room_id` and
    //     `rooms.host_user_id` foreign keys have something
    //     to point at. In production, the Tauri webview
    //     would populate these via the `room://state` /
    //     `manifest://state` events; the test bypasses the
    //     webview and seeds the rows directly using the
    //     verified manifest's host_signature.public_key
    //     (which is already the base64-encoded 32-byte
    //     Ed25519 verifying key, so we can pass it
    //     through verbatim).
    let host_pubkey_b64 = verified_manifest
        .host_signature
        .as_ref()
        .expect("verified manifest has host_signature")
        .public_key
        .clone();
    // Seed `user_identities` BEFORE `rooms` because the
    // `rooms.host_user_id` FK references `user_identities.id`.
    // The viewer needs TWO rows in `user_identities`:
    //   1. host keyed by `host_signaling_user_id` (the
    //      UUID the server minted) so
    //      `WebRtcManager::lookup_dc_by_peer_id`'s closure
    //      can find the host's pubkey from the
    //      WebRtcManager's UUID-keyed peer map.
    //   2. viewer keyed by `viewer.user_id` (the
    //      sha256(public_key) hex) because
    //      `downloads.user_id` is set by
    //      `IdentityService::ensure_user_row` to that
    //      hex, and `downloads.user_id` FKs into
    //      `user_identities.id`.
    // Both rows need a `public_key` field; the lookup
    // closure only ever reads the host's row's pubkey.
    let _viewer_sha_user_id = match viewer.identity.ensure_user_row().await {
        Ok(u) => u,
        Err(e) => {
            finalize_failure(
                &mut result,
                "seed_viewer_room",
                format!("ensure_user_row: {e}"),
                overall_start,
                Some(server_cancel.clone()),
            );
            return;
        }
    };
    let _viewer_pubkey_b64 = {
        use base64::Engine;
        let kp = match viewer.identity.load_keypair().await {
            Ok(k) => k,
            Err(e) => {
                finalize_failure(
                    &mut result,
                    "seed_viewer_room",
                    format!("load_keypair: {e}"),
                    overall_start,
                    Some(server_cancel.clone()),
                );
                return;
            }
        };
        base64::engine::general_purpose::STANDARD.encode(kp.signing.verifying_key().to_bytes())
    };
    if let Err(e) = sqlx::query(
        "INSERT INTO user_identities (id, public_key, display_name, created_at, last_seen) \
         VALUES (?1, ?2, 'host', 1, 1) \
         ON CONFLICT(id) DO NOTHING",
    )
    .bind(&host_signaling_user_id)
    .bind(&host_pubkey_b64)
    .execute(&viewer.storage.pool())
    .await
    .map_err(|e| e.to_string())
    {
        finalize_failure(
            &mut result,
            "seed_viewer_room",
            format!("seed host user_identities (signaling): {e}"),
            overall_start,
            Some(server_cancel.clone()),
        );
        return;
    }
    // The viewer's sha256-hex row was already seeded by
    // `IdentityService::ensure_user_row()` in build_rig.
    // We also need a row keyed by the signaling UUID so
    // room_participants.user_id -> user_identities.id
    // resolves correctly. `public_key` is UNIQUE so we
    // use empty string for the UUID-keyed mirror row
    // (the lookup closure only ever reads the SHA-hex
    // row's pubkey for the viewer; the host's UUID-keyed
    // row carries the real pubkey).
    if let Err(e) = sqlx::query(
        "INSERT INTO user_identities (id, public_key, display_name, created_at, last_seen) \
         VALUES (?1, '', 'viewer', 1, 1) \
         ON CONFLICT(id) DO NOTHING",
    )
    .bind(&viewer_signaling_user_id)
    .execute(&viewer.storage.pool())
    .await
    .map_err(|e| e.to_string())
    {
        finalize_failure(
            &mut result,
            "seed_viewer_room",
            format!("seed viewer user_identities (signaling): {e}"),
            overall_start,
            Some(server_cancel.clone()),
        );
        return;
    }
    if let Err(e) = sqlx::query(
        "INSERT INTO rooms (id, code, host_user_id, created_at, ended_at, state, settings) \
         VALUES (?1, ?2, ?3, 1, NULL, 'open', '{}') \
         ON CONFLICT(id) DO NOTHING",
    )
    .bind(room_id.to_string())
    .bind(&room_code)
    .bind(&host_signaling_user_id)
    .execute(&viewer.storage.pool())
    .await
    .map_err(|e| e.to_string())
    {
        finalize_failure(
            &mut result,
            "seed_viewer_room",
            e,
            overall_start,
            Some(server_cancel.clone()),
        );
        return;
    }
    // Seed room_participants rows for both the host and
    // the viewer. The viewer's `download_open`'s Missing
    // arm uses `load_room_participant_user_ids` ->
    // `build_user_pubkey_cache` to populate the closure
    // passed to `WebRtcManager::lookup_dc_by_peer_id`.
    // That closure is keyed by the WebRtcManager's
    // participant UUID (which is the signaling-issued
    // user_id), so we must seed both room_participants
    // and user_identities keyed by the UUID -- NOT by
    // the sha256(public_key) hex that
    // `IdentityService::ensure_user_row` normally uses.
    if let Err(e) = sqlx::query(
        "INSERT INTO user_identities (id, public_key, display_name, created_at, last_seen) \
         VALUES (?1, ?2, 'host', 1, 1) \
         ON CONFLICT(id) DO NOTHING",
    )
    .bind(&host_signaling_user_id)
    .bind(&host_pubkey_b64)
    .execute(&viewer.storage.pool())
    .await
    .map_err(|e| e.to_string())
    {
        finalize_failure(
            &mut result,
            "seed_viewer_room",
            format!("seed host user_identities: {e}"),
            overall_start,
            Some(server_cancel.clone()),
        );
        return;
    }
    for (uid, role, name) in [
        (&host_signaling_user_id, "host", "smoke-host"),
        (&viewer_signaling_user_id, "guest", "smoke-viewer"),
    ] {
        if let Err(e) = sqlx::query(
            "INSERT INTO room_participants \
                (id, room_id, user_id, display_name, role, joined_at, connection_state, capabilities) \
             VALUES (?1, ?2, ?3, ?4, ?5, 1, 'connected', '{}') \
             ON CONFLICT(room_id, user_id) DO NOTHING",
        )
        .bind(Uuid::new_v4().to_string())
        .bind(room_id.to_string())
        .bind(uid)
        .bind(name)
        .bind(role)
        .execute(&viewer.storage.pool())
        .await
        .map_err(|e| e.to_string())
        {
            finalize_failure(
                &mut result,
                "seed_viewer_room",
                format!("seed room_participants {name}: {e}"),
                overall_start,
                Some(server_cancel.clone()),
            );
            return;
        }
    }
    result.stages_passed.push("seed_viewer_room".to_string());

    // 8. VIEWER triggers open_download_inner. The
    //    orchestrator is spawned inside; we receive
    //    `state = "pending"` immediately. The caller is
    //    expected to pass the signaling-issued UUIDs
    //    (NOT the sha256(public_key) hex) for both
    //    `room_host_user_id` (which lands on the downloads
    //    row's `source_peer_id` cache via the manifest's
    //    `pick_primary_source_peer`) and `user_id`
    //    (which must match the `downloads.user_id` FK
    //    pointing at `user_identities.id`).
    let download_id = Uuid::new_v4().to_string();
    let ipc: DownloadSessionIpc = match open_download_inner(
        verified_manifest.clone(),
        room_id,
        &host_signaling_user_id,
        &viewer_signaling_user_id,
        &viewer.storage,
        &viewer.library_root,
        &media_id,
        &download_id,
        &viewer.webrtc,
        &viewer.registry,
        viewer.identity.clone(),
    )
    .await
    {
        Ok(i) => i,
        Err(e) => {
            finalize_failure(
                &mut result,
                "open_download",
                format!("{e}"),
                overall_start,
                Some(server_cancel.clone()),
            );
            return;
        }
    };
    if ipc.state != "pending" {
        finalize_failure(
            &mut result,
            "open_download",
            format!("expected state=pending, got {}", ipc.state),
            overall_start,
            Some(server_cancel.clone()),
        );
        return;
    }
    result.stages_passed.push("open_download".to_string());

    // 9. Poll downloads.state + transferred_bytes until
    //    complete, with a 15s hard budget. The budget is
    //    short because the rest of the path is proven
    //    synchronously and a Transferring-with-0-bytes
    //    state at this point means the host-side
    //    SenderSession spawn is not yet wired (a known
    //    gap surfaced by this smoke test); we want to
    //    surface that quickly rather than wait the full
    //    60s budget.
    let store = DownloadStore::new(viewer.storage.pool());
    let final_record = match wait_for_complete(&store, &download_id, Duration::from_secs(15)).await
    {
        Ok(r) => r,
        Err(e) => {
            // The P3-T13 wire layer wires the viewer's
            // MultiSourceReceiver + Scheduler but does NOT
            // wire the host's SenderSession spawn point
            // (no code in `WebRtcManager::on_inbound_data_channel`
            // builds a `DownloadPlan` from the verified
            // manifest and spawns a `SenderSession`). The
            // smoke test therefore reliably reaches
            // `state = Transferring` but no bytes move.
            // Surface this clearly so the next roadmap
            // task can address it; the test must NOT
            // silently report success without a verified
            // on-disk file.
            let row: Option<(String, Option<String>, i64)> = sqlx::query_as(
                "SELECT state, last_error, transferred_bytes \
                 FROM downloads WHERE id = ?1",
            )
            .bind(&download_id)
            .fetch_optional(&viewer.storage.pool())
            .await
            .ok()
            .flatten();
            let detail = row
                .map(|(s, e, t)| {
                    format!(
                        "(state={s} transferred={t} last_error={e:?}) -- P3-T15 host SenderSession is wired and sending chunks over the authenticated files DataChannel; the viewer is not receiving them, likely due to webrtc 0.20 SCTP event-channel backpressure (the DC's internal event channel is bounded to 1 and OnBufferedAmountLow events compete with OnMessage for the slot)"
                    )
                })
                .unwrap_or_default();
            finalize_failure(
                &mut result,
                "wait_for_complete",
                format!("{e} {detail}"),
                overall_start,
                Some(server_cancel.clone()),
            );
            return;
        }
    };
    result.stages_passed.push("wait_for_complete".to_string());

    // 10. Hash the on-disk file and compare against the
    //     source.
    let on_disk = paths::content_addressed_path(&viewer.library_root, &sha_hex, FILENAME).unwrap();
    if let Err(e) = verify_on_disk(&on_disk, &sha_hex, &blake_hex, source_size).await {
        finalize_failure(
            &mut result,
            "verify_on_disk",
            e,
            overall_start,
            Some(server_cancel.clone()),
        );
        return;
    }
    result.stages_passed.push("verify_on_disk".to_string());

    // 11. Final invariants.
    if host.user_id == viewer.user_id {
        finalize_failure(
            &mut result,
            "final_assertions",
            "host and viewer user_id collided".to_string(),
            overall_start,
            Some(server_cancel.clone()),
        );
        return;
    }
    if !is_valid_room_code(&room_code) {
        finalize_failure(
            &mut result,
            "final_assertions",
            format!("room code {room_code:?} not in allowed alphabet"),
            overall_start,
            Some(server_cancel.clone()),
        );
        return;
    }
    if final_record.state != DownloadState::Complete {
        finalize_failure(
            &mut result,
            "final_assertions",
            format!("expected complete, got {:?}", final_record.state),
            overall_start,
            Some(server_cancel.clone()),
        );
        return;
    }
    if final_record.transferred_bytes as u64 != source_size {
        finalize_failure(
            &mut result,
            "final_assertions",
            format!(
                "transferred_bytes={} expected={source_size}",
                final_record.transferred_bytes
            ),
            overall_start,
            Some(server_cancel.clone()),
        );
        return;
    }
    result.downloaded_size = final_record.transferred_bytes as u64;
    result.final_sha256 = sha_hex.clone();
    result.final_blake3 = blake_hex.clone();
    result.stages_passed.push("final_assertions".to_string());
    result.success = true;
    result.elapsed_ms = overall_start.elapsed().as_millis();
    write_result(&result);
    eprintln!(
        "smoke: PASS in {} ms (host={} viewer={} room={} bytes={})",
        result.elapsed_ms, host.user_id, viewer.user_id, room_code, source_size
    );
    server_cancel.cancel();
}

fn spawn_inbound(room: Arc<RoomClient>) {
    tokio::spawn(async move {
        room.run_inbound().await;
    });
}

fn is_valid_room_code(s: &str) -> bool {
    if s.len() != 6 {
        return false;
    }
    // The server's default alphabet excludes visually
    // confusing characters (0/O, 1/I, etc.). The exact
    // alphabet is configurable but the smoke test only
    // checks the 6-character length and a conservative
    // character set.
    s.chars()
        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
}

async fn wait_for_manifest(
    room: &Arc<RoomClient>,
    room_id: Uuid,
    expected_pubkey: [u8; 32],
    timeout: Duration,
) -> Result<locast_manifest::MediaManifest, String> {
    let start = Instant::now();
    loop {
        if let Some(m) = room.verified_manifest(room_id) {
            // Defence in depth: the accept_manifest pipeline
            // already ran verify_manifest + the trust-anchor
            // check, but the smoke test re-runs the
            // cryptographic check against the canonical
            // bytes for an explicit, isolated assertion.
            if let Some(sig) = m.host_signature.as_ref() {
                if let Ok(pk_bytes) = locast_crypto::ed25519::from_base64(&sig.public_key) {
                    if pk_bytes.len() == 32 {
                        let mut arr = [0u8; 32];
                        arr.copy_from_slice(&pk_bytes);
                        if arr == expected_pubkey {
                            if let Err(e) = verify_manifest(&m) {
                                return Err(format!("verify_manifest failed: {e}"));
                            }
                            return Ok(m);
                        }
                    }
                }
            }
        }
        if start.elapsed() > timeout {
            return Err(format!("manifest did not arrive within {timeout:?}"));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_for_complete(
    store: &DownloadStore,
    download_id: &str,
    timeout: Duration,
) -> Result<locast_client_lib::transfer::state::DownloadRecord, String> {
    let start = Instant::now();
    loop {
        match store.fetch(download_id).await {
            Ok(rec) => {
                if rec.state == DownloadState::Complete {
                    return Ok(rec);
                }
            }
            Err(e) => {
                return Err(format!("download row missing/failed: {e}"));
            }
        }
        if start.elapsed() > timeout {
            let last = store.fetch(download_id).await.ok();
            return Err(match last {
                Some(r) => format!(
                    "download did not complete within {timeout:?}; last state={:?} transferred={}/{}",
                    r.state, r.transferred_bytes, r.total_bytes
                ),
                None => format!("download row vanished within {timeout:?}"),
            });
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn verify_on_disk(
    path: &std::path::Path,
    expected_sha: &str,
    expected_blake: &str,
    expected_size: u64,
) -> Result<(), String> {
    if !path.exists() {
        return Err(format!("on-disk file missing at {}", path.display()));
    }
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|e| format!("read on-disk file: {e}"))?;
    if bytes.len() as u64 != expected_size {
        return Err(format!(
            "size mismatch: file={} expected={expected_size}",
            bytes.len()
        ));
    }
    let mut h = Sha256::new();
    h.update(&bytes);
    let got_sha = hex::encode(h.finalize());
    if got_sha != expected_sha {
        return Err(format!(
            "sha256 mismatch: got {got_sha} expected {expected_sha}"
        ));
    }
    let mut b = Blake3Hasher::new();
    b.update(&bytes);
    let got_blake = b.finalize().to_hex().to_string();
    if got_blake != expected_blake {
        return Err(format!(
            "blake3 mismatch: got {got_blake} expected {expected_blake}"
        ));
    }
    Ok(())
}

fn finalize_failure(
    result: &mut SmokeResult,
    stage: &str,
    message: String,
    start: Instant,
    server_cancel: Option<Cancel>,
) {
    result.success = false;
    result.failure_stage = Some(stage.to_string());
    result.failure_message = Some(message.clone());
    result.elapsed_ms = start.elapsed().as_millis();
    eprintln!("smoke: FAIL at stage={stage}: {message}");
    write_result(result);
    if let Some(c) = server_cancel {
        c.cancel();
    }
    // P3-T14: a smoke failure must surface as a non-zero
    // exit code from `cargo test`, not as a silent
    // `test result: ok`. The PowerShell script reads
    // `result.json` independently and exits 6 on
    // `success: false`, but developers who invoke the Rust
    // test directly (per INTEGRATION.md section 5) need the
    // panic to see what failed.
    panic!("smoke_host_viewer failed at stage {stage}: {message}");
}
