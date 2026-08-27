//! Locast desktop client library.
//!
//! P0-T02: wires up the Tauri 2 builder, opens a single main window, and
//! initializes the section-5 plugin set. The IPC command registry and the
//! storage layer are added in P0-T05, P0-T06, and P1-T04..P1-T07.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

use tauri_plugin_log::{Target, TargetKind};

pub mod commands;
pub mod core;
pub mod events;
pub mod library;
pub mod probe;
pub mod storage;

/// Library version string. Bumped per release alongside the workspace.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Library name. Used by downstream binaries and tests.
pub fn name() -> &'static str {
    "locast-client"
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

    tauri::Builder::default()
        .plugin(log_plugin)
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_os::init())
        .plugin(tauri_plugin_process::init())
        .setup(|app| {
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
            // `tauri::async_runtime::block_on` is used because `setup`
            // is synchronous; the open + migrate step is bounded by
            // the SQLx pool warm-up and the single 0001_init migration.
            use tauri::Manager as _;
            let data_dir = app.path().app_data_dir().expect("resolve app data dir");
            std::fs::create_dir_all(&data_dir).expect("create app data dir");
            let db_path = data_dir.join("index.sqlite");
            let storage =
                tauri::async_runtime::block_on(async { storage::Storage::open(&db_path).await })
                    .expect("open storage");
            let accountant = core::quota::QuotaAccountant::new(storage.clone());

            app.manage(storage);
            app.manage(accountant);

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::greet,
            commands::import::media_import,
            commands::quota::quota_get,
            commands::quota::quota_set,
            commands::scan::library_scan,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Locast desktop client");
}
