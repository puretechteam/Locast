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
//!    `permanent` `media_items` row, creates a room, and
//!    publishes a signed manifest for exactly that item via
//!    `room::host::build_sign_and_publish_selected` (the
//!    `manifest_publish` command path).
//! 4. VIEWER joins through the host's invite link: the
//!    `room_join` command's `invite_anchor_for` turns the
//!    invite `h=` key into the manifest trust anchor. The
//!    viewer's `RoomClient` mirrors the room snapshot into its
//!    local `rooms` / `room_participants` / `user_identities`
//!    tables (no hand-seeded rows).
//! 5. VIEWER verifies the manifest, then calls
//!    `commands::download::open_download_inner` the way the UI
//!    does (retrying while no source DataChannel is open yet)
//!    and polls `downloads.state` until completion.
//! 6. Asserts the on-disk file matches by SHA-256 / BLAKE3 /
//!    size, is a library item, that re-opening the same media
//!    is a dedup hit that starts no transfer, and that the
//!    `locast://` handler serves it with a bounded Range.
//! 7. Writes a safe-only `result.json` summarising the run.

#![allow(clippy::needless_return)]
#![allow(clippy::field_reassign_with_default)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use blake3::Hasher as Blake3Hasher;
use locast_client_lib::commands::download::{open_download_inner, DownloadSessionIpc};
use locast_client_lib::commands::room::invite_anchor_for;
use locast_client_lib::core::paths;
use locast_client_lib::identity::keystore::{IdentityKeyring, IdentityService, MockKeyring};
use locast_client_lib::library::protocol::{resolve_media_url, ProtocolHandler};
use locast_client_lib::net::config::SignalingConfig;
use locast_client_lib::net::room::RoomClient;
use locast_client_lib::net::signaling::SignalingClient;
use locast_client_lib::net::state::ConnPhase;
use locast_client_lib::net::webrtc::WebRtcManager;
use locast_client_lib::room::host::{build_invite_url, build_sign_and_publish_selected};
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
        epoch_counter: std::sync::Arc::new(std::sync::Mutex::new(
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
    match host.room.room_create("smoke-room".into(), false).await {
        Ok(summary) => {
            room_id = Uuid::parse_str(&summary.id).expect("room_id uuid");
            room_code = summary.code.clone();
            result.room_id = summary.id.clone();
            result.room_code = summary.code.clone();
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

    // 5. VIEWER joins through the host's invite link, exactly
    //    as the `room_join` command does: the strict invite
    //    parser yields the host key, which becomes the
    //    manifest trust anchor BEFORE the join is sent.
    let invite = build_invite_url("locast", &room_code, host.pubkey).expect("invite url");
    match invite_anchor_for(&room_code, Some(&invite)) {
        Ok(Some(pk)) => viewer.room.set_expected_host_pubkey(pk),
        other => {
            finalize_failure(
                &mut result,
                "room_join",
                format!("invite anchor: {other:?}"),
                overall_start,
                Some(server_cancel.clone()),
            );
            return;
        }
    }
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
    result.stages_passed.push("room_join".to_string());

    // 6. HOST publishes a signed manifest for exactly the
    //    seeded item (the `manifest_publish` command path
    //    with a host selection).
    if let Err(e) = build_sign_and_publish_selected(
        host.identity.clone(),
        host.signaling.clone(),
        host.room.clone(),
        host.storage.pool(),
        host.library_root.clone(),
        room_id,
        Some(vec![media_id.clone()]),
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
    if verified_manifest.media.len() != 1 || verified_manifest.media[0].id != media_id {
        finalize_failure(
            &mut result,
            "wait_for_manifest",
            "manifest does not carry exactly the selected item".to_string(),
            overall_start,
            Some(server_cancel.clone()),
        );
        return;
    }
    result.stages_passed.push("wait_for_manifest".to_string());

    // 7b. The viewer's RoomClient mirrored the ROOM_JOINED
    //     snapshot: the room row, and the host as a participant
    //     whose identity row carries the host pubkey.
    let host_server_id = viewer
        .room
        .state()
        .await
        .map(|s| s.host_user_id)
        .unwrap_or_default();
    let mirrored: i64 = {
        use base64::Engine as _;
        sqlx::query_as::<_, (i64,)>(
            "SELECT COUNT(*) FROM room_participants rp \
             JOIN rooms r ON r.id = rp.room_id \
             JOIN user_identities ui ON ui.id = rp.user_id \
             WHERE rp.room_id = ?1 AND rp.user_id = ?2 AND ui.public_key = ?3",
        )
        .bind(room_id.to_string())
        .bind(&host_server_id)
        .bind(base64::engine::general_purpose::STANDARD.encode(host.pubkey))
        .fetch_one(&viewer.storage.pool())
        .await
        .map(|r| r.0)
        .unwrap_or(0)
    };
    if mirrored != 1 {
        finalize_failure(
            &mut result,
            "mirror_room",
            format!("expected the host mirrored on the viewer, found {mirrored} rows"),
            overall_start,
            Some(server_cancel.clone()),
        );
        return;
    }
    result.stages_passed.push("mirror_room".to_string());

    // 8. VIEWER opens the download the way the UI does: call
    //    `download_open` and retry while no source DataChannel
    //    is open yet (`pending` without `transfer_started`).
    //    Arguments mirror the `download_open` command: the
    //    room's host id and the local identity row id.
    let (manifest_ref, host_ref, viewer_ref, media_ref) =
        (&verified_manifest, &host_server_id, &viewer, &media_id);
    let open = move |download_id: String| async move {
        open_download_inner(
            manifest_ref.clone(),
            room_id,
            host_ref,
            &viewer_ref.user_id,
            &viewer_ref.storage,
            &viewer_ref.library_root,
            media_ref,
            &download_id,
            &viewer_ref.webrtc,
            &viewer_ref.registry,
            viewer_ref.identity.clone(),
        )
        .await
    };
    let deadline = Instant::now() + Duration::from_secs(20);
    let ipc: DownloadSessionIpc = loop {
        match open(Uuid::new_v4().to_string()).await {
            Ok(i) if i.transfer_started || i.dedup_hit || i.state != "pending" => break i,
            Ok(_) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            Ok(i) => {
                finalize_failure(
                    &mut result,
                    "open_download",
                    format!("no source transport within 20 s: {i:?}"),
                    overall_start,
                    Some(server_cancel.clone()),
                );
                return;
            }
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
        }
    };
    if ipc.dedup_hit || !ipc.transfer_started {
        finalize_failure(
            &mut result,
            "open_download",
            format!("expected a real transfer, got {ipc:?}"),
            overall_start,
            Some(server_cancel.clone()),
        );
        return;
    }
    let download_id = ipc.download_id.clone();
    result.stages_passed.push("open_download".to_string());

    // 9. Poll downloads.state + transferred_bytes until
    //    complete, with a 15s hard budget. On a typical
    //    workstation the full 1 MiB transfer finishes in
    //    under 8 seconds; the budget exists to surface
    //    stalls quickly.
    let store = DownloadStore::new(viewer.storage.pool());
    let final_record = match wait_for_complete(&store, &download_id, Duration::from_secs(15)).await
    {
        Ok(r) => r,
        Err(e) => {
            // Transfer did not complete in the budget.
            // Surface the last DB state so the failure
            // mode is unambiguous. The test must NOT
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
                        "(state={s} transferred={t} last_error={e:?}) -- transfer stalled before completion; check the orchestrator logs for the underlying cause."
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

    // 10b. The installed file is a library item pointing at
    //      the verified on-disk file.
    let item: Option<(String, String)> =
        sqlx::query_as("SELECT status, relative_path FROM media_items WHERE id = ?1")
            .bind(&ipc.media_id)
            .fetch_optional(&viewer.storage.pool())
            .await
            .ok()
            .flatten();
    match &item {
        Some((_, rel)) if viewer.library_root.join(rel) == on_disk => {}
        other => {
            finalize_failure(
                &mut result,
                "library_item",
                format!("library row does not point at the installed file: {other:?}"),
                overall_start,
                Some(server_cancel.clone()),
            );
            return;
        }
    }
    result.stages_passed.push("library_item".to_string());

    // 10c. Dedup: re-opening the same media resolves locally
    //      and starts no transfer.
    match open(Uuid::new_v4().to_string()).await {
        Ok(i) if i.dedup_hit && !i.transfer_started && i.state == "complete" => {}
        other => {
            finalize_failure(
                &mut result,
                "dedup_reopen",
                format!("expected a dedup hit without a transfer, got {other:?}"),
                overall_start,
                Some(server_cancel.clone()),
            );
            return;
        }
    }
    result.stages_passed.push("dedup_reopen".to_string());

    // 10d. Playback: the P1-T10 `locast://` handler serves the
    //      downloaded item with a bounded 206 Range response.
    let handler = ProtocolHandler::new(viewer.storage.clone(), viewer.library_root.clone());
    let served = match resolve_media_url(&viewer.storage, &ipc.media_id).await {
        Ok(url) => handler.handle(&url, "GET", Some("bytes=0-")).await.ok(),
        Err(_) => None,
    };
    let content_range = served.as_ref().and_then(|r| {
        r.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("Content-Range"))
            .map(|(_, v)| v.clone())
    });
    let range_ok = served.as_ref().map(|r| r.status) == Some(206)
        && content_range
            .as_deref()
            .is_some_and(|v| v.ends_with(&format!("/{source_size}")));
    if !range_ok {
        finalize_failure(
            &mut result,
            "playback",
            format!(
                "locast:// did not serve a 206 for the download: status={:?} range={content_range:?}",
                served.as_ref().map(|r| r.status)
            ),
            overall_start,
            Some(server_cancel.clone()),
        );
        return;
    }
    result.stages_passed.push("playback".to_string());

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
