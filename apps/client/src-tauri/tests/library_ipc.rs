//! P1-T09 IPC test: drives the library commands through Tauri's real
//! invoke path (argument deserialisation, managed state, result
//! serialisation) on the mock runtime, using the exact command names and
//! argument keys the TypeScript bindings send. No window and no user
//! profile is involved: storage lives in a tempdir.
//!
//! This is the contract the Library page depends on:
//!   media_import        { paths }
//!   library_list        { query, limit, offset }
//!   library_make_permanent { id }
//!   library_delete      { id }
//!
//! Run with `cargo test -p locast-client --test library_ipc -j 1`.
//!
//! Windows note: like `gen_bindings.rs`, this binary cannot start on Windows
//! hosts whose bundled `WebView2Loader.dll` is incompatible with the installed
//! WebView2 runtime (`STATUS_ENTRYPOINT_NOT_FOUND`), so it is compiled only on
//! Linux and macOS, where CI runs it.

#![cfg(not(target_os = "windows"))]

use locast_client_lib::core::quota::QuotaAccountant;
use locast_client_lib::storage::Storage;
use serde_json::{json, Value};
use tauri::ipc::{CallbackFn, InvokeBody};
use tauri::test::{get_ipc_response, mock_context, noop_assets, MockRuntime, INVOKE_KEY};
use tauri::webview::InvokeRequest;
use tauri::{Manager, WebviewWindow, WebviewWindowBuilder};
use tempfile::TempDir;

struct Harness {
    window: WebviewWindow<MockRuntime>,
    // Held so the storage directory outlives the test.
    dir: TempDir,
    src: TempDir,
}

fn harness() -> Harness {
    let dir = TempDir::new().expect("tempdir");
    // Same runtime and call shape as `lib.rs` setup.
    let storage = tauri::async_runtime::block_on(async {
        Storage::open(dir.path().join("index.sqlite")).await
    })
    .expect("storage opens");
    let accountant = QuotaAccountant::new(storage.clone());

    let app = tauri::test::mock_builder()
        .manage(storage)
        .manage(accountant)
        .manage(std::sync::Arc::new(
            locast_client_lib::transfer::TransferRegistry::new(),
        ))
        .invoke_handler(tauri::generate_handler![
            locast_client_lib::commands::import::media_import,
            locast_client_lib::commands::library::library_list,
            locast_client_lib::commands::library::library_make_permanent,
            locast_client_lib::commands::library::library_delete
        ])
        .build(mock_context(noop_assets()))
        .expect("mock app builds");
    let window = WebviewWindowBuilder::new(&app, "main", Default::default())
        .build()
        .expect("mock window builds");
    // Keep the app alive for the lifetime of the window handle.
    std::mem::forget(app);
    Harness {
        window,
        dir,
        src: TempDir::new().expect("source tempdir"),
    }
}

impl Harness {
    fn invoke(&self, cmd: &str, body: Value) -> Result<Value, Value> {
        get_ipc_response(
            &self.window,
            InvokeRequest {
                cmd: cmd.into(),
                callback: CallbackFn(0),
                error: CallbackFn(1),
                // The local origin on Linux and macOS (it is `http://tauri.localhost`
                // only on Windows). Any other origin is treated as remote and is
                // refused by Tauri's ACL for lack of a remote capability.
                url: "tauri://localhost".parse().expect("url"),
                body: InvokeBody::Json(body),
                headers: Default::default(),
                invoke_key: INVOKE_KEY.to_string(),
            },
        )
        .map(|b| b.deserialize::<Value>().expect("json response"))
    }

    fn write_source(&self, name: &str, seed: u8) -> String {
        let p = self.src.path().join(name);
        std::fs::write(&p, vec![seed; 4096]).expect("write source");
        p.to_string_lossy().into_owned()
    }
}

#[test]
fn import_list_search_make_permanent_delete_over_ipc() {
    let h = harness();

    // Empty library.
    let empty = h
        .invoke(
            "library_list",
            json!({ "query": null, "limit": null, "offset": null }),
        )
        .expect("library_list");
    assert_eq!(empty, json!([]));

    // Import two files exactly as `commands.mediaImport(paths)` sends them.
    let a = h.write_source("Movie Night.mp4", 3);
    let b = h.write_source("Holiday Clip.mkv", 5);
    let imported = h
        .invoke("media_import", json!({ "paths": [a, b] }))
        .expect("media_import");
    assert_eq!(imported.as_array().expect("array").len(), 2);

    // List: the JSON shape the TypeScript `LibraryItem` type declares.
    let listed = h
        .invoke(
            "library_list",
            json!({ "query": null, "limit": null, "offset": null }),
        )
        .expect("library_list");
    let rows = listed.as_array().expect("array");
    assert_eq!(rows.len(), 2);
    let movie = rows
        .iter()
        .find(|r| r["filename"] == "Movie Night.mp4")
        .expect("movie listed");
    for key in [
        "id",
        "sha256",
        "filename",
        "size_bytes",
        "duration_ms",
        "width",
        "height",
        "video_codec",
        "audio_codec",
        "container",
        "status",
        "created_at",
    ] {
        assert!(movie.get(key).is_some(), "missing key {key}: {movie}");
    }
    assert_eq!(movie["size_bytes"], 4096);
    assert_eq!(movie["status"], "permanent");
    assert!(movie["created_at"].as_i64().expect("number") > 0);

    // Search (a real FTS5 query through the real command).
    let found = h
        .invoke(
            "library_list",
            json!({ "query": "movie", "limit": null, "offset": null }),
        )
        .expect("search");
    assert_eq!(found.as_array().expect("array").len(), 1);
    assert_eq!(found[0]["filename"], "Movie Night.mp4");

    // Make permanent is accepted (and idempotent) for a real id.
    let id = movie["id"].as_str().expect("id").to_string();
    h.invoke("library_make_permanent", json!({ "id": id }))
        .expect("make permanent");

    // Unknown ids surface as a rejected invoke (the store shows a notice).
    let err = h
        .invoke("library_make_permanent", json!({ "id": "nope" }))
        .expect_err("unknown id rejects");
    assert_eq!(err["kind"], "NotFound", "{err}");

    // Delete: row gone from list and search; file is in <root>/trash.
    h.invoke("library_delete", json!({ "id": id }))
        .expect("delete");
    let after = h
        .invoke(
            "library_list",
            json!({ "query": null, "limit": null, "offset": null }),
        )
        .expect("list after delete");
    assert_eq!(after.as_array().expect("array").len(), 1);
    let gone = h
        .invoke(
            "library_list",
            json!({ "query": "movie", "limit": null, "offset": null }),
        )
        .expect("search after delete");
    assert_eq!(gone, json!([]));
    assert!(h.dir.path().join("trash").is_dir(), "trash dir created");

    // Keep the harness (and its tempdirs) alive until the end.
    let _ = h.window.app_handle();
}
