//! A viewer's network drops in the middle of a session. When it comes back it
//! must be in the room, in step with the host, and able to keep receiving.
//!
//! Real pieces only: the real `locast-server` router (in-process), two real
//! `SignalingClient` + `RoomClient` pairs, and a TCP proxy between the viewer
//! and the server that this test cuts and restores to cause a genuine
//! disconnect and reconnect. The post-AUTH hook that production installs in
//! `lib.rs` (rejoin the active room) is installed the same way here.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use locast_client_lib::identity::keystore::{IdentityKeyring, IdentityService, MockKeyring};
use locast_client_lib::net::config::SignalingConfig;
use locast_client_lib::net::room::{PlaybackStateEvent, RoomClient, RoomEventSink, RoomSummaryIpc};
use locast_client_lib::net::signaling::SignalingClient;
use locast_client_lib::net::state::ConnPhase;
use locast_client_lib::storage::Storage;
use locast_protocol::envelope::{Envelope, MessageKind};
use locast_protocol::handshake::Platform;
use locast_protocol::room::{PlaybackAction, PlaybackCommandPayload};
use tokio::net::{TcpListener, TcpStream};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// server + a severable proxy in front of it
// ---------------------------------------------------------------------------

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

/// A TCP forwarder. `sever()` kills every open connection and refuses new ones
/// (the network is down); `restore()` lets connections through again.
struct Proxy {
    addr: SocketAddr,
    generation: tokio::sync::watch::Sender<u64>,
    down: Arc<AtomicBool>,
}

impl Proxy {
    async fn start(target: SocketAddr) -> Proxy {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind proxy");
        let addr = listener.local_addr().expect("addr");
        let (generation, _) = tokio::sync::watch::channel(0u64);
        let down = Arc::new(AtomicBool::new(false));
        let gen_tx = generation.clone();
        let down_flag = down.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut inbound, _)) = listener.accept().await else {
                    return;
                };
                if down_flag.load(Ordering::SeqCst) {
                    drop(inbound);
                    continue;
                }
                let mut gen_rx = gen_tx.subscribe();
                tokio::spawn(async move {
                    let Ok(mut outbound) = TcpStream::connect(target).await else {
                        return;
                    };
                    tokio::select! {
                        _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound) => {}
                        _ = gen_rx.changed() => {}
                    }
                });
            }
        });
        Proxy {
            addr,
            generation,
            down,
        }
    }

    fn sever(&self) {
        self.down.store(true, Ordering::SeqCst);
        self.generation.send_modify(|g| *g += 1);
    }

    fn restore(&self) {
        self.down.store(false, Ordering::SeqCst);
    }
}

// ---------------------------------------------------------------------------
// a client
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Recorder {
    states: StdMutex<Vec<RoomSummaryIpc>>,
    playback: StdMutex<Vec<PlaybackStateEvent>>,
    cleared: AtomicU64,
}

impl RoomEventSink for Recorder {
    fn emit_state(&self, s: &RoomSummaryIpc) {
        self.states.lock().unwrap().push(s.clone());
    }
    fn emit_event(&self, _s: &RoomSummaryIpc) {}
    fn emit_state_cleared(&self) {
        self.cleared.fetch_add(1, Ordering::SeqCst);
    }
    fn emit_playback_state(&self, ev: &PlaybackStateEvent) {
        self.playback.lock().unwrap().push(ev.clone());
    }
}

struct Client {
    _dir: tempfile::TempDir,
    signaling: Arc<SignalingClient>,
    room: Arc<RoomClient>,
    recorder: Arc<Recorder>,
}

async fn client(url: String, label: &str) -> Client {
    // `RUST_LOG=locast_client_lib=debug` shows what the clients do.
    if std::env::var("RUST_LOG").is_ok() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_test_writer()
            .try_init();
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let storage = Storage::open(&dir.path().join("index.sqlite"))
        .await
        .expect("storage");
    let keyring: Arc<dyn IdentityKeyring> = Arc::new(MockKeyring::new());
    let identity = Arc::new(IdentityService::with_keyring(keyring, storage));
    identity.get_or_create(label).await.expect("identity");
    let signaling = Arc::new(SignalingClient::new(
        SignalingConfig::new_for_test(url, Duration::from_secs(5), 1024 * 1024, Platform::Linux),
        identity,
    ));
    let room = Arc::new(RoomClient::new(signaling.clone()));
    let recorder = Arc::new(Recorder::default());
    room.install_event_sink(recorder.clone()).await;

    // What `lib.rs` installs: after every AUTH_OK, rejoin the active room.
    {
        let rc = room.clone();
        signaling
            .on_authenticated(move || {
                let rc = rc.clone();
                tokio::spawn(async move {
                    let _ = rc.rejoin_active_room().await;
                });
            })
            .await;
    }
    room.init().await;
    signaling.start().await.expect("start");
    let rc = room.clone();
    tokio::spawn(async move { rc.run_inbound().await });
    Client {
        _dir: dir,
        signaling,
        room,
        recorder,
    }
}

async fn until<F, Fut>(what: &str, secs: u64, mut f: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = Instant::now() + Duration::from_secs(secs);
    while !f().await {
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn playback_cmd(room_id: Uuid, action: PlaybackAction, seq: u64, pos: u64) -> Envelope {
    Envelope {
        v: 1,
        r#type: MessageKind::PlaybackCmd,
        id: Uuid::now_v7(),
        room_id: Some(room_id),
        sender: None,
        ts_ms: 0,
        seq,
        payload: serde_json::to_value(PlaybackCommandPayload {
            action,
            monotonic_seq: seq,
            media_position_ms: pos,
            client_ts_ms: 0,
        })
        .unwrap(),
    }
}

// ---------------------------------------------------------------------------
// the scenario
// ---------------------------------------------------------------------------

/// The host plays, the viewer's network drops, the host pauses and then seeks
/// to the end of a three hour film while the viewer is gone, and the network
/// returns. The viewer must be back in the room with the CURRENT playback
/// state, and must hear the host's next command.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_viewer_whose_network_drops_comes_back_in_step_with_the_host() {
    let server = start_server().await;
    let proxy = Proxy::start(server).await;
    let host = client(format!("ws://{server}/ws"), "host").await;
    let viewer = client(format!("ws://{}/ws", proxy.addr), "viewer").await;

    until("both authenticated", 15, || async {
        host.signaling.snapshot().await.phase == ConnPhase::Authenticated
            && viewer.signaling.snapshot().await.phase == ConnPhase::Authenticated
    })
    .await;

    // Host creates the room, the viewer joins.
    let created = host
        .room
        .room_create("film night".into(), false)
        .await
        .expect("create");
    let room_id = Uuid::parse_str(&created.id).unwrap();
    viewer
        .room
        .room_join(created.code.clone(), "viewer".into())
        .await
        .expect("join");

    // Host: PLAY at 20 s. The viewer sees it live.
    host.signaling
        .send_envelope(playback_cmd(room_id, PlaybackAction::Play, 1, 20_000))
        .await
        .expect("send play");
    until("viewer sees PLAY", 10, || async {
        viewer.recorder.playback.lock().unwrap().len() == 1
    })
    .await;

    // The viewer's network goes down.
    proxy.sever();
    until("viewer notices the outage", 10, || async {
        viewer.signaling.snapshot().await.phase != ConnPhase::Authenticated
    })
    .await;

    // While it is gone the host pauses, then jumps to 2h59m50s.
    let end = 10_790_000u64;
    host.signaling
        .send_envelope(playback_cmd(room_id, PlaybackAction::Pause, 2, 27_000))
        .await
        .expect("send pause");
    host.signaling
        .send_envelope(playback_cmd(room_id, PlaybackAction::Seek, 3, end))
        .await
        .expect("send seek");
    until("the server applied both", 10, || async {
        // The host's own cached summary carries the server's view.
        host.room.state().await.is_some()
    })
    .await;

    // The network returns; the client reconnects on its own.
    proxy.restore();
    until("viewer reconnects", 30, || async {
        viewer.signaling.snapshot().await.phase == ConnPhase::Authenticated
    })
    .await;

    eprintln!(
        "test: after reconnect, state = {:?}",
        viewer.room.state().await.map(|s| (s.id, s.playback))
    );
    tokio::time::sleep(Duration::from_secs(2)).await;
    eprintln!(
        "test: 2s later, state = {:?}; states recorded = {}",
        viewer.room.state().await.map(|s| s.playback),
        viewer.recorder.states.lock().unwrap().len()
    );
    // Back in the room, with the current state (paused, at the new position).
    // (The re-join that follows the reconnect is refused, because the server
    // still has the viewer in the room; that must not clear the cached room.)
    let current = |s: &RoomSummaryIpc| s.playback.as_ref().is_some_and(|p| p.server_seq == 3);
    until("viewer is told the current playback state", 20, || async {
        match viewer.room.state().await {
            Some(s) => current(&s),
            None => false,
        }
    })
    .await;
    let s = viewer.room.state().await.expect("still in the room");
    let p = s.playback.clone().expect("snapshot");
    assert!(p.paused, "the host paused before the seek: {p:?}");
    assert_eq!(p.media_position_ms, end);
    assert_eq!(s.id, created.id, "same room");

    // The viewer is still a participant (nobody saw it leave for good).
    assert!(s.participants.iter().any(|x| !x.is_host), "{s:?}");

    // And it hears what the host does next.
    host.signaling
        .send_envelope(playback_cmd(room_id, PlaybackAction::Play, 4, end))
        .await
        .expect("send play");
    until("viewer hears the next command", 10, || async {
        viewer
            .recorder
            .playback
            .lock()
            .unwrap()
            .iter()
            .any(|e| e.server_seq == 4)
    })
    .await;
}

/// A refused request says why, and does not tear down a session that exists.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_join_reports_the_servers_reason_and_keeps_the_current_room() {
    use locast_client_lib::net::room::{RoomClientError, RoomErrorCodeIpc};

    let server = start_server().await;
    let host = client(format!("ws://{server}/ws"), "host").await;
    let viewer = client(format!("ws://{server}/ws"), "viewer").await;
    until("both authenticated", 15, || async {
        host.signaling.snapshot().await.phase == ConnPhase::Authenticated
            && viewer.signaling.snapshot().await.phase == ConnPhase::Authenticated
    })
    .await;
    let created = host
        .room
        .room_create("film night".into(), false)
        .await
        .expect("create");

    // A code nobody has: the reason is the server's, not a decode failure.
    let err = viewer
        .room
        .room_join("AAAAAA".into(), "viewer".into())
        .await
        .expect_err("no such room");
    assert!(
        matches!(
            err,
            RoomClientError::Server {
                code: RoomErrorCodeIpc::RoomNotFound,
                ..
            }
        ),
        "got {err:?}"
    );

    // Join for real, then try a second, bad join: the first room survives.
    viewer
        .room
        .room_join(created.code.clone(), "viewer".into())
        .await
        .expect("join");
    let again = viewer
        .room
        .room_join("AAAAAA".into(), "viewer".into())
        .await
        .expect_err("still no such room");
    assert!(
        matches!(again, RoomClientError::Server { .. }),
        "got {again:?}"
    );
    let still = viewer.room.state().await.expect("still in the first room");
    assert_eq!(still.id, created.id);
}
