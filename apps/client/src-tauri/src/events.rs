//! IPC event registry for the Locast desktop client.
//!
//! P0-T06 does not define any events. This module exists so the
//! `tauri-specta` bindings generator has a stable path to call
//! `collect_events![]` against. P3+ tasks (download progress, room
//! state, etc.) add concrete event types here and register them via
//! `tauri_specta::collect_events!` in the bindings generator.
