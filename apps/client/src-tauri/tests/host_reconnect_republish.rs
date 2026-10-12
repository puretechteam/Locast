//! When the host's signaling connection comes back, the app re-shares what the
//! host had shared. A host who had NOT shared anything in the room used to be
//! treated as "share everything": the reconnect hook built and published a
//! manifest of every permanent library item, hashing the whole library and
//! showing every filename to the room, because the "nothing shared yet" state
//! and the "all permanent items" state were the same `None`.

use std::sync::Arc;
use std::time::Duration;

use locast_client_lib::identity::keystore::{IdentityKeyring, IdentityService, MockKeyring};
use locast_client_lib::net::config::SignalingConfig;
use locast_client_lib::net::room::RoomClient;
use locast_client_lib::net::signaling::SignalingClient;
use locast_client_lib::room::host::{republish_after_reconnect, HostError};
use locast_client_lib::storage::Storage;
use locast_protocol::handshake::Platform;
use uuid::Uuid;

struct Rig {
    dir: tempfile::TempDir,
    storage: Storage,
    identity: Arc<IdentityService>,
    signaling: Arc<SignalingClient>,
    room: Arc<RoomClient>,
}

async fn rig() -> Rig {
    let dir = tempfile::tempdir().expect("tempdir");
    let storage = Storage::open(&dir.path().join("index.sqlite"))
        .await
        .expect("storage");
    let keyring: Arc<dyn IdentityKeyring> = Arc::new(MockKeyring::new());
    let identity = Arc::new(IdentityService::with_keyring(keyring, storage.clone()));
    identity.get_or_create("host").await.expect("identity");
    // Never started: any attempt to actually send would fail, which is how the
    // tests tell "tried to publish" from "did nothing".
    let signaling = Arc::new(SignalingClient::new(
        SignalingConfig::new_for_test(
            String::from("ws://127.0.0.1:1/ws"),
            Duration::from_secs(1),
            1024 * 1024,
            Platform::Linux,
        ),
        identity.clone(),
    ));
    let room = Arc::new(RoomClient::new(signaling.clone()));
    Rig {
        dir,
        storage,
        identity,
        signaling,
        room,
    }
}

async fn republish(r: &Rig) -> Result<bool, HostError> {
    republish_after_reconnect(
        r.identity.clone(),
        r.signaling.clone(),
        r.room.clone(),
        r.storage.pool(),
        r.dir.path().to_path_buf(),
        Uuid::now_v7(),
    )
    .await
}

/// A permanent library row, so that "publish everything" would find something.
async fn add_permanent_item(r: &Rig) {
    sqlx::query(
        "INSERT INTO media_items (id, filename, sha256, blake3, relative_path, size_bytes, \
         mime, duration_ms, status, created_at, last_seen_at, provenance) \
         VALUES ('m-1', 'film.mp4', ?1, ?2, 'library/aa/bb/film.mp4', 1024, 'video/mp4', 0, \
         'permanent', 1, 1, '{}')",
    )
    .bind("aa".repeat(32))
    .bind("bb".repeat(32))
    .execute(&r.storage.pool())
    .await
    .expect("insert media row");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_host_who_never_shared_publishes_nothing_on_reconnect() {
    let r = rig().await;
    add_permanent_item(&r).await;
    assert_eq!(r.room.host_media_selection(), None);

    let published = republish(&r).await.expect("nothing to do is not an error");

    assert!(
        !published,
        "a reconnect must not publish for a host who never shared"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_host_who_shared_a_selection_re_shares_that_selection() {
    let r = rig().await;
    add_permanent_item(&r).await;
    r.room
        .set_host_media_selection(Some(vec!["not-in-the-library".into()]));

    // It tried to build the manifest for exactly that selection.
    match republish(&r).await {
        Err(HostError::UnknownMedia(id)) => assert_eq!(id, "not-in-the-library"),
        other => panic!("expected the selection to be attempted, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_host_who_chose_to_share_everything_still_re_shares_everything() {
    // `manifest_publish(None)` means "all permanent items" on purpose.
    let r = rig().await;
    r.room.set_host_media_selection(None);
    assert_eq!(r.room.host_media_selection(), Some(None));

    // Attempted: an empty library has nothing to publish, which is the
    // error that proves the share-all path ran.
    assert!(matches!(republish(&r).await, Err(HostError::NoMedia)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_room_starts_with_nothing_shared() {
    let r = rig().await;
    r.room
        .set_host_media_selection(Some(vec!["from-the-last-room".into()]));
    r.room.clear_host_media_selection();
    assert_eq!(r.room.host_media_selection(), None);
    assert!(!republish(&r).await.expect("ok"));
}
