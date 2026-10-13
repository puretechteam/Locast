//! How big a film can a room share? The signed manifest lists one SHA-256 per
//! 256 KiB chunk (about 67 bytes each), so it grows with the file: ~2.7 MB for
//! a 10.5 GB film, ~13.7 MB for the default 50 GiB media quota. A signaling
//! frame is capped at 1 MiB, so such a manifest travels as `MANIFEST_PART`
//! pieces (see `locast_protocol::manifest_parts`): the host splits the one
//! signed manifest, paces the pieces inside the server's rate limit, the
//! server reassembles it, verifies it and persists it, and relays it to the
//! room in pieces again, where each viewer reassembles it and runs the same
//! verification as for a single-frame manifest.
//!
//! These tests use the DEFAULT frame cap and the DEFAULT rate limits on both
//! ends (before the pieces existed, a manifest above ~4.1 GB was refused up
//! front with `ManifestTooLarge`, and the only way through was raising both
//! caps), and the production signing, publishing, relay, assembly and
//! verification code.

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
/// 50 GiB at 256 KiB per chunk.
const QUOTA_CHUNKS: u32 = 204_800;
/// The default signaling frame cap.
const DEFAULT_FRAME: usize = 1024 * 1024;

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

/// An unsigned manifest for a film of `chunks` chunks. Nothing here touches a disk: the
/// chunk hashes are synthetic, which is all the wire and the server look at.
fn film_manifest(room_id: Uuid, host_peer_id: String, chunks: u32) -> MediaManifest {
    MediaManifest {
        manifest_version: 1,
        room_id: room_id.to_string(),
        media: vec![MediaEntry {
            id: Uuid::new_v4().to_string(),
            filename: "long film.mkv".into(),
            sha256: "ab".repeat(32),
            blake3: "cd".repeat(32),
            size_bytes: u64::from(CHUNK) * u64::from(chunks),
            mime: "video/x-matroska".into(),
            duration_ms: 10_800_000,
            dimensions: None,
            codecs: None,
            sources: vec![Source {
                peer_id: host_peer_id,
                url_hint: None,
                priority: 0,
                chunk_size: CHUNK,
                total_chunks: chunks,
                chunk_hashes: (0..chunks).map(|i| format!("{i:064x}")).collect(),
            }],
        }],
        subtitles: vec![],
        created_at: 1_700_000_000_000,
        host_signature: None,
    }
}

struct Room {
    host: Client,
    viewer: Client,
    room_id: Uuid,
    code: String,
    host_pubkey: [u8; 32],
}

/// A host in a room with a viewer who joined BEFORE the publish.
async fn room_with_viewer(addr: SocketAddr) -> Room {
    let host = client(addr, "host", DEFAULT_FRAME).await;
    let viewer = client(addr, "viewer", DEFAULT_FRAME).await;
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
    host.room.set_expected_host_pubkey(host_pubkey);
    viewer.room.set_expected_host_pubkey(host_pubkey);
    viewer
        .room
        .room_join(created.code.clone(), "viewer".into())
        .await
        .expect("join");
    Room {
        host,
        viewer,
        room_id,
        code: created.code,
        host_pubkey,
    }
}

async fn verified(c: &Client, room_id: Uuid, what: &str) -> MediaManifest {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        if let Some(m) = c.room.verified_manifest(room_id) {
            return m;
        }
        assert!(Instant::now() < deadline, "{what}: no verified manifest");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Publish a `chunks`-chunk film from `room.host` and check every field and
/// hash arrives at the viewer, who joined before the publish, intact and
/// verified.
async fn publish_and_check(room: &Room, chunks: u32) -> (MediaManifest, Duration) {
    let manifest = film_manifest(
        room.room_id,
        locast_client_lib::room::peer_id::derive_peer_id(room.host_pubkey),
        chunks,
    );
    let started = Instant::now();
    let signed = sign_and_publish(&room.host.identity, &room.host.signaling, &manifest)
        .await
        .map_err(|e: HostError| format!("{e}"))
        .expect("with default settings the manifest is sent");
    let received = verified(&room.viewer, room.room_id, "viewer").await;
    let took = started.elapsed();

    // Every chunk hash and metadata field, and the signature, survived.
    assert_eq!(received, signed, "the viewer holds exactly what was signed");
    locast_manifest::verify_manifest(&received).expect("signature verifies");
    let m = &received.media[0];
    assert_eq!(m.sources[0].chunk_hashes.len(), chunks as usize);
    assert_eq!(m.sources[0].total_chunks, chunks);
    assert_eq!(m.sources[0].chunk_hashes[0], format!("{:064x}", 0));
    assert_eq!(
        m.sources[0].chunk_hashes[chunks as usize - 1],
        format!("{:064x}", chunks - 1)
    );
    assert_eq!(m.size_bytes, u64::from(CHUNK) * u64::from(chunks));
    assert_eq!(m.duration_ms, 10_800_000);
    assert_eq!(m.filename, "long film.mkv");
    assert_eq!(received.room_id, room.room_id.to_string());
    assert!(
        matches!(
            room.viewer.signaling.snapshot().await.phase,
            ConnPhase::Authenticated
        ),
        "the viewer's connection survived the pieces"
    );
    assert!(
        room.host.signaling.snapshot().await.connected,
        "and so did the host's"
    );
    (received, took)
}

/// ~10.5 GB (40,000 chunks, ~2.7 MB of hashes): above the old 4.1 GB ceiling.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_ten_gigabyte_film_is_shared_with_default_settings() {
    let server = start_server().await;
    let room = room_with_viewer(server).await;
    let (_m, took) = publish_and_check(&room, 40_000).await;
    eprintln!("10.5 GB manifest: host publish -> viewer verified in {took:?}");
}

/// The default media quota is 50 GiB: 204,800 chunks, a ~13.7 MB manifest,
/// the largest the app is built to share. Published to a viewer already in the
/// room, then fetched by a late joiner (the MANIFEST_RESPONSE path).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_manifest_for_the_default_quota_is_shared_and_fetched_by_a_late_joiner() {
    let server = start_server().await;
    let room = room_with_viewer(server).await;
    let (published, took) = publish_and_check(&room, QUOTA_CHUNKS).await;
    eprintln!("50 GiB manifest: host publish -> viewer verified in {took:?}");

    // A late joiner fetches it from the server.
    let late = client(server, "late", DEFAULT_FRAME).await;
    late.room.set_expected_host_pubkey(room.host_pubkey);
    late.room
        .room_join(room.code.clone(), "late".into())
        .await
        .expect("join");
    let started = Instant::now();
    let response = late
        .room
        .manifest_fetch(room.room_id, Uuid::new_v4())
        .await
        .expect("a late joiner can fetch a manifest of any supported size");
    eprintln!("50 GiB manifest: late fetch in {:?}", started.elapsed());
    assert_eq!(response.manifest, published);
    assert_eq!(response.version, 1);
    assert_eq!(late.room.verified_manifest(room.room_id), Some(published));
}

/// Beyond what the app is built to share, the host is told so up front and
/// nothing is sent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_manifest_beyond_the_supported_size_is_refused_up_front() {
    let server = start_server().await;
    let room = room_with_viewer(server).await;
    // 600,000 chunks: ~40 MB of hashes, past the 32 MiB assembly limit.
    let manifest = film_manifest(
        room.room_id,
        locast_client_lib::room::peer_id::derive_peer_id(room.host_pubkey),
        600_000,
    );
    let err = sign_and_publish(&room.host.identity, &room.host.signaling, &manifest)
        .await
        .expect_err("too large");
    assert!(matches!(err, HostError::ManifestTooLarge { .. }), "{err:?}");
    assert!(room.host.signaling.snapshot().await.connected);
    assert!(room.viewer.room.verified_manifest(room.room_id).is_none());
}
