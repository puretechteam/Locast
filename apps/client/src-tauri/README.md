# Locast desktop client (Rust core)

This crate is the Rust core of the Locast desktop client. P0-T01 establishes it as an empty workspace member so the Cargo workspace compiles; P0-T02 (Tauri 2 scaffold) adds the Tauri app, the webview entry, IPC commands, and the `locast://` protocol placeholder. P1+ adds the storage, signaling, transfer, and playback modules described in `docs/ARCHITECTURE.md` section 26.2.
