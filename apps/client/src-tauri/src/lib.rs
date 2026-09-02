//! Locast desktop client library.
//!
//! P0-T02: wires up the Tauri 2 builder, opens a single main window, and
//! initializes the section-5 plugin set. The IPC command registry and the
//! storage layer are added in P0-T05, P0-T06, and P1-T04..P1-T07.
//! P1-T08: registers the `locast://` URI scheme handler and adds
//! `media_resolve_url` to the IPC surface.
//! P2-T01: installs the `IdentityService` as managed state and
//! exposes `identity_get` / `identity_rotate` /
//! `identity_set_display_name` over IPC.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

use tauri::Manager as _;
use tauri_plugin_log::{Target, TargetKind};

pub mod commands;
pub mod core;
pub mod events;
pub mod identity;
pub mod library;
pub mod net;
pub mod probe;
pub mod room;
pub mod storage;
pub mod transfer;

use identity::keystore::IdentityService;
use library::protocol::ProtocolHandler;
use net::config::SignalingConfig;
use net::room::RoomClient;
use net::signaling::SignalingClient;
use transfer::events::{DownloadEventEmitter, NoopSink};

/// Library version string. Bumped per release alongside the workspace.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Library name. Used by downstream binaries and tests.
pub fn name() -> &'static str {
    "locast-client"
}

/// P3-T08: process-global handle to the live download event
/// emitter. Written exactly once during Tauri `setup()` in
/// non-test builds; the `ReceiverSession` ctor reads from it
/// so the transfer layer never has to be re-threaded with a
/// Tauri handle.
///
/// Tests do NOT call `install_download_event_emitter`, so
/// the `OnceLock` stays empty and the transfer layer falls
/// back to a [`NoopSink`].
static DOWNLOAD_EVENT_EMITTER: std::sync::OnceLock<std::sync::Arc<DownloadEventEmitter>> =
    std::sync::OnceLock::new();

/// P3-T08: install the global download event emitter.
/// Called once during Tauri `setup()` in non-test builds.
#[cfg(not(test))]
pub fn install_download_event_emitter(emitter: std::sync::Arc<DownloadEventEmitter>) {
    let _ = DOWNLOAD_EVENT_EMITTER.set(emitter);
}

/// P3-T08: read the global download event emitter, falling
/// back to a fresh [`NoopSink`]-backed emitter when one has
/// not been installed. Used by `ReceiverSession` when it
/// needs to talk to the live `AppHandle`.
pub fn get_download_event_emitter() -> std::sync::Arc<DownloadEventEmitter> {
    DOWNLOAD_EVENT_EMITTER.get().cloned().unwrap_or_else(|| {
        std::sync::Arc::new(DownloadEventEmitter::new(std::sync::Arc::new(NoopSink)))
    })
}

/// Build and run the Locast desktop client.
///
/// The Tauri 2 builder is configured per `docs/ARCHITECTURE.md` section 5:
/// a single main window is opened, and only the plugins whose capabilities
/// are listed in `capabilities/default.json` are initialized. No shell or
/// arbitrary HTTP capability is granted to the webview; all privileged
/// filesystem and network operations stay in Rust.
pub fn run() {
    let log_plugin = tauri_plugin_log::Builder::new()
        .targets([
            Target::new(TargetKind::Stdout),
            Target::new(TargetKind::LogDir { file_name: None }),
        ])
        .level(log::LevelFilter::Info)
        .build();

    // P3-T13 review fix H#28: when the main window is closing,
    // cancel every in-flight transfer so the spawned
    // orchestrators exit before the process tears down.
    // Stash the registry handle (created below inside
    // `setup`) in a closure-captured cell so the
    // `Builder::on_window_event` hook can reach it.
    let registry_cell: std::sync::Arc<
        std::sync::Mutex<Option<std::sync::Arc<transfer::TransferRegistry>>>,
    > = std::sync::Arc::new(std::sync::Mutex::new(None));
    let registry_cell_for_setup = registry_cell.clone();
    let registry_cell_for_event = registry_cell.clone();
    tauri::Builder::default()
        .plugin(log_plugin)
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_os::init())
        .plugin(tauri_plugin_process::init())
        .setup(move |app| {
            let _registry_cell = registry_cell_for_setup;
            let _main = app
                .get_webview_window("main")
                .expect("main window missing from tauri.conf.json");

            // P1-T04: open (or create) the local SQLite database and
            // install it as managed state so `media_import` (and future
            // commands) can take `tauri::State<'_, Storage>`. The path
            // is hardcoded to the per-user app-data dir; P1-T05 will
            // replace this with a settings-driven location.
            //
            // P1-T05: also build a `QuotaAccountant` against the same
            // storage and install it as managed state so the quota
            // commands can take `tauri::State<'_, QuotaAccountant>`.
            // The library root is the parent of the SQLite file
            // (`<app_data_dir>/index.sqlite` => `app_data_dir`); the
            // Settings UI is the future task that lets the user pick
            // a different root.
            //
            // P1-T08: install a `ProtocolHandler` against the same
            // library root so the `locast://` scheme can be
            // registered with Tauri. The library root is the parent
            // of the SQLite file (matches the convention in
            // `commands::import::resolve_library_root`).
            //
            // P2-T01: install the `IdentityService` so the
            // `identity_*` commands can take
            // `tauri::State<'_, IdentityService>`. The service holds
            // a clone of the storage handle (cheap) and a real OS
            // keyring.
            //
            // `tauri::async_runtime::block_on` is used because `setup`
            // is synchronous; the open + migrate step is bounded by
            // the SQLx pool warm-up and the single 0001_init migration.
            let data_dir = app.path().app_data_dir().expect("resolve app data dir");
            std::fs::create_dir_all(&data_dir).expect("create app data dir");
            let db_path = data_dir.join("index.sqlite");
            let storage =
                tauri::async_runtime::block_on(async { storage::Storage::open(&db_path).await })
                    .expect("open storage");
            let accountant = core::quota::QuotaAccountant::new(storage.clone());
            let library_root = data_dir.clone();
            let protocol_handler = ProtocolHandler::new(storage.clone(), library_root);
            let identity_service = std::sync::Arc::new(IdentityService::new(storage.clone()));

            // P3-T04 prerequisite 4: clone the storage pool
            // BEFORE the `Storage` is moved into
            // `app.manage` below, so the RoomClient can be
            // given a copy for persistence.
            let storage_pool = storage.pool();
            app.manage(storage.clone());
            app.manage(accountant);
            app.manage(protocol_handler);
            app.manage(identity_service.clone());
            let signaling_config = SignalingConfig::from_env();
            let signaling_client = std::sync::Arc::new(SignalingClient::new(
                signaling_config,
                identity_service.clone(),
            ));
            // P2-T04: the RoomClient piggy-backs on the
            // signaling WS. Subscribe it to the signaling
            // client's inbound envelope stream so the
            // `room_*` commands can send ROOM_* envelopes
            // and observe the server's replies.
            //
            // P2-T05: install the Tauri `AppHandle` so
            // the client can emit `room://state` and
            // `room://event` events.
            let room_client = std::sync::Arc::new(RoomClient::new(signaling_client.clone()));
            // P3-T04 prerequisite 4: give the RoomClient the
            // local SQLite pool so the inbound
            // MANIFEST_PUBLISHED handler can persist
            // verified manifests to the local
            // `room_manifests` table.
            room_client.set_storage_pool(storage_pool);
            let app_handle_for_room = app.handle().clone();
            let app_handle_for_downloads = app.handle().clone();
            // P3-T13: build the WebRtcManager BEFORE the block_on so
            // we can register it with `app.manage` afterward. The
            // inbound loop and any peer-connection lifecycle is
            // started from inside the block_on.
            let webrtc_manager = std::sync::Arc::new(net::webrtc::WebRtcManager::new(
                signaling_client.clone(),
                identity_service.clone(),
                room_client.clone(),
            ));
            let webrtc_for_state = webrtc_manager.clone();
            tauri::async_runtime::block_on(async {
                room_client.init().await;
                // P2-T05: install the Tauri `AppHandle` so
                // the client can emit `room://state` and
                // `room://event` events. The Tauri-backed
                // sink is only compiled in non-test builds;
                // tests must use `install_event_sink` (or
                // leave the sink as `None`).
                #[cfg(not(test))]
                {
                    use net::room::RoomEventSink;
                    use net::room::TauriEventSink;
                    let sink: std::sync::Arc<dyn RoomEventSink> =
                        std::sync::Arc::new(TauriEventSink::new(app_handle_for_room));
                    room_client.install_event_sink(sink).await;
                }
                #[cfg(test)]
                {
                    let _ = app_handle_for_room;
                }
                let rc = room_client.clone();
                tokio::spawn(async move { rc.run_inbound().await });

                // P3-T08: install the Tauri-backed download
                // event emitter so ReceiverSession instances
                // spawned later (P3-T09+) emit `download://state`
                // and `download://progress` events to the
                // webview. The emitter is stored in a
                // process-global OnceLock that `get_download_
                // event_emitter` reads.
                #[cfg(not(test))]
                {
                    use transfer::events::TauriDownloadEventSink;
                    let sink: std::sync::Arc<dyn transfer::events::DownloadEventSink> =
                        std::sync::Arc::new(TauriDownloadEventSink::new(
                            app_handle_for_downloads.clone(),
                        ));
                    install_download_event_emitter(std::sync::Arc::new(DownloadEventEmitter::new(
                        sink,
                    )));
                }
                #[cfg(test)]
                {
                    let _ = app_handle_for_downloads;
                }

                // P3-T05: start the WebRTC PeerConnection
                // manager's inbound loop. The manager subscribes
                // to the signaling client's inbound envelope
                // stream and, on room-state changes (polled at
                // 200 ms -- a deliberate P3-T05 simplification;
                // see `net::webrtc` module-level docs), creates /
                // tears down per-peer PeerConnections and
                // exchanges SDP / ICE over the new SIGNAL
                // envelope.
                webrtc_manager
                    .clone()
                    .start_with_room_client(room_client.clone());
            });
            app.manage(signaling_client);
            app.manage(room_client.clone());
            // P3-T13: register the WebRtcManager + TransferRegistry
            // as managed state so `download_open` (and future
            // commands) can reach them. The manager itself is
            // already alive (the inbound loop was spawned above);
            // `app.manage` is a Tauri-side alias that only sets a
            // `tauri::State` lookup key.
            app.manage(webrtc_for_state.clone());
            // P3-T13: a fresh TransferRegistry. Spawning a transfer
            // registers it here; `cancel_all()` is wired into
            // future shutdown / room_leave paths.
            let registry = std::sync::Arc::new(transfer::TransferRegistry::new());
            app.manage(registry.clone());

            // P3-T15: install the host-side sender dispatch
            // on the WebRtcManager. The dispatch consults the
            // host's verified manifest and serves chunks over
            // the inbound `files` DataChannel. The local
            // pubkey is derived from the OS keyring; if the
            // keyring read fails (e.g. on a fresh install
            // where the keypair has not been generated yet),
            // the dispatch is skipped and the manager falls
            // back to the viewer-only path. The next
            // `identity_get` will create the keypair, and a
            // future setup-hook replay will re-install the
            // dispatch.
            {
                let webrtc_for_dispatch = webrtc_for_state.clone();
                let identity_for_dispatch = identity_service.clone();
                let storage_for_dispatch = storage.clone();
                let library_root_for_dispatch = data_dir.clone();
                let room_client_for_dispatch = room_client.clone();
                let cancel_for_dispatch = webrtc_for_dispatch.cancel_token();
                tauri::async_runtime::spawn(async move {
                    match identity_for_dispatch.load_keypair().await {
                        Ok(kp) => {
                            let pubkey = kp.signing.verifying_key().to_bytes();
                            let ctx = transfer::HostDispatchContext::new(
                                storage_for_dispatch,
                                library_root_for_dispatch,
                                room_client_for_dispatch,
                                pubkey,
                                cancel_for_dispatch,
                            );
                            let dispatch = transfer::HostSenderDispatcher::new(ctx);
                            webrtc_for_dispatch.set_host_dispatch(dispatch);
                        }
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                "host dispatch: load_keypair failed; manager is viewer-only"
                            );
                        }
                    }
                });
            }

            // P3-T13 review fix H#28: when the main window is
            // closing, cancel every in-flight transfer so the
            // spawned orchestrators exit before the process
            // tears down. Stash the registry handle in the
            // closure-captured cell so the `Builder::on_window_
            // event` hook (registered outside `setup`) can
            // reach it.
            {
                let mut slot = _registry_cell.lock().expect("registry_cell mutex poisoned");
                *slot = Some(registry.clone());
            }

            Ok(())
        })
        .on_window_event(move |_window, event| {
            if let tauri::WindowEvent::CloseRequested { .. } = event {
                let slot = registry_cell_for_event
                    .lock()
                    .expect("registry_cell mutex poisoned");
                if let Some(r) = slot.as_ref() {
                    let r = r.clone();
                    drop(slot);
                    tauri::async_runtime::spawn(async move {
                        r.cancel_all().await;
                    });
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::greet,
            commands::import::media_import,
            commands::quota::quota_get,
            commands::quota::quota_set,
            commands::scan::library_scan,
            commands::protocol::media_resolve_url,
            commands::identity::identity_get,
            commands::identity::identity_rotate,
            commands::identity::identity_set_display_name,
            commands::signaling::signaling_get_state,
            commands::signaling::signaling_connect,
            commands::signaling::signaling_disconnect,
            commands::room::room_connect_signaling,
            commands::room::room_create,
            commands::room::room_join,
            commands::room::room_leave,
            commands::room::room_get_state,
            commands::room::recent_rooms_list,
            commands::room::recent_room_upsert,
            commands::room::manifest_publish,
            commands::room::manifest_fetch,
            commands::download::download_open,
        ])
        .register_asynchronous_uri_scheme_protocol("locast", |ctx, request, responder| {
            // P1-T08: the `locast://` URI scheme handler. Tauri
            // hands us the request off the main thread; we
            // resolve the URL, look up the row, and return the
            // file content (or a Range slice) as an HTTP
            // response. The handler state is fetched from the
            // app handle's managed state.
            let url = request.uri().to_string();
            let method = request.method().to_string();
            let range = request
                .headers()
                .get("range")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string());
            let app_handle = ctx.app_handle().clone();
            tauri::async_runtime::spawn(async move {
                let handler = app_handle.state::<ProtocolHandler>();
                let res = handler.handle(&url, &method, range.as_deref()).await;
                let response = match res {
                    Ok(r) => protocol_response_to_tauri(r).await,
                    Err(e) => error_to_tauri(&e),
                };
                responder.respond(response);
            });
        })
        .run(tauri::generate_context!())
        .expect("error while running Locast desktop client");
}

/// Convert a `ProtocolResponse` into a Tauri `Response`. For
/// the 200 / Full path we read the file from disk and return
/// its bytes; for the 206 / Range path we stream the requested
/// window. Errors are flattened to a plain text body.
async fn protocol_response_to_tauri(
    r: library::protocol::ProtocolResponse,
) -> tauri::http::Response<Vec<u8>> {
    use tauri::http::{HeaderValue, Response, StatusCode};
    let status = StatusCode::from_u16(r.status).unwrap_or(StatusCode::OK);
    let mut builder = Response::builder().status(status);
    for (k, v) in &r.headers {
        builder = builder.header(k.as_str(), HeaderValue::from_str(v).unwrap());
    }
    match r.body {
        library::protocol::ResponseBody::Full(bytes) => builder.body(bytes).unwrap(),
        library::protocol::ResponseBody::Range {
            path,
            start,
            length,
        } => {
            let mut out: Vec<u8> = Vec::with_capacity(length.min(8 * 1024 * 1024) as usize);
            let mut cursor = std::io::Cursor::new(Vec::<u8>::new());
            let write_result =
                library::protocol::stream_range(&path, start, length, &mut cursor).await;
            if write_result.is_ok() {
                out = cursor.into_inner();
            }
            builder.body(out).unwrap()
        }
        library::protocol::ResponseBody::File(path) => {
            // For the no-Range 200 path, read the whole file
            // into memory. Tauri 2's Response body is bounded
            // by an in-memory `Vec<u8>`; for very large media
            // (multi-GiB) the webview should issue a Range
            // request after the first byte anyway, and the
            // chunked path is what serves the bulk of the
            // bytes. The 200 path is mostly used for tiny
            // subtitle files and the very first frame; if a
            // user has a multi-GiB file and the webview asks
            // for the whole thing, we accept the memory cost
            // and the user can restart with Range support.
            let bytes = tokio::fs::read(&path).await.unwrap_or_default();
            builder.body(bytes).unwrap()
        }
    }
}

fn error_to_tauri(e: &library::protocol::ProtocolError) -> tauri::http::Response<Vec<u8>> {
    use tauri::http::{Response, StatusCode};
    let status = match e {
        library::protocol::ProtocolError::NotFound(_) => StatusCode::NOT_FOUND,
        library::protocol::ProtocolError::BadRange(_) => StatusCode::RANGE_NOT_SATISFIABLE,
        library::protocol::ProtocolError::OutOfLibrary(_) => StatusCode::FORBIDDEN,
        library::protocol::ProtocolError::BadUrl(_) => StatusCode::BAD_REQUEST,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    Response::builder()
        .status(status)
        .header("Content-Type", "text/plain; charset=utf-8")
        .body(e.to_string().into_bytes())
        .unwrap()
}
