//! How big a film can a room share? The signed manifest lists one SHA-256 per
//! 256 KiB chunk, and the whole manifest travels in ONE signaling frame, which
//! the server and the client cap at 1 MiB by default. At about 66 bytes per
//! chunk that is roughly 15,800 chunks, a file of about 4.1 GB; a typical
//! 1080p film of three hours is larger.
//!
//! This test pins down both halves of the behaviour with a synthetic manifest
//! for a ~10.5 GB film (40,000 chunks, ~2.7 MB on the wire):
//!
//! 1. With the default caps the host is told, up front and in words, that the
//!    selection is too large, and the connection is left alone (it used to be
//!    dropped by the server, then re-hashed and re-published on reconnect).
//! 2. The caps are configurable (`LOCAST_BIND_MAX_FRAME_BYTES` and
//!    `LOCAST_RATE_BYTES_BURST` on the server, the client's frame cap), and with
//!    them raised the same manifest is published, relayed and verified by a
//!    viewer. Whether to raise the defaults, scale the chunk size, or send the
//!    hashes another way is a product decision; this is the verified
//!    workaround meanwhile.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use locast_client_lib::identity::keystore::{IdentityKeyring, IdentityService, MockKeyring};
use locast_client_lib::net::config::SignalingConfig;
use locast_client_lib::net::room::RoomClient;
use locast_client_lib::net::signaling::SignalingClient;
use locast_client_lib::net::state::ConnPhase;
use locast_client_lib::room::host::{sign_and_publish, HostError};
use locast_client_lib::storage::Storage;
use locast_manifest::{MediaEntry, MediaManifest, Source};
use locast_protocol::handshake::Platform;
use tokio::net::TcpListener;
use uuid::Uuid;

const CHUNK: u32 = 256 * 1024;
const CHUNKS: u32 = 40_000;

async fn start_server() -> SocketAddr {
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
        epoch_counter: Arc::new(StdMutex::new(locast_server::auth::EpochCounter::default())),
    };
    let app: axum::Router = locast_server::router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    addr
}

struct Client {
    _dir: tempfile::TempDir,
    identity: Arc<IdentityService>,
    signaling: Arc<SignalingClient>,
    room: Arc<RoomClient>,
}

async fn client(addr: SocketAddr, label: &str, max_frame_bytes: usize) -> Client {
    let dir = tempfile::tempdir().expect("tempdir");
    let storage = Storage::open(&dir.path().join("index.sqlite"))
        .await
        .expect("storage");
    let keyring: Arc<dyn IdentityKeyring> = Arc::new(MockKeyring::new());
    let identity = Arc::new(IdentityService::with_keyring(keyring, storage));
    identity.get_or_create(label).await.expect("identity");
    let signaling = Arc::new(SignalingClient::new(
        SignalingConfig::new_for_test(
            format!("ws://{addr}/ws"),
            Duration::from_secs(5),
            max_frame_bytes,
            Platform::Linux,
        ),
        identity.clone(),
    ));
    let room = Arc::new(RoomClient::new(signaling.clone()));
    room.init().await;
    signaling.start().await.expect("start");
    let rc = room.clone();
    tokio::spawn(async move { rc.run_inbound().await });
    let deadline = Instant::now() + Duration::from_secs(15);
    while signaling.snapshot().await.phase != ConnPhase::Authenticated {
        assert!(Instant::now() < deadline, "{label}: not authenticated");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    Client {
        _dir: dir,
        identity,
        signaling,
        room,
    }
}

/// An unsigned manifest for a ~10.5 GB film. Nothing here touches a disk: the
/// chunk hashes are synthetic, which is all the wire and the server look at.
fn film_manifest(room_id: Uuid, host_peer_id: String) -> MediaManifest {
    MediaManifest {
        manifest_version: 1,
        room_id: room_id.to_string(),
        media: vec![MediaEntry {
            id: Uuid::new_v4().to_string(),
            filename: "long film.mkv".into(),
            sha256: "ab".repeat(32),
            blake3: "cd".repeat(32),
            size_bytes: u64::from(CHUNK) * u64::from(CHUNKS),
            mime: "video/x-matroska".into(),
            duration_ms: 10_800_000,
            dimensions: None,
            codecs: None,
            sources: vec![Source {
                peer_id: host_peer_id,
                url_hint: None,
                priority: 0,
                chunk_size: CHUNK,
                total_chunks: CHUNKS,
                chunk_hashes: (0..CHUNKS).map(|i| format!("{i:064x}")).collect(),
            }],
        }],
        subtitles: vec![],
        created_at: 1_700_000_000_000,
        host_signature: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_manifest_for_a_ten_gigabyte_film_needs_the_raised_frame_caps() {
    // ---- 1. Default caps: refused up front, connection untouched. --------
    let server = start_server().await;
    let host = client(server, "host", 1024 * 1024).await;
    let created = host
        .room
        .room_create("big film".into(), true)
        .await
        .expect("create");
    let room_id = Uuid::parse_str(&created.id).unwrap();
    let host_peer = locast_client_lib::room::peer_id::derive_peer_id(
        host.identity
            .load_keypair()
            .await
            .unwrap()
            .signing
            .verifying_key()
            .to_bytes(),
    );
    let manifest = film_manifest(room_id, host_peer.clone());

    let err = sign_and_publish(&host.identity, &host.signaling, &manifest)
        .await
        .expect_err("a 2.7 MB manifest does not fit a 1 MiB frame");
    assert!(
        matches!(err, HostError::ManifestTooLarge { .. }),
        "the host is told the selection is too large, got {err:?}"
    );
    assert!(
        host.signaling.snapshot().await.connected,
        "refusing the message must not drop the connection"
    );
    assert!(host.room.state().await.is_some(), "still in the room");

    // ---- 2. Raised caps: published, relayed, verified by a viewer. -------
    // (Process-wide env, read once when the server's config is built; this
    // test is the only one in this binary.)
    std::env::set_var("LOCAST_BIND_MAX_FRAME_BYTES", (8 * 1024 * 1024).to_string());
    std::env::set_var("LOCAST_RATE_BYTES_BURST", (8 * 1024 * 1024).to_string());
    let server = start_server().await;
    std::env::remove_var("LOCAST_BIND_MAX_FRAME_BYTES");
    std::env::remove_var("LOCAST_RATE_BYTES_BURST");
    let host = client(server, "host2", 8 * 1024 * 1024).await;
    let viewer = client(server, "viewer2", 8 * 1024 * 1024).await;
    let created = host
        .room
        .room_create("big film".into(), true)
        .await
        .expect("create");
    let room_id = Uuid::parse_str(&created.id).unwrap();
    let host_pubkey = host
        .identity
        .load_keypair()
        .await
        .unwrap()
        .signing
        .verifying_key()
        .to_bytes();
    viewer.room.set_expected_host_pubkey(host_pubkey);
    viewer
        .room
        .room_join(created.code.clone(), "viewer".into())
        .await
        .expect("join");

    let manifest = film_manifest(
        room_id,
        locast_client_lib::room::peer_id::derive_peer_id(host_pubkey),
    );
    let signed = sign_and_publish(&host.identity, &host.signaling, &manifest)
        .await
        .map_err(|e: HostError| format!("{e}"))
        .expect("with the caps raised the manifest is sent");
    assert_eq!(
        signed.media[0].sources[0].chunk_hashes.len(),
        CHUNKS as usize
    );

    let deadline = Instant::now() + Duration::from_secs(20);
    let received = loop {
        if let Some(m) = viewer.room.verified_manifest(room_id) {
            break m;
        }
        assert!(
            Instant::now() < deadline,
            "the viewer never got a verified manifest"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(
        received.media[0].sources[0].chunk_hashes.len(),
        CHUNKS as usize
    );
    assert_eq!(received.media[0].duration_ms, 10_800_000);
    assert!(
        matches!(
            viewer.signaling.snapshot().await.phase,
            ConnPhase::Authenticated
        ),
        "the viewer's connection survived a 2.7 MB frame"
    );
}
