//! IPC command registry for the Locast desktop client.
//!
//! P0-T06 introduced the first command, `greet`, to prove the
//! `tauri-specta` -> TypeScript bindings toolchain end-to-end.
//! P1-T04 added the first non-trivial command, `media_import`, which
//! ingests one or more files into the local library with content-
//! addressed dedup. P1-T05 added `quota_get` and `quota_set` over the
//! disk-quota engine. P1-T07 added `library_scan`, which reconciles
//! the on-disk library against the `media_items` table. The companion
//! `events` module is empty for P1-T07. P2-T01 extracted the closed
//! `AppError` set into its own `commands::error` module so multiple
//! IPC consumers can depend on the same error type; the legacy
//! `commands::import::AppError` re-export is preserved so every
//! existing caller keeps compiling unchanged.

pub mod error;
pub mod identity;
pub mod import;
pub mod protocol;
pub mod quota;
pub mod room;
pub mod scan;
pub mod signaling;

pub use error::AppError;
pub use identity::{identity_get, identity_rotate, identity_set_display_name};
pub use import::{media_import, AppError as _AppErrorCompat, ImportedMedia};
pub use protocol::media_resolve_url;
pub use quota::{quota_get, quota_set, QuotaInfo};
pub use room::{
    recent_room_upsert, recent_rooms_list, room_connect_signaling, room_create, room_get_state,
    room_join, room_leave,
};
pub use scan::{library_scan, ScanResult};
pub use signaling::{signaling_connect, signaling_disconnect, signaling_get_state};

/// The single Tauri command exposed to the webview in P0-T06.
///
/// Returns a fixed greeting so the React side can confirm the IPC
/// round-trip and render the generated binding. The lib's
/// `invoke_handler` will register this in the bindings-generation
/// bring-up that follows P0-T06.
#[tauri::command]
#[specta::specta]
pub fn greet() -> String {
    "Hello, Locast".to_string()
}
