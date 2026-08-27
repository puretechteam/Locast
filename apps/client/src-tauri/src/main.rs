//! Locast desktop client entry point.
//!
//! P0-T02: delegates to the library so integration tests can exercise the
//! Tauri 2 builder. The real application logic (commands, `locast://`
//! protocol, IPC surface) lands in P0-T06 and P1+.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    locast_client_lib::run();
}
