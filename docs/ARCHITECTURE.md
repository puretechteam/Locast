# Locast Architecture

## About this document

This document is the canonical architecture for Locast, a local-first watch-together application. It is the single source of truth for the project's design. Implementation work is organized against it; the roadmap (`docs/ROADMAP.md`) is the broken-down plan.

This is version 0 of the assembled document. It was stitched from seventeen per-section drafts under `design/drafts/`. The original prompt assigned sections 1-30; in this assembled revision:

- The intro and TOC below stand in for the planned section 1 (executive summary preamble).
- Sections 23 and 24 (which were originally separate) are combined in this document under section 13 as "Media lifecycle (temporary and permanent)" because the underlying state machine and `media_items` table are shared; see draft 12 Part B for the source.

The substance here is lifted directly from the drafts. No new technical content has been invented. Where a draft flagged an item as "Decisions deferred", those items have been pulled out of the individual sections and collected into a single "Open Decisions to Confirm with Stakeholders" appendix at the end of this document. The other "Decisions deferred" notes that were specific to a section (and not yet flagged for user confirmation) have been left in place inside the relevant section as "Implementation notes (draft deferred items)" so the original context is preserved.

Heading conventions: `#` for the document title, `##` for each numbered section (2-30), `###` for subheadings.

> **v1 Locked Decisions (do not change without product review):**
> - WebSocket server is authoritative for room control, sync, drawing, laser, chat, presence, permissions.
> - WebRTC DataChannel is used ONLY for complete-file transfer during download.
> - TURN is required as a fallback when direct ICE fails.
> - Storage cap counts permanent + temporary + in-flight download bytes.
> - `locast://` is local-only file serving to the app's own webview; it is not internet streaming.
> - Host disconnect (30s grace) ends the room. Host transfer is deferred to v1.1.

## Table of contents

- 2. Recommended Technology Stack
- 3. High-Level Architecture
- 4. Component Architecture
- 5. Client Architecture (Tauri 2 specifics)
- 6. Native Filesystem / Storage Architecture
- 7. Local SQLite Schema
- 8. Media Manifest Design
- 9. Download / P2P Architecture
- 10. Room Architecture
- 11. Room State Machine
- 12. Playback Synchronization Architecture
- 13. Exact Synchronization Semantics
- 14. Permission Model
- 15. Drawing Protocol
- 16. Laser Pointer Protocol
- 17. Subtitle Architecture
- 18. Network Protocol Design
- 19. WebRTC and Signaling Architecture
- 20. Server Architecture
- 21. Security Model
- 22. Reconnection, Failure Handling, and Media Lifecycle
- 23. Media Lifecycle (temporary and permanent)
- 24. (Combined with section 23; see draft 12 Part B)
- 25. UI / UX Structure
- 26. Suggested Project / Repository Structure
- 27. Testing Strategy
- 28. Performance Considerations
- 29. Major Technical Risks
- 30. Decisions That Should Be Deferred Until Later
- Appendix A. Open Decisions to Confirm with Stakeholders

## 2. Recommended Technology Stack

All picks are chosen to be boring, well-maintained, and suitable for a Tauri 2 desktop app. Each is justified in one or two sentences.

### Desktop shell

| Choice | Version | Justification |
|---|---|---|
| Tauri 2 | 2.x stable | Required by the spec. Bundles a system WebView (Edge WebView2 on Windows, WebKit on macOS, WebKitGTK on Linux), ships a capability-based permission model, and exposes a small Rust-JS IPC. Smaller and faster than Electron; we need a real browser engine for `<video>`. |
| Rust | stable, MSRV 1.75 | Required by Tauri 2. 1.75 covers the ed25519-dalek, sqlx, and tokio versions we want. No nightly features. |
| Tokio | 1.x (multi-thread) | The async runtime for the Rust core: WebSocket client, WebRTC signaling, DataChannel I/O, chunk transfer, hash workers, periodic disk scans. |
| `axum` | 0.7+ | Lightweight HTTP server for the embedded Tauri sidecar (optional: health endpoint, local status page). Also a fine router for any local-only HTTP. |
| `sqlx` (sqlite) | 0.8+ | Async, compile-time-checked SQL, supports migrations, plays well with tokio. Preferred over `rusqlite` because all our DB work is async and we want connection pooling. WAL mode + busy timeout handled at startup via PRAGMAs. |
| `serde` / `serde_json` | 1.x | Manifest, settings, IPC payloads. |
| `tokio-tungstenite` | 0.23+ | Pure-Rust WebSocket client for the signaling connection. No native dependencies. |
| WebRTC: `webrtc-rs` (via `webrtc` crate) | 0.10+ | Pure-Rust WebRTC implementation. Used for DataChannel-only peer connections in v1; no audio/video tracks. Picked over C++ bindings (libdatachannel) to avoid a build-time C++ toolchain. |
| `thiserror` | 1.x | Error enums in the Rust core. |
| `blake3` | 1.x | Fast streaming hash for full-file verification and incremental progress reporting. |
| `sha2` | 0.10+ | SHA-256 for chunk integrity (cross-tool verifiable, mandated by manifest). |
| `uuid` | 1.x | v4 for client-side IDs (download ids, chunk request ids, room invite codes via a separate v7 namespace if needed). |
| `tracing` + `tracing-subscriber` | latest | Structured logging; one sink to stderr (captured by Tauri's log plugin) and a rolling file under the app data dir. |
| `ed25519-dalek` | 2.x | Verify host signatures on manifests. |
| `subparse` | 0.7+ | Parse SRT, ASS/SSA, and WebVTT for duration/language detection. No re-encoding. |
| `ffmpeg` (sidecar) | 6.x (optional) | Used **only for probing** a file (`-show_format -show_streams -of json`) to fill in `duration_ms`, `width`, `height`, `video_codec`, `audio_codec`, `container`. **Not** used to decode or transcode. The probe binary is downloaded at first run, not bundled. |

### Frontend

| Choice | Version | Justification |
|---|---|---|
| React | 18.x | Required by the spec. Concurrent rendering, Suspense, and the ecosystem we already know. |
| TypeScript | 5.x strict | Required by the spec. Catches the boring bugs around manifest and IPC payload shapes. |
| Vite | 5.x | Tauri 2's default and the fastest dev loop we can get. HMR is reliable. |
| TanStack Query | 5.x | Server-state cache for room state, manifest fetches, and participant lists. Pairs well with our event bus; invalidations are explicit. |
| **Zustand** (chosen) | 4.x | Lightweight, no boilerplate, no provider tree, plays nicely with Tauri events. Redux Toolkit would buy us normalized state and time-travel debugging, but our domain is small and most state is server-owned; Zustand is the right size. |
| TailwindCSS | 3.x | Utility-first; works well with shadcn/ui copy-paste components. |
| shadcn/ui (built on Radix) | latest | Accessible primitives (Dialog, Popover, Slider) plus unstyled components we own. No versioned library to chase. |
| `react-router` | 6.x | Three screens: Library, Room, Settings. |
| `react-hook-form` + `zod` | latest | Forms (library path, room settings, invites) with shared schemas on the Rust side via `zod`-derived types. |

### Storage and media

| Choice | Version | Justification |
|---|---|---|
| SQLite | 3.45+ (system) | Single-file embedded DB. WAL mode for concurrent reader/writer, FTS5 for library search. |
| Browser `<video>` element | n/a | Plays the local file directly. No MSE, no WebCodecs, no custom decoder. |
| Custom Tauri protocol | n/a | `locast://media/<sha256-prefix>/<filename>` resolves to a file inside the scoped library root. Replaces `file://`, which the webview blocks. |

### What is explicitly NOT in the stack

- No Electron, no Node runtime in the shipped app.
- No `ffmpeg` for decode or transcode; only for probing. The `<video>` element does the playback.
- No Media Source Extensions; we serve the whole file, not a streamable container.
- No external CDN, no cloud storage, no remote auth provider. Signaling is the only network dependency.

## 3. High-Level Architecture

```
+---------------------------------------------------------------------------+
|                              DESKTOP CLIENT (per user)                    |
|                                                                           |
|  +------------------+        +----------------------+                      |
|  |  Webview (React) |  <---> |   Rust Core (Tauri)  |                      |
|  |  - UI screens    |  IPC   |   - domain logic     |                      |
|  |  - <video> tag   |        |   - download state   |                      |
|  |  - drawing/laser |        |   - transfer engine  |                      |
|  +------------------+        +----+-----------+-----+                      |
|          |                         |           |                            |
|          | asset:// / locast://    | SQLite    | WebRTC DataChannel        |
|          v                         v           v                            |
|  +------------------+        +-----------+   (peer media)                 |
|  |  Custom protocol |        |  SQLite   |                                 |
|  |  -> library root |        |  WAL DB   |                                 |
|  +--------+---------+        +-----------+                                 |
|           |                          ^                                     |
|           v                          |                                     |
|  +------------------------------+    |                                     |
|  |      Local Filesystem        |    |                                     |
|  |  /library/<sha>/<file>       |    |                                     |
|  |  /tmp/incomplete/<id>/       |    |                                     |
|  |  /tmp/staging/               |    |                                     |
|  +------------------------------+    |                                     |
+--------------------------------------|-------------------------------------+
                                       |
              WebSocket (signaling)    |     WebRTC DataChannel (file
              + playback commands      |      transfer ONLY)
                                       |
                              +--------v---------+
                              |                  |
                              | SIGNALING/RELAY  |  <-- trust boundary
                              |     SERVER       |
                              |                  |
                              +--------+---------+
                                       |
                                       |  (same as above, mirrored)
                                       v
                          +-----------------------------+
                          |      OTHER CLIENT(S)        |
                          |   (same diagram repeated)   |
                          +-----------------------------+
```

### Trust boundaries

- **Webview to Rust.** Tauri 2 IPC. Only commands registered in `tauri::Builder::invoke_handler` are callable; only events explicitly emitted reach the webview. Capability list in section 5.
- **Webview to filesystem.** Mediated by the custom `locast://` protocol (section 5). The webview can never open a raw `file://` URL or read a directory listing.
- **Client to signaling server.** TLS (wss://). The server is trusted for *delivery* of messages but not for *content*: every manifest is host-signed; every chunk is content-hashed. A hostile server cannot forge media.
- **Client to peer (WebRTC).** DTLS over UDP, with server-relayed SDP/ICE. Peers are authenticated by the host's signature on the manifest (which lists peer public keys). DataChannel messages carry only complete-file transfer traffic; they are application-framed and length-prefixed.
- **Process boundary.** Tauri 2 main process and webview are separate OS processes; a webview compromise cannot directly read the SQLite database or files outside the scoped library root without going through Rust commands.

## 4. Component Architecture

### UI layer (React, `src/`)

- `screens/Library.tsx` - browse, search (FTS5-backed via TanStack Query), import, delete, inspect.
- `screens/Room.tsx` - player + participant strip + drawing/laser overlay + chat sidebar.
- `screens/Settings.tsx` - library root, disk quota, signaling URL, identity.
- `components/Player.tsx` - `<video>` + `<track>` for subtitles; subscribes to playback commands.
- `components/DrawingLayer.tsx` - SVG overlay; receives server-relayed drawing events over the WebSocket.
- `components/ChatPanel.tsx`, `components/LaserPointer.tsx`, `components/ParticipantTile.tsx`.
- `components/TransferList.tsx` - per-file progress, sources, pause/resume.

### Frontend services (`src/services/`)

- `ipc.ts` - typed wrappers over `invoke()`; one function per Rust command.
- `events.ts` - typed `listen<T>()` helpers; one channel per event family.
- `roomClient.ts` - orchestrator: connects to signaling, sends/recv room events, fans out to UI.
- `downloadClient.ts` - subscribes to a media's source list, opens transfer sessions, surfaces progress.
- `identity.ts` - local Ed25519 keypair (stored in OS keychain via `keyring` crate), display name.
- `mediaCatalog.ts` - TanStack Query hooks over `ipc.listMedia`, `ipc.searchMedia`, etc.
- `protocol.ts` - turns `locast://` URLs into `<video src>`; nothing else.

### IPC bridge (Tauri 2)

**Commands** (frontend to Rust, request/response):

- `library_set_root(path)`, `library_get_root()`, `library_scan()`
- `media_list({query, limit, offset})`, `media_get(id)`, `media_import(paths[])`
- `media_resolve_url(id) -> "locast://media/<sha-prefix>/<filename>"`
- `room_create({manifest, settings}) -> {code, roomId}`, `room_join(code)`, `room_leave()`
- `room_get_state()`, `room_get_events(since_seq)`
- `download_start(mediaId)`, `download_pause(id)`, `download_resume(id)`, `download_cancel(id)`
- `download_list()`, `download_get(id)`
- `settings_get(key)`, `settings_set(key, value)`
- `identity_get()`, `identity_rotate()`

**Events** (Rust to frontend, push):

- `room://state` - full room state diff.
- `room://event` - single room_event row.
- `room://presence` - participant list delta.
- `download://progress` - coalesced at <=5 Hz per download.
- `download://state` - state machine transitions.
- `media://added`, `media://removed`, `media://probed`.
- `system://error`, `system://toast`.

### Rust core (`src-tauri/src/`)

- `app.rs` - Tauri builder, command registry, plugin set.
- `domain/` - pure types and rules (no I/O): `MediaItem`, `Room`, `Manifest`, `Download`, `RoomEvent`.
- `library/` - filesystem operations, sanitization, atomic rename, quota.
- `storage/` - sqlx repository layer, migrations, PRAGMA setup.
- `probe/` - optional ffmpeg sidecar wrapper.
- `identity/` - keypair loading, signing helpers.
- `net/signaling.rs` - WebSocket client, reconnect, backoff.
- `net/webrtc.rs` - peer connection lifecycle, ICE, DataChannel for file transfer only.
- `transfer/` - chunk scheduler, sliding window, per-chunk verify, bitmap persistence.
- `room/` - room state machine, event log, command dispatch.
- `sync/` - playback clock, drift correction.
- `telemetry/` - tracing setup, error reporting hooks.
- `ipc/` - `#[tauri::command]` adapters; thin, no logic.

### Storage layer

- **SQLite** - see section 7 for full DDL.
- **Filesystem** - see section 6 for layout and rules.

### Network layer

- **Signaling** - single WebSocket connection per app, multiplexed rooms. Reconnect with exponential backoff (1s to 30s, jitter +/-20%).
- **WebRTC** - one PeerConnection per remote participant per room. One DataChannel per peer, used ONLY for complete-file transfer during the download phase:
  - `files` (reliable, ordered) - complete-file transfer; per-download sub-stream identified by message prefix. No traffic flows over this DataChannel after a file is fully downloaded.
  Playback commands, room events, chat, drawing, laser, presence, and permissions are all server-relayed over the WebSocket and never traverse a DataChannel.

### Media engine

The browser's `<video>` element. `src` is a `locast://` URL resolved through the custom protocol. Subtitles are attached via `<track src="locast://subtitles/<id>/<file>">`. No JS-driven decode.

## 5. Client Architecture (Tauri 2 specifics)

### Process model

- **Main process** (Rust binary). Owns the tokio runtime, SQLite, filesystem, networking, and the IPC command registry. The "source of truth" for application state.
- **Webview process** (system WebView). Runs the React app. Cannot open `file://` URLs, cannot read the filesystem, cannot make raw network requests. All privileged operations go through `invoke()`.
- **One window** in v1. A second window (pop-out player) is possible later via the `tauri-plugin-window-state` and `tauri::WebviewWindowBuilder` APIs but is not in v1.

### Required Tauri 2 capabilities and their scope

Configured in `src-tauri/capabilities/default.json`. Each capability is scoped to the main window only.

| Capability | Scope | Why |
|---|---|---|
| `core:default` | window | `invoke`, `event`, basic window controls. |
| `core:window:allow-close`, `allow-minimize`, `allow-maximize`, `allow-set-title` | window | Standard window ops. |
| `core:webview:allow-set-webview-focus` | window | Needed to refocus after dialogs. |
| `core:event:default` | global | Rust to JS events. |
| `core:path:default` | app | Resolve `$APPDATA`, `$LOCALDATA`, etc. |
| `core:app:default` | app | App metadata. |
| `dialog:default` (allow-open, allow-message) | app | File picker for library root and file import. |
| `notification:default` (allow-notify, allow-is-permission-granted, allow-request-permission) | app | "Download complete" and "Room starting" toasts. |
| `fs:default` (with explicit scope) | scoped to library root | Read access to user-chosen library folder only. **Never** `fs:scope-home-recursive` or anything broader. |
| `fs:allow-read-text-file`, `fs:allow-read-file` | scoped to library root | Subtitle and sidecar reads. |
| Custom protocol `locast://` (see below) | n/a | Replaces the need to expose `fs:` broadly. |
| `opener:default` (allow-open-path) | scoped to `https://*` and `http://*` | "Open in browser" links only. Never arbitrary local paths. |
| `log:default` | app | Tauri log plugin. |
| `os:default` | app | OS version, locale. |
| `process:default` | app | Restart on update. |

Explicitly **not** granted: `shell:execute`, `http:default` (no arbitrary HTTP from the webview; all HTTP goes through Rust), `fs:allow-write-file` outside the temporary directories managed by Rust, any `*:*` capability.

### Command and event surface

Discipline rules:

- Every `#[tauri::command]` takes typed inputs and returns `Result<T, AppError>`. Errors implement `serde::Serialize` so they round-trip to the UI.
- Every long-lived push channel is an `emit("namespace://event", payload)`. Payloads are versioned (`{ v: 1, ... }`) so we can change shapes later.
- No "command that returns a stream." Long-lived flows are push-only; the UI subscribes.

### Custom protocol for serving local media to `<video>`

> **Clarification:** The local file protocol (locast://) is LOCAL file serving by the desktop application to its own webview. It is not internet media streaming. The bytes never leave the machine. The protocol exists only because browsers block `file://` for media. Do not confuse this with a streaming service. All bytes served by `locast://` are read from the local filesystem and never traverse the network.

**Why we need it.** Tauri 2's webview blocks `file://` URLs in `<video src>`, `<img src>`, and `fetch()`. Even when a file is on the same machine, the webview will refuse to load it. We must route the request through a Tauri-controlled protocol so we can:

1. Enforce that the requested path lies under the user's library root (no path traversal).
2. Set `Content-Type` correctly from the file extension.
3. Support HTTP `Range` requests so the browser can seek without downloading the whole file.
4. Set `Content-Length`, `Accept-Ranges: bytes`, and `Cache-Control: no-store`.

**Protocol scheme.** `locast://`

**URL shapes.**

- Media: `locast://media/<sha256-hex-prefix[0..16]>/<filename>`
- Subtitle: `locast://subtitles/<sub-id>/<filename>`
- Sidecar metadata: `locast://meta/<media-id>/locast.json` (optional; mostly for debugging)

**Implementation.** Registered via `tauri::Builder::register_asynchronous_uri_scheme_protocol("locast", handler)`. The handler:

1. Parses the URL; rejects anything that is not `locast://media/...` or `locast://subtitles/...`.
2. Resolves the requested file by looking up `media_items` (or `media_subtitles`) by `id` (or by `sha256` prefix) and reading its `relative_path`.
3. Canonicalizes the resulting absolute path and verifies that it starts with the library root (after canonicalization, to defeat `..` and symlinks). Returns 403 otherwise.
4. Handles HTTP `Range` (single range only for v1). Returns 206 with the byte range.
5. Sets `Content-Type` from a static extension map (`.mp4` -> `video/mp4`, `.mkv` -> `video/x-matroska`, `.webm` -> `video/webm`, `.srt` -> `application/x-subrip`, `.vtt` -> `text/vtt`, `.ass`/`.ssa` -> `text/x-ssa`).
6. Streams the file with a 1 MiB buffer; closes the response on client disconnect.

**Scope and security.**

- The protocol handler has no notion of "outside the library." Even if the webview tries to construct a `locast://media/../../etc/passwd` URL, the SQL lookup will fail and the request is rejected.
- The webview never receives the on-disk path; it only ever sees the `locast://` URL.
- The protocol is not registered with the OS; only the in-process webview can resolve it.

**Why not asset://?** Tauri 2's built-in `asset://` protocol is for files bundled inside the app. Our media is user-supplied and lives outside the bundle, so we need a custom scheme scoped to a runtime-chosen directory.

## 6. Native Filesystem / Storage Architecture

### Directory layout

All paths are relative to the user-chosen **library root** (set on first launch, changeable in Settings). `<sha>` is the full lowercase hex SHA-256 of the file. `<id>` is a v4 UUID.

```
<library_root>/
  index.sqlite                    # main DB (WAL mode; WAL/SHM live alongside)
  index.sqlite-wal
  index.sqlite-shm
  trash/                          # soft-deletes staged for OS trash handoff
  library/
    <sha[0..2]>/<sha[2..4]>/<sha>/<filename>       # content-addressed permanent storage
  tmp/
    incomplete/<download-id>/
      <download-id>.part.<chunk-index>            # in-flight chunks, sparse
      manifest.json                                # copy of the manifest slice for this file
    staging/<download-id>/
      <sha>.partial                               # concatenated, verified, awaiting rename
```

**Why two-level prefix.** A flat `<sha>/<file>` is fine for thousands of files but starts to hurt listings past ~100k. `<sha[0..2]>/<sha[2..4]>/<sha>/<file>` keeps every directory under a few thousand entries and matches what `git`, `ipfs`, and most content stores do.

**Why content-addressed.** Renaming, deduplication, and "do I already have this file?" become O(1) lookups. Two users importing the same file share storage.

### Sidecar metadata

A file at `library/<...>/<sha>/<filename>` may have a sibling `<filename>.locast.json`:

```json
{
  "schema": 1,
  "media_id": "uuid",
  "sha256": "...",
  "blake3": "...",
  "size_bytes": 1234567890,
  "imported_at": "2026-08-26T01:00:00Z",
  "source": "host:room-ABCD",
  "probe": {
    "container": "matroska",
    "duration_ms": 5400123,
    "width": 1920,
    "height": 1080,
    "video_codec": "h264",
    "audio_codec": "aac"
  }
}
```

The sidecar is for recovery: if the SQLite DB is lost or corrupted, we can rebuild `media_items` by walking the library tree and reading each sidecar (or, failing that, re-hashing the file). The sidecar is **never** trusted over the DB; the DB is authoritative when present.

### Atomic completion

Downloads write chunks under `tmp/incomplete/<download-id>/`, then concatenate-and-verify into `tmp/staging/<download-id>/<sha>.partial`. On full verification, the final file is moved into place with `std::fs::rename` (atomic on the same filesystem on Windows since Rust 1.65 with `MoveFileExW` semantics, and on POSIX by definition). If the rename fails, the partial file is left in staging for the next startup to clean up.

### Disk-quota enforcement

- **Settings key.** `library.quota_bytes` (default 50 GiB).
- **Counted size.** The configured storage cap includes ALL of the following: (a) permanent media, (b) temporary media, (c) active and paused download staging files (i.e. bytes under `tmp/incomplete/<download-id>/` and `tmp/staging/<download-id>/`, plus any `.partial` files). The total occupied bytes is `SUM(size_bytes)` for all `media_items` rows (regardless of `status`) plus the on-disk size of in-flight `.partial` files for any active or paused download. Nothing is exempt; temporary and in-flight bytes count fully.
- **Recompute.** On startup, on every `media_import` success, on every download completion, and on a 60-second background timer.
- **Refusal.** The application MUST refuse to start a new download if it would push total occupied bytes above the cap. The cap is checked atomically before each chunk fetch begins; a download that would exceed the cap is paused with a clear UI error. A new `download_start` whose target is not already in the library is rejected with `AppError::QuotaExceeded { used, cap, needed }` if `used + needed > cap * 1.0` (no over-commit). A new `media_import` is rejected the same way.
- **Adjustable.** The user can raise the cap in Settings. The check runs again on the next event; we do not preemptively abort an in-flight transfer unless the user explicitly lowers the cap below current usage.
- **Per-room cap.** Settings key `transfer.per_room_outbound_bytes_per_sec` (default 0 = unlimited) so a host can throttle their own uploads.

### Trash vs immediate delete

- `media_delete(id, mode)`:
  - `mode = "trash"` (default on Windows/macOS): move the file to `<library_root>/trash/`, then delete the row. On Windows, prefer `IFileOperation::MoveItems` (via the `windows` crate) to send it to the system Recycle Bin if the library root is on a volume that supports it; otherwise fall back to the in-library trash dir.
  - `mode = "permanent"`: `std::fs::remove_file` (best-effort) and delete the row.
- The in-library `trash/` directory is purged on startup if older than 30 days. A settings toggle controls retention.

### Filename sanitization rules

All filenames in `media_items.filename` and `media_subtitles.filename` are sanitized at import:

1. **Strip path components.** Split on `\` and `/`; keep the last segment. Reject if empty.
2. **Reject control characters.** Any code point `< 0x20` or in `0x7F..=0x9F` is replaced with `_`.
3. **Reject reserved Windows names (case-insensitive).** `CON`, `PRN`, `AUX`, `NUL`, `COM1`-`COM9`, `LPT1`-`LPT9`. Both the bare name and `name.ext` form are rejected.
4. **Reject trailing dots and spaces.** Windows silently strips these; we strip them explicitly and reject if the result is empty.
5. **Reject Windows-forbidden characters.** `< > : " | ? *` replaced with `_`.
6. **Length cap.** 255 bytes in UTF-8 (the Windows MAX_PATH component limit). Truncate by characters, not bytes, preserving UTF-8 validity.
7. **Case-insensitive collision check.** Before insert, look up by `(LOWER(relative_path))`. SQLite expression index `CREATE UNIQUE INDEX ux_media_path_ci ON media_items(relative_path COLLATE NOCASE)`. Subtitle filenames collide only within the same `media_id`.
8. **NFC normalization.** `unicode-normalization` crate, NFC form, applied before storage and before any on-disk path is computed.

The `relative_path` stored in the DB is the **sanitized** name. The on-disk path under the library root is exactly `<library>/<sha[0..2]>/<sha[2..4]>/<sha>/<sanitized_filename>`.

### Hashing strategy

- **BLAKE3** for full-file integrity: streaming, ~1 GiB/s on a modern CPU, used for the final verification step and for incremental progress reporting (hash tree not needed in v1; we recompute on completion).
- **SHA-256** for per-chunk integrity: mandated by the manifest spec so external tools can verify chunks independently. Also used as the content-address key for storage.
- **Cross-check.** A successful download requires both: every chunk's SHA-256 matches, **and** the recomputed BLAKE3 of the concatenated file matches `manifest.media[].blake3`. Either failure fails the download.

## 7. Local SQLite Schema

Database file: `<library_root>/index.sqlite`. Single writer (the Rust core) under a `tokio::sync::Mutex`; readers use a separate `sqlx::SqlitePool` with `max_connections = 8`. PRAGMAs are set on every new connection by `sqlx::SqliteConnectOptions`.

### PRAGMAs (set on connection)

```sql
PRAGMA journal_mode = WAL;          -- concurrent readers + one writer
PRAGMA synchronous = NORMAL;        -- WAL-safe; full=NORMAL is the standard tradeoff
PRAGMA foreign_keys = ON;           -- required for FK enforcement
PRAGMA busy_timeout = 5000;         -- 5s wait on locked writers
PRAGMA temp_store = MEMORY;         -- temp tables/indices in RAM
PRAGMA mmap_size = 268435456;       -- 256 MiB mmap upper bound (best effort)
PRAGMA cache_size = -64000;         -- ~64 MiB page cache
```

FTS5 is enabled for `media_items` to support fast library search; see the virtual table at the end.

### Migrations

Stored in `src-tauri/migrations/` and applied by `sqlx::migrate!()` at startup. The schema below is migration `0001_init.sql`.

### Tables

```sql
-- media_items: every file we know about.
CREATE TABLE media_items (
    id              TEXT PRIMARY KEY,                              -- uuid v4
    sha256          TEXT NOT NULL UNIQUE,                          -- 64 hex chars
    blake3          TEXT NOT NULL,                                 -- 64 hex chars
    size_bytes      INTEGER NOT NULL CHECK (size_bytes >= 0),
    filename        TEXT NOT NULL,                                 -- sanitized
    relative_path   TEXT NOT NULL UNIQUE COLLATE NOCASE,           -- library-relative
    mime            TEXT NOT NULL,                                 -- e.g. video/mp4
    duration_ms     INTEGER CHECK (duration_ms IS NULL OR duration_ms >= 0),
    width           INTEGER CHECK (width  IS NULL OR width  > 0),
    height          INTEGER CHECK (height IS NULL OR height > 0),
    video_codec     TEXT,                                          -- h264, hevc, av1, vp9, ...
    audio_codec     TEXT,                                          -- aac, opus, ...
    container       TEXT,                                          -- mp4, matroska, webm
    status          TEXT NOT NULL CHECK (status IN ('permanent','temporary')),
    created_at      INTEGER NOT NULL,                              -- unix ms
    last_seen_at    INTEGER NOT NULL,
    last_room_id    TEXT REFERENCES rooms(id) ON DELETE SET NULL,
    source_url      TEXT,                                          -- optional, for provenance
    provenance      TEXT NOT NULL DEFAULT '{}'                     -- JSON
);
CREATE INDEX ix_media_status        ON media_items(status);
CREATE INDEX ix_media_last_seen     ON media_items(last_seen_at DESC);
CREATE INDEX ix_media_last_room     ON media_items(last_room_id);
CREATE INDEX ix_media_size          ON media_items(size_bytes);

-- media_subtitles: sidecar subtitle tracks linked to a media item.
CREATE TABLE media_subtitles (
    id              TEXT PRIMARY KEY,
    media_id        TEXT NOT NULL REFERENCES media_items(id) ON DELETE CASCADE,
    language        TEXT,                                          -- BCP-47, best-effort
    label           TEXT NOT NULL,                                 -- user-visible
    filename        TEXT NOT NULL,                                 -- sanitized
    relative_path   TEXT NOT NULL,                                 -- under media dir
    sha256          TEXT NOT NULL,
    size_bytes      INTEGER NOT NULL CHECK (size_bytes >= 0),
    codec           TEXT NOT NULL CHECK (codec IN ('srt','ass','ssa','vtt','webvtt')),
    UNIQUE (media_id, filename COLLATE NOCASE)
);
CREATE INDEX ix_subtitles_media ON media_subtitles(media_id);

-- rooms: a watch-together session.
CREATE TABLE rooms (
    id              TEXT PRIMARY KEY,
    code            TEXT NOT NULL UNIQUE,                          -- 6-char invite code
    host_user_id    TEXT NOT NULL REFERENCES user_identities(id) ON DELETE RESTRICT,
    created_at      INTEGER NOT NULL,
    ended_at        INTEGER,
    state           TEXT NOT NULL CHECK (state IN ('open','playing','paused','ended','cancelled')),
    manifest_id     TEXT REFERENCES room_manifests(id) ON DELETE SET NULL,
    settings        TEXT NOT NULL DEFAULT '{}'                     -- JSON
);
CREATE INDEX ix_rooms_state      ON rooms(state);
CREATE INDEX ix_rooms_host       ON rooms(host_user_id);
CREATE INDEX ix_rooms_created    ON rooms(created_at DESC);

-- room_manifests: signed manifest snapshots. Immutable; new manifest = new row.
CREATE TABLE room_manifests (
    id              TEXT PRIMARY KEY,
    room_id         TEXT NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
    created_at      INTEGER NOT NULL,
    media           TEXT NOT NULL,                                 -- JSON array
    subtitles       TEXT NOT NULL DEFAULT '[]',                    -- JSON array
    version         INTEGER NOT NULL,
    UNIQUE (room_id, version)
);
CREATE INDEX ix_manifests_room ON room_manifests(room_id, version DESC);

-- room_participants: who has joined which room and in what role.
CREATE TABLE room_participants (
    id                TEXT PRIMARY KEY,
    room_id           TEXT NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
    user_id           TEXT NOT NULL REFERENCES user_identities(id) ON DELETE RESTRICT,
    display_name      TEXT NOT NULL,
    role              TEXT NOT NULL CHECK (role IN ('host','cohost','guest')),
    joined_at         INTEGER NOT NULL,
    left_at           INTEGER,
    connection_state  TEXT NOT NULL CHECK (connection_state IN
                          ('connecting','connected','reconnecting','disconnected','left')),
    capabilities      TEXT NOT NULL DEFAULT '{}',                  -- JSON
    UNIQUE (room_id, user_id)
);
CREATE INDEX ix_participants_room     ON room_participants(room_id);
CREATE INDEX ix_participants_user     ON room_participants(user_id);
CREATE INDEX ix_participants_state    ON room_participants(connection_state);

-- downloads: one row per file being fetched into the library.
CREATE TABLE downloads (
    id                TEXT PRIMARY KEY,
    media_id          TEXT NOT NULL REFERENCES media_items(id) ON DELETE CASCADE,
    room_id           TEXT REFERENCES rooms(id) ON DELETE SET NULL,
    user_id           TEXT NOT NULL REFERENCES user_identities(id) ON DELETE RESTRICT,
    state             TEXT NOT NULL CHECK (state IN
                          ('pending','connecting','transferring','verifying',
                           'complete','failed','paused','cancelled')),
    total_bytes       INTEGER NOT NULL CHECK (total_bytes >= 0),
    transferred_bytes INTEGER NOT NULL DEFAULT 0 CHECK (transferred_bytes >= 0),
    started_at        INTEGER,
    completed_at      INTEGER,
    last_error        TEXT,
    source_peer_id    TEXT,                                         -- primary; multi-source uses bitmap
    chunk_size_bytes  INTEGER NOT NULL DEFAULT 262144               -- 256 KiB
);
CREATE INDEX ix_downloads_state     ON downloads(state);
CREATE INDEX ix_downloads_media     ON downloads(media_id);
CREATE INDEX ix_downloads_room      ON downloads(room_id);

-- download_chunks: per-chunk bookkeeping. The union of state=verified/recv'd
-- chunks is the source of truth for resumability.
CREATE TABLE download_chunks (
    id          TEXT PRIMARY KEY,
    download_id TEXT NOT NULL REFERENCES downloads(id) ON DELETE CASCADE,
    index       INTEGER NOT NULL CHECK (index >= 0),
    offset      INTEGER NOT NULL CHECK (offset >= 0),
    length      INTEGER NOT NULL CHECK (length > 0),
    sha256      TEXT NOT NULL,
    state       TEXT NOT NULL CHECK (state IN
                    ('pending','in_flight','received','verified','failed')),
    UNIQUE (download_id, index)
);
CREATE INDEX ix_chunks_download_state ON download_chunks(download_id, state);
-- Fast "give me the next pending chunk" query:
CREATE INDEX ix_chunks_pending        ON download_chunks(download_id, index)
    WHERE state = 'pending';

-- room_events: append-only log. (room_id, seq) is strictly monotonic.
CREATE TABLE room_events (
    id          TEXT PRIMARY KEY,
    room_id     TEXT NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
    seq         INTEGER NOT NULL,
    sender_id   TEXT REFERENCES user_identities(id) ON DELETE SET NULL,
    type        TEXT NOT NULL,                                     -- play|pause|seek|chat|draw|...
    payload     TEXT NOT NULL,                                     -- JSON
    created_at  INTEGER NOT NULL,                                  -- client clock
    server_ts   INTEGER,                                           -- server clock; NULL if offline
    UNIQUE (room_id, seq)
);
CREATE INDEX ix_events_room_seq ON room_events(room_id, seq);

-- presence: a thin "who is currently connected" table, used for the
-- participant strip without scanning room_events.
CREATE TABLE presence (
    id                TEXT PRIMARY KEY,
    room_id           TEXT NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
    user_id           TEXT NOT NULL REFERENCES user_identities(id) ON DELETE CASCADE,
    last_seen         INTEGER NOT NULL,
    connection_state  TEXT NOT NULL CHECK (connection_state IN
                          ('online','away','reconnecting','offline')),
    UNIQUE (room_id, user_id)
);
CREATE INDEX ix_presence_room ON presence(room_id);
CREATE INDEX ix_presence_user ON presence(user_id);

-- user_identities: every Locast user we've ever met. Display name is local-only;
-- the public key is the stable identifier.
CREATE TABLE user_identities (
    id           TEXT PRIMARY KEY,                                 -- sha256(public_key) hex
    public_key   TEXT NOT NULL UNIQUE,                             -- base64 ed25519 public key
    display_name TEXT NOT NULL,
    created_at   INTEGER NOT NULL,
    last_seen    INTEGER NOT NULL
);
CREATE INDEX ix_identities_last_seen ON user_identities(last_seen DESC);

-- room_invites: pre-generated invite codes, with optional expiry and max uses.
CREATE TABLE room_invites (
    id          TEXT PRIMARY KEY,
    room_id     TEXT NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
    code        TEXT NOT NULL UNIQUE,
    created_by  TEXT NOT NULL REFERENCES user_identities(id) ON DELETE RESTRICT,
    expires_at  INTEGER,                                           -- NULL = never
    used_at     INTEGER,                                           -- first use; NULL if unused
    max_uses    INTEGER NOT NULL DEFAULT 1 CHECK (max_uses > 0)
);
CREATE INDEX ix_invites_room ON room_invites(room_id);
CREATE INDEX ix_invites_code ON room_invites(code);

-- settings: typed key/value bag. Values are JSON; keys are dotted namespaces.
CREATE TABLE settings (
    key    TEXT PRIMARY KEY,
    value  TEXT NOT NULL                                           -- JSON
);
```

### FTS5 virtual table for library search

```sql
CREATE VIRTUAL TABLE media_items_fts USING fts5(
    filename,
    display_label,                                                 -- user-set label
    tokenize = 'unicode61 remove_diacritics 2',
    content = 'media_items',
    content_rowid = 'rowid'
);

-- Triggers to keep FTS in sync.
CREATE TRIGGER media_items_ai AFTER INSERT ON media_items BEGIN
    INSERT INTO media_items_fts(rowid, filename, display_label)
    VALUES (new.rowid, new.filename, COALESCE(json_extract(new.provenance, '$.label'), ''));
END;
CREATE TRIGGER media_items_ad AFTER DELETE ON media_items BEGIN
    INSERT INTO media_items_fts(media_items_fts, rowid, filename, display_label)
    VALUES ('delete', old.rowid, old.filename, '');
END;
CREATE TRIGGER media_items_au AFTER UPDATE ON media_items BEGIN
    INSERT INTO media_items_fts(media_items_fts, rowid, filename, display_label)
    VALUES ('delete', old.rowid, old.filename, '');
    INSERT INTO media_items_fts(rowid, filename, display_label)
    VALUES (new.rowid, new.filename, COALESCE(json_extract(new.provenance, '$.label'), ''));
END;
```

Search query: `SELECT m.* FROM media_items m JOIN media_items_fts f ON f.rowid = m.rowid WHERE media_items_fts MATCH ? ORDER BY rank LIMIT ? OFFSET ?;`

### Notes on shapes

- All timestamps are `INTEGER` unix milliseconds. Storing as `TEXT` ISO-8601 is more readable but more expensive to compare; v1 uses integers.
- All JSON columns are validated by Rust before insert; the DB is the dumb container.
- The `seq` column on `room_events` is assigned by the server (or, for offline-created events, by the client with a `server_ts IS NULL` marker); `UNIQUE (room_id, seq)` enforces ordering.
- `provenance` JSON shape (advisory, not enforced): `{ "label": "Movie Night", "imported_from": "host:room-ABCD", "notes": "..." }`.

## 8. Media Manifest Design

The manifest is the contract between a host (who has the files) and viewers (who need to acquire them before joining the room). It is the only piece of room state a viewer needs in order to start downloading, and it is the only piece the host signs.

### Canonical manifest JSON

The host produces exactly one of these per room, then re-issues a new one whenever the set of files changes (e.g. the host adds a movie mid-evening). New manifest = new `room_manifests.version`, same `room_id`.

```json
{
  "manifest_version": 1,
  "room_id": "9f0c1c2e-7a4b-4b1f-9d2a-1c0d8e7f6a5b",
  "media": [
    {
      "id": "5b1f3e7a-2c1d-4f1e-9c0b-1a2b3c4d5e6f",
      "filename": "Spirited Away.mkv",
      "sha256": "9a8b7c6d5e4f3a2b1c0d9e8f7a6b5c4d3e2f1a0b9c8d7e6f5a4b3c2d1e0f9a8b",
      "blake3": "ab1c2d3e4f5061728394a5b6c7d8e9f0a1b2c3d4e5f60718293a4b5c6d7e8f90",
      "size_bytes": 8912894720,
      "mime": "video/x-matroska",
      "duration_ms": 7200023,
      "dimensions": { "width": 1920, "height": 1080 },
      "codecs": { "video": "h264", "audio": "flac", "container": "matroska" },
      "sources": [
        {
          "peer_id": "host-public-key-sha256-hex",
          "url_hint": null,
          "priority": 0,
          "chunk_size": 262144,
          "total_chunks": 33986,
          "chunk_hashes": [
            "9a8b7c6d5e4f3a2b1c0d9e8f7a6b5c4d3e2f1a0b9c8d7e6f5a4b3c2d1e0f9a8b",
            "..."
          ]
        },
        {
          "peer_id": "cohost-public-key-sha256-hex",
          "url_hint": null,
          "priority": 1,
          "chunk_size": 262144,
          "total_chunks": 33986,
          "chunk_hashes": ["..."]
        }
      ]
    }
  ],
  "subtitles": [
    {
      "id": "sub-uuid",
      "language": "en",
      "label": "English (CC)",
      "filename": "Spirited Away.en.srt",
      "sha256": "...",
      "size_bytes": 142336,
      "codec": "srt",
      "sources": [
        { "peer_id": "host-public-key-sha256-hex", "url_hint": null, "chunk_size": 65536, "total_chunks": 3, "chunk_hashes": ["...", "...", "..."] }
      ]
    }
  ],
  "created_at": 1764181764123,
  "host_signature": {
    "public_key": "base64-ed25519-public-key",
    "algorithm": "ed25519",
    "value": "base64-ed25519-signature"
  }
}
```

### Field rules

- `manifest_version`. Integer. Currently `1`. Bumped on any breaking change to the shape.
- `room_id`. UUIDv4 minted by the host when the room is created. Stable for the life of the room.
- `media[].id`. UUIDv4, generated by the host. Used as the primary key in the viewer's `media_items` table on first import.
- `media[].filename`. Sanitized filename (see section 6). The viewer uses this verbatim as the on-disk name.
- `media[].sha256`. Full lowercase hex SHA-256 of the file. Cross-tool verifiable.
- `media[].blake3`. Full lowercase hex BLAKE3 of the file. Used for the fast final verification.
- `media[].size_bytes`. Total file size in bytes. The viewer uses it to preallocate and to compute `total_chunks = ceil(size_bytes / chunk_size)`.
- `media[].mime`. IANA media type. Informational; the viewer infers from extension as a fallback.
- `media[].duration_ms`. Probed duration; the viewer uses it for the seek bar and the "ready to play" check.
- `media[].dimensions`. `width` and `height` in pixels. Optional in the manifest (some files are audio-only); the viewer schema marks it nullable.
- `media[].codecs`. `video`, `audio`, `container`. All optional strings; absent for audio-only.
- `media[].sources[]`. One entry per peer that holds the file. v1 always has at least one source; the host itself is always the first.
  - `peer_id`. The base64 Ed25519 public key of the peer, or its SHA-256 hex prefix (we use the hex prefix to keep URLs short).
  - `url_hint`. v1 always `null`. v2 may carry a magnet-style multi-hash or an HTTP webseed URL.
  - `priority`. `0` = preferred. Lower numbers are tried first when multiple peers are reachable.
  - `chunk_size`. Bytes per chunk. All peers in a room **must** advertise the same `chunk_size` for the same file, so the chunk hash lists line up.
  - `total_chunks`. `ceil(size_bytes / chunk_size)`. The last chunk is `size_bytes - (total_chunks-1) * chunk_size` bytes.
  - `chunk_hashes[]`. SHA-256 of each chunk, in order. Length equals `total_chunks`. Used for per-chunk integrity.
- `subtitles[]`. Same shape as `media[]` minus `duration_ms`/`dimensions`/`codecs`; the `codec` field on the parent is the subtitle codec.
- `created_at`. Unix milliseconds (host clock). Not trusted for ordering; used for display only.
- `host_signature`. Signs the canonicalized form of the manifest **with the `host_signature` field itself zeroed out**.

### Canonicalization

The host and every viewer must compute the signature over the exact same byte sequence. We define a single canonical form:

1. Strip the `host_signature` field entirely (replace with `null`).
2. Serialize with `serde_json` using a custom serializer that:
   - Sorts object keys lexicographically at every depth.
   - Emits no insignificant whitespace (compact form).
   - Uses `\uXXXX` escapes only where required for valid JSON; otherwise emits raw UTF-8.
   - Rejects non-finite floats (`NaN`, `+-Infinity`); integers only.
3. Normalize the input string to UTF-8 NFC before serializing (`unicode-normalization::UnicodeNormalization::nfc`).
4. Append a single trailing newline. This makes the canonical form `cat`-friendly for debugging.

The Rust implementation lives in `src-tauri/src/manifest/canonical.rs` and is exercised by a unit test against a known golden vector.

### Signature

- Algorithm: **Ed25519** (`ed25519-dalek`, `SigningKey::sign`).
- The host's public key is part of the manifest, so verification is self-contained: a viewer who trusts the manifest's `public_key` as the host's identity can verify offline, without out-of-band key exchange.
- Verification flow on the viewer:
  1. Receive manifest.
  2. Recompute canonical bytes.
  3. `ed25519_dalek::VerifyingKey::from_bytes(&public_key).verify(&canonical_bytes, &signature)`.
  4. On failure, refuse to download anything. Surface a `system://error` event: `"Manifest signature invalid; refusing to download from untrusted host"`.
- Out-of-band trust bootstrap: the host's public key is also delivered in the room invite (see below). The viewer stores it as `user_identities.public_key` on first contact, and from then on expects the manifest to be signed by the **same** key. Mismatch = reject.

### How a viewer uses the manifest

1. Receive `manifest` over `ctrl` DataChannel from the host, or via the signaling server as a fallback.
2. Cross-check `host_signature.public_key` against the room's `host_user_id`. If absent, store it.
3. Verify signature.
4. For each `media[]` entry, check if `media_items` already has a row with that `sha256`.
   - If `status = 'permanent'`: skip download.
   - If `status = 'temporary'` or absent: create a `downloads` row in state `pending`.
5. For each pending download, open a transfer session (see section 9).
6. When all chunks for a file are `verified`, recompute the full BLAKE3 over the assembled file and compare to `blake3`. On match, atomic-rename into place and flip `media_items.status` to `permanent`.

### Room invite bootstrap

The room invite is a short URL the host shares out of band:

```
locast://join/<room_code>?h=<base64-host-public-key>&v=1
```

or, if the app isn't installed:

```
https://locast.example/join/<room_code>?h=<base64-host-public-key>&v=1
```

The `h` parameter is the host's Ed25519 public key, base64-url. The viewer's first action on join is to verify that the manifest it later receives is signed by exactly this key. This closes the "signaling-server-swap-attack" gap.

## 9. Download / P2P Architecture

> **Definition:** In this document, P2P refers to media acquisition (file transfer) between peers using WebRTC DataChannels. The authoritative room-control plane is the WebSocket server; that is not P2P.

v1 transfers complete files between Locast clients over **WebRTC DataChannels**. There is no media relay server. The signaling server only sees SDP/ICE exchange and short control messages.

> **v1 Transport Split:** WebSocket via the server is authoritative for room control, sync, drawing, laser, chat, presence, permissions. WebRTC DataChannels are used ONLY for complete-file transfer during the download phase. After download, no DataChannel traffic is required for playback.

| Traffic | Path | Reason |
|---|---|---|
| Room control, sync, drawing, laser, chat, presence, permissions | WebSocket via the server (authoritative) | Single arbiter for permissions, audit, presence consistency; no per-peer permission enforcers |
| Complete-file transfer during download | WebRTC DataChannel (`files`) | Bulk bytes never traverse the server; host-signed manifest + per-chunk hash/sig is the integrity guarantee |
| SDP / ICE signaling | WebSocket via the server (pure relay) | Standard WebRTC bootstrap; server does not inspect |

### Chunk model

- A file is a sequence of `N` chunks, where `N = ceil(size_bytes / chunk_size)`.
- Default `chunk_size = 262144` bytes (256 KiB). Last chunk is `size_bytes - (N-1) * chunk_size` bytes, which may be smaller; the manifest records `length` per chunk via the `chunk_hashes` array length and the `size_bytes` total.
- Allowed `chunk_size` values: `65536` (64 KiB), `262144` (256 KiB), `1048576` (1 MiB). All peers in a room agree on the same `chunk_size` for a given file (enforced by the host when issuing the manifest).
- Each chunk is integrity-checked with **SHA-256** as it arrives. After all chunks are assembled, the **BLAKE3** of the full file is computed and compared to the manifest.

### DataChannel framing

One **reliable, ordered** DataChannel per peer named `media` carries transfer traffic. The wire format is length-prefixed CBOR (or JSON for v1 simplicity; CBOR is the documented v1.1 target):

```
Frame = {
  "t":  "request"  | "data"  | "ack"  | "nak"  | "cancel" | "bitmap" | "hello",
  "d":  <download_id>,
  "i":  <chunk_index>,          // request, data, ack, nak
  "b":  <base64 chunk bytes>,   // data
  "s":  <base64 sha256>         // data, ack, nak (mismatch)
}
```

- Maximum frame size: 256 KiB + 1 KiB overhead. A single chunk never spans frames.
- Messages larger than the SCTP buffered amount (default ~256 KiB) are fragmented by us before send; we use simple 16-bit length prefix + payload chunks and reassemble.

### Sliding window and backpressure

- **In-flight window.** Each download has a sliding window of up to `W = 16` outstanding chunk requests. The viewer sends up to 16 `request` frames; for each `ack` it receives, it sends one more `request` (or stops if all chunks are in flight / done).
- **Per-peer cap.** The host applies a per-peer token bucket: at most `B = 4` chunks in flight to a given viewer peer at any time. The viewer learns this from the SDP/ICE setup; `B` is exchanged in the `hello` frame.
- **SCTP buffer watermarks.** The webrtc-rs DataChannel default buffer is 16 MiB. We lower it to 4 MiB and treat "bufferedAmount > 2 MiB" as a soft backpressure signal: stop sending new `request` frames for that peer until the buffer drains below 1 MiB.
- **Burst limit.** A peer may not send more than `B * chunk_size` bytes to a given viewer in a 250 ms sliding window. We do not implement this in v1; the SCTP buffer watermark is enough.

### Per-download state machine

```
                download_start
                      |
                      v
              +---------------+
              |   pending     |   <- row exists, no peer yet
              +-------+-------+
                      | peer chosen
                      v
              +---------------+
              |  connecting   |   <- PeerConnection / DataChannel opening
              +-------+-------+
                      | data channel open + first chunk request sent
                      v
              +---------------+
              | transferring  |   <- chunk requests in flight, acks arriving
              +-------+-------+
                      | all chunks verified
                      v
              +---------------+
              |   verifying   |   <- BLAKE3 over assembled file
              +-------+-------+
                  /         \
                 /           \  mismatch
                v             v
          +-----------+   +-----------+
          | complete  |   |  failed   |
          +-----------+   +-----------+
                                  ^
                                  | retries exhausted

  Any state can transition to:
    - paused   (user request; DataChannel closed; bitmap persisted)
    - cancelled (user request; row marked; partials deleted)
```

### State transitions in detail

- `pending -> connecting`: a peer is selected from `media[].sources` by priority; the WebRTC PeerConnection (if not already up) begins ICE.
- `connecting -> transferring`: the `media` DataChannel emits `open`; the viewer sends a `hello` frame carrying the download id and the list of `chunk_index`es already in state `received` or `verified` (resumability).
- `transferring -> verifying`: every `download_chunks` row is `verified`.
- `verifying -> complete`: BLAKE3 matches `manifest.media[].blake3`. The file is atomic-renamed into the library. `media_items.status` flipped to `permanent` if it was `temporary`. `downloads.completed_at` set. UI emits `download://state = "complete"`.
- `verifying -> failed`: BLAKE3 mismatch. All chunks marked `pending` again; transfer restarts from `transferring` with the same sources.
- `* -> paused`: user-initiated. The DataChannel is closed gracefully; `downloads.state = 'paused'`. Chunks already `received` or `verified` are kept on disk. The `download_chunks` rows are the source of truth for what we have.
- `paused -> transferring`: user resumes. A fresh `connecting` is taken; the `hello` frame's bitmap skips anything we already have.
- `* -> failed`: a chunk fails verification `MAX_CHUNK_RETRIES = 5` times across peers, or a transport error is unrecoverable.
- `* -> cancelled`: user cancels. Partial files in `tmp/incomplete/<id>/` are deleted.

### Multi-source

A viewer may pull different chunks from different peers. The host always holds a complete copy; co-hosts and other participants may also hold copies and advertise them in `media[].sources`.

- **Selection (v1).** Round-robin across reachable peers in `priority` order. Ties broken by least-recently-used peer.
- **Bitmap merge.** A peer's `hello` returns the chunks it has already verified for this download; we treat them as `received` and skip them in the request loop.
- **Failure fallback.** If a peer NAKs 3 chunks in a row, or its RTT exceeds 2000 ms for 10 s, mark it `unavailable` for this download and rotate to the next source.
- **Rarest-first (v2, not v1).** The viewer would compute the global chunk availability map and request the rarest first. Punt to v2.

### Host upload fairness

The host runs one DataChannel per peer; per peer, a token bucket of size `B = 4` (configurable via `settings.transfer.per_peer_inflight_chunks`, default 4). The bucket refills at `4` tokens per 250 ms. Each `request` consumes one token; each `data` emission waits for a token if none are available.

The host also enforces a per-room outbound cap: `settings.transfer.per_room_outbound_bytes_per_sec` (default 0 = unlimited). Implemented as a global token bucket sized in bytes.

### Pause / resume

- **Pause.** Close the `media` DataChannel gracefully (send `cancel` frames for all in-flight requests, then `close()`). Mark `downloads.state = 'paused'`. The `download_chunks` rows are authoritative for what we have.
- **Resume.** Reopen the channel (potentially with a different peer). The `hello` frame includes the bitmap of `received` + `verified` chunks. The peer (or us, depending on direction) only re-sends the missing ones.
- **Restart after long pause.** If a `paused` download is older than `RESUME_MAX_AGE_HOURS = 72`, the viewer discards `received` chunks (they go back to `pending`) but keeps `verified` chunks. The disk usage of the partials is bounded by `W * chunk_size * num_active_downloads`; for a 1 GiB/s capable host and 16 active downloads, that is ~4 GiB worst case.

### Reconnect

- The WebSocket signaling client uses exponential backoff: 1 s, 2 s, 4 s, 8 s, 16 s, 30 s (cap), with +/-20% jitter.
- A dropped peer triggers a `connecting -> connecting` cycle. The download does not regress to `pending`; the existing `download_chunks` bitmap is preserved.
- If no peer is reachable for `PEER_RECONNECT_TIMEOUT_SECONDS = 60` seconds, the download transitions to `paused` automatically.

### Corruption handling

- On `data` arrival, recompute SHA-256 of the bytes and compare to the expected `chunk_hashes[i]`. On match, persist as `verified` and write the bytes to `tmp/incomplete/<id>/<id>.part.<i>`. On mismatch, send `nak` with the expected hash and re-queue the chunk.
- Per-chunk retry budget: `MAX_CHUNK_RETRIES = 5`. After 5 failed attempts (across any number of peers), the download transitions to `failed`.
- A `failed` download is **not** auto-retried. The user must explicitly press "Retry" in the UI. This avoids burning bandwidth and battery in a background loop.

### Final verification and atomic completion

1. Concatenate `tmp/incomplete/<id>/<id>.part.<i>` for all `i` in order into `tmp/staging/<id>/<sha>.partial`. This step is a single streaming copy in Rust.
2. Compute BLAKE3 over the partial file. Compare to `manifest.media[].blake3`. If mismatch, leave the partial on disk (it will be cleaned up by the next startup's staging purge) and transition to `failed`.
3. On match, `std::fs::rename` the partial to `library/<sha[0..2]>/<sha[2..4]>/<sha>/<filename>`. This is atomic on the same filesystem.
4. Delete `tmp/incomplete/<id>/`. Update SQLite: insert `media_items` (or update if it was `temporary`), mark `download_chunks` all `verified`, mark `downloads.state = 'complete'`, set `completed_at`.
5. Emit `download://state = "complete"` and `media://added` to the UI.

### Multiple simultaneous downloads

- One transfer session per `(download_id)`. The viewer maintains an `Arc<Mutex<HashMap<DownloadId, TransferSession>>>` in the Rust core.
- Multiple sessions may run concurrently in a single room. Default cap: `MAX_CONCURRENT_DOWNLOADS = 4` (configurable in settings). Excess downloads sit in `pending` until a slot frees.
- The total in-flight chunk count across all sessions is also capped: `MAX_INFLIGHT_CHUNKS_TOTAL = 64`. This is the global backpressure knob.
- The peer-level cap (`B = 4`) is enforced per DataChannel, so concurrency at the host is naturally bounded.

### Progress reporting to the UI

- Progress events are coalesced: at most one `download://progress` event per download per **200 ms** (5 Hz).
- Payload: `{ id, transferred_bytes, total_bytes, bytes_per_sec_ema, eta_seconds, state }`. The EMA uses `alpha = 0.3`.
- State transitions (`download://state`) are emitted immediately, not coalesced.
- A room-level `room://bandwidth` event aggregates the EMA across all downloads for the participant strip.

### Numbers and limits summary (v1)

| Knob | Value | Source |
|---|---|---|
| Default chunk size | 256 KiB | `downloads.chunk_size_bytes` default |
| Allowed chunk sizes | 64 KiB, 256 KiB, 1 MiB | manifest validation |
| In-flight window per download | 16 | `WINDOW` constant |
| Per-peer in-flight cap (host) | 4 | `PER_PEER_BUCKET` |
| Global in-flight cap (viewer) | 64 | `MAX_INFLIGHT_CHUNKS_TOTAL` |
| Max concurrent downloads | 4 | settings default |
| Max chunk retries | 5 | `MAX_CHUNK_RETRIES` |
| Peer reconnect timeout | 60 s | `PEER_RECONNECT_TIMEOUT_SECONDS` |
| Resume max age (preserve received) | 72 h | `RESUME_MAX_AGE_HOURS` |
| Progress event rate | 5 Hz | coalesce window |
| Chunk SHA-256 mismatch retry across peers | 5 total | `MAX_CHUNK_RETRIES` |
| DataChannel buffer | 4 MiB | webrtc-rs config |
| DataChannel soft backpressure threshold | 2 MiB | `bufferedAmount` |
| Signaling backoff | 1 s -> 30 s, +/-20% jitter | exponential |
| Heartbeat | 5 s, 3 misses = drop | `ctrl` channel ping |
| RTT "unavailable" threshold | 2000 ms for 10 s | peer health |

### What v1 does NOT do

- No DHT, no tracker, no BitTorrent-style swarm.
- No webseed (HTTP fallback to a server-side seed). All transfers are peer-to-peer.
- No NAT traversal beyond STUN and TURN. Direct P2P is preferred; TURN is REQUIRED as a fallback for cases where direct STUN/ICE fails (symmetric NAT, restrictive firewalls). The server mints short-lived TURN credentials (HMAC, e.g. coturn's `use-auth-secret` model) on a per-session basis. TURN bandwidth costs are an operator concern; the server logs TURN usage for capacity planning. TURN is used only when ICE connectivity checks fail.
- No upload from a viewer to the host (viewers are downloaders; only the host and any co-host with the file upload).
- No resumable upload from the host. If the host's library file changes mid-room, viewers re-download the affected chunks.
- No multi-file atomic commit. Each file is committed independently.
- No differential/incremental updates. A re-download is a full re-download.
- No bandwidth probing or congestion control beyond the sliding window and the host's token bucket. We rely on SCTP's built-in reliability.

### Failure isolation

- A failed download (corruption, peer gone, retry budget exhausted) affects **only** its own `downloads` row and `tmp/incomplete/<id>/` directory. No other download, no other room, no playback of an already-verified file.
- A WebRTC PeerConnection error for one peer does not affect DataChannels to other peers. Each `(peer, download)` pair is independent.
- A SQLite write failure aborts the current operation and emits `system://error`; in-memory state is rolled back via the surrounding transaction. The DB itself is in WAL mode, so a crash mid-write does not corrupt prior data.
- A panic in the transfer engine is caught at the `tokio::spawn` boundary (`std::panic::AssertUnwindSafe` + `catch_unwind`), logged, and the affected download transitions to `failed` with `last_error = "internal: panic"`.

## 10. Room Architecture

### 10.1 Overview

A Locast room is the runtime container for a single synchronized watch session. A room binds together:

- One **host** participant who controls playback
- Zero or more **viewer** participants
- One **media manifest** describing the file(s) to play
- A **transient session** bounded by explicit start (`PLAY`) and end (`ENDED`) events
- A **set of participants**, each with an Ed25519 identity and a capability set

A room is created on demand, lives for the duration of one watch session, and is persisted by both server and clients.

### 10.2 Room Code

**Format.**

- 6 characters
- Alphabet: `[A-Z0-9]` minus ambiguous characters
- Excluded characters: `0`, `O`, `1`, `I`, `L`
- Effective alphabet: 22 letters + 6 digits = 28 characters
- Code space: 28^6 = 481,890,304 (~482M) codes

**Generation.**

- Generated via CSPRNG (`OsRng` in Rust, `crypto.getRandomValues` in TS)
- Exclude the 5 ambiguous characters from the RNG output
- Rejection sampling: roll until 6 valid characters are obtained
- Collisions are possible but rare; on insert, use a `UNIQUE` constraint on `rooms.code` and retry up to 5 times
- If 5 consecutive collisions occur, abort creation with a user-visible error

**Validation.**

- Client input must be normalized (uppercase, whitespace trimmed)
- Codes are case-insensitive in display but stored uppercase
- Reject codes containing excluded characters at the client to avoid round-trips
- Server re-validates on lookup

### 10.3 Room Lifecycle

```
CREATED -> MANIFEST_CONFIRMED -> ACCEPTING_VIEWERS -> PLAYING/PAUSED -> ENDED
```

| State | Description | Entered by |
|---|---|---|
| `CREATED` | Host has generated a code; no manifest yet | Host action |
| `MANIFEST_CONFIRMED` | Manifest has been uploaded and verified by server | Server, after hash check |
| `ACCEPTING_VIEWERS` | Room accepts new viewer joins | Host action or auto, on `MANIFEST_CONFIRMED` |
| `PLAYING` | At least one `PLAY` command has been issued | Host `PLAY` command |
| `PAUSED` | Currently paused; resume returns to `PLAYING` | Host `PAUSE` command |
| `ENDED` | Session is terminated | Host `END` command, timeout, or host-transfer failure |

Terminal state: `ENDED`. Rooms in `ENDED` state are retained for audit/history but reject all new commands.

### 10.4 Room Persistence

**Server (authoritative minimum record).**

Table: `rooms`

| Column | Type | Notes |
|---|---|---|
| `id` | UUID v7 | Primary key |
| `code` | CHAR(6) | Unique, indexed |
| `host_pubkey` | BLOB(32) | Ed25519 public key |
| `manifest_hash` | BLOB(32) | BLAKE3 of manifest JSON |
| `created_at` | INTEGER | Unix ms |
| `ended_at` | INTEGER, NULL | Unix ms, set on `ENDED` |

The server does **not** store media, drawing strokes, laser history, or participant preferences. It stores:

- Per-room participant list with capabilities (table: `room_participants`)
- Command log (table: `room_commands`) for replay/recovery
- Connection/session state for active participants

The server can rebuild state for any room by reading the command log.

**Client (everything else).**

Each client maintains a local SQLite database (`locast.db`) with:

- Full room history (including ended rooms)
- Media manifests
- Downloaded file paths and verification hashes
- All drawing strokes and laser history
- Per-user preferences
- Cached participant info and capability snapshots

This means a client can re-open a room after restart and view its full state, including playback history, even if the server has purged the room.

### 10.5 Room Discovery

- Rooms are discoverable **only by code**
- There is **no** listing, search, browse, or directory endpoint
- Codes are out-of-band shared (e.g., in a chat, DM, voice call)
- The server has no public API to enumerate rooms
- Ended rooms are queryable by the host only via their own client

### 10.6 Identity

**Keypair.**

- Algorithm: Ed25519
- Each client has exactly one long-lived keypair per device
- Generated on first launch, stored in OS keyring
- Used for: server authentication challenge, signed room events, capability assertions
- Private key **never** leaves the device; the keyring API is the only access path

**First-launch flow.**

1. Detect absence of stored keypair
2. Prompt for display name (1-32 chars, no leading/trailing whitespace, no control chars)
3. Generate Ed25519 keypair via `rand::rngs::OsRng`
4. Store private key in OS keyring under service `locast`, account `<user_id>`
5. Store public key + display name locally
6. Register public key with server (one-shot, with a server-issued install token from the desktop bundle)

**Display name.**

- Local-only; not unique
- Can be changed in settings (does not rotate keypair)
- Shown to other participants

**Sessions.**

- A **session** is a short-lived bearer token issued by the server
- Obtained via signed challenge:
  1. Client connects via WebSocket
  2. Server sends `CHALLENGE { nonce, server_ts }`
  3. Client signs the nonce with its Ed25519 private key
  4. Server verifies signature against the registered public key
  5. Server issues `SESSION_TOKEN { token, expires_at }`
- Token TTL: 24 hours, refreshable
- One session per (device, room) at a time
- Server can revoke a session; revocation is immediate and broadcast to all rooms the session is in

## 11. Room State Machine

### 11.1 States

| State | Description |
|---|---|
| PREPARING | Host is building the manifest. Viewers may not join. The host can change the manifest freely. |
| DOWNLOADING | Manifest confirmed; at least one required participant is still downloading files (not READY). |
| READY | All required participants are READY. The host can issue PLAY to transition to PLAYING. |
| PLAYING | The host has issued PLAY. The room is actively playing. |
| PAUSED | The host has issued PAUSE. The room resumes to PLAYING on PLAY. |
| ENDED | Terminal. The room no longer accepts commands. |

A participant's local sub-state (separate from room state) is one of: CONNECTED, RECONNECTING, LEFT.

### 11.2 Transition Table

Rows are current state; columns are event. Cell content is the resulting state, or `â€”` if invalid.

| From \ Event | MANIFEST_PUBLISH | PARTICIPANT_READY (last) | PLAY | PAUSE | SEEK | END | MANIFEST_CHANGE | host disconnect (30s) | viewer disconnect (5m) |
|---|---|---|---|---|---|---|---|---|---|
| PREPARING | DOWNLOADING | â€” | â€” | â€” | â€” | ENDED | PREPARING (allowed) | ENDED | â€” |
| DOWNLOADING | â€” | READY | â€” | â€” | â€” | ENDED | â€” | ENDED | DOWNLOADING (still) |
| READY | â€” | â€” | PLAYING | â€” | â€” | ENDED | â€” | ENDED | READY (if not last) |
| PLAYING | â€” | â€” | â€” (idempotent) | PAUSED | PLAYING (seek) | ENDED | â€” (forbidden) | ENDED | PLAYING |
| PAUSED | â€” | â€” | PLAYING | â€” (idempotent) | PAUSED (seek) | ENDED | â€” (forbidden) | ENDED | PAUSED |
| ENDED | â€” | â€” | â€” | â€” | â€” | â€” | â€” | â€” | â€” |

`â€”` means the event is not applicable in that state; the client ignores it and the server drops it.

### 11.3 Host Disconnect

**Grace period.**

- Duration: **30 seconds** from the last received heartbeat/command from the host
- During the grace period, the room remains in its current state
- All viewers see a "Host reconnecting..." indicator
- Host is marked RECONNECTING; viewers continue to receive the last-known playback state

**After grace period.**

- If the host does not return within 30 seconds, the server ends the room. All viewers are notified and returned to the library. Host transfer is explicitly deferred to v1.1.

### 11.4 Viewer Disconnect

**Mark.**

- Viewer is marked RECONNECTING
- Last known state is retained on the server

**Within 5 minutes.**

- If the viewer reconnects with the same user_id, they are restored to their previous state
- Their READY status is preserved if they were READY
- If a download was in progress, the manifest/version hash is compared; if identical, download is resumed from checkpoint

**After 5 minutes.**

- Viewer is marked LEFT
- Server retains their participant record (for history) but removes them from the active set
- If they were the only non-ready participant, the room may transition from DOWNLOADING to READY
- They cannot rejoin the same room; they must rejoin with a new participant entry
- Their drawing strokes and laser history are retained on their own client but are not redisplayed to others

### 11.5 Permission Changes During PLAYING

- Capability grants/revokes are signed room events
- They take effect immediately on the server
- The next PLAY/PAUSE/SEEK from a now-revoked user is rejected
- In-flight commands already in the log are not retroactively revoked
- A REVOKE against the host is treated as an implicit request to end the room; the server transitions the room to ENDED.
- Drawing permission changes affect the next stroke; in-progress strokes are not aborted

### 11.6 Manifest Changes

- Manifest changes (add file, remove file, change version) are **only allowed in PREPARING**
- A MANIFEST_PUBLISH in any other state is rejected
- Changing the manifest after the room has reached READY requires the host to first transition the room back to PREPARING (which forces all viewers back to DOWNLOADING)
- In PLAYING or PAUSED, manifest changes are forbidden; the host must END and create a new room

### 11.7 Out-of-Order Commands

**Per-sender monotonic sequence.**

- Each participant assigns a strictly increasing monotonic_seq to every command they emit
- Sequence numbers are dense: 1, 2, 3, ... no gaps permitted
- On reconnect, the client resumes at last_acked_seq + 1

**Per-room sequence.**

- Server assigns a per-room server_seq to every accepted command
- This is a total order across all senders

**Replay/duplicate handling.**

- Each client tracks the last applied monotonic_seq per sender
- A command with monotonic_seq <= last_applied is dropped as a duplicate
- A late command with monotonic_seq within 5 seconds of last_applied is applied if it would change state (e.g., a SEEK after a more recent SEEK is dropped; a PAUSE arriving after a PLAY is dropped if a more recent PAUSE is already applied)
- Commands with monotonic_seq > last_applied + 1 indicate a gap; the client requests a replay from the server for the missing range

### 11.8 Authoritative Clock

- The **server clock** is the reference for all server_ts values
- Clients compute their own offset_to_server = server_time - local_time via NTP-style request/response (4 samples, median, jitter tracked)
- All server_ts values in messages are server-authored; clients never trust a peer's clock
- A playback command carries:
  - media_position_ms: the position the host wants the room to be at
  - target_server_ts: the server time at which media_position_ms should be the current position
- Each client computes its own playback start time:
  - local_playback_start = target_server_ts - offset_to_server
  - At local time t, the expected media position is media_position_ms + (t - local_playback_start)
  - Drift = local_video.currentTime * 1000 - expected_media_position
- Drift correction is **never automatic**; it surfaces in the UI (see section 12) and the user must click "Sync to Host" to apply a manual seek

### 11.9 Idle and Timeout Rules

| Condition | Action |
|---|---|
| Room in READY/PLAYING/PAUSED with no participants for 5 minutes | Auto ENDED |
| Host disconnects for 30 seconds | End the room |
| Viewer disconnects for 5 minutes | Mark LEFT |
| Server unreachable for 60 seconds | Local UI shows offline banner; room continues based on last-known state |

## 12. Playback Synchronization Architecture

### 12.1 Core Principle

Synchronization in Locast is **command-synchronized, not frame-synchronized**. The local `<video>` element plays the local file directly. There is no peer-to-peer video streaming and no shared media clock across devices.

The architecture rests on a strict separation:

- **COMMAND messages** are authoritative and synchronized. They are signed, ordered, validated by the server, and applied identically by all authorized viewers.
- **POSITION reports** are non-authoritative passive state. They reflect what a participant's player is doing locally and are used only to render the "you are X seconds behind" indicator.

### 12.2 Local Playback Path

1. The manifest is fully downloaded and verified locally (see Core Rule: complete local file required before playback).
2. A `<video>` element is mounted with the local file URL (`file://` or a custom protocol handler).
3. The video element's `currentTime` and `playbackRate` are controlled exclusively by the command-application layer.
4. The user may pause/seek their local player manually; manual actions do not propagate to the room unless they have the `playback.issue_commands` capability.

### 12.3 Command vs. Position

| Aspect | COMMAND (PLAY/PAUSE/SEEK) | POSITION_REPORT |
|---|---|---|
| Source | Host (or co-host) only | Any participant |
| Direction | Server-relayed, authoritative | Server-relayed, advisory |
| Affects local playback | Yes | No |
| Used for drift UI | No | Yes |
| Signed | Yes (host key) | Optional, server may accept unsigned reports |
| Frequency | Low (bounded by user actions) | ~1 Hz per participant |
| Persisted in command log | Yes | No |

### 12.4 Drift Detection

**Sampling.**

- Runs at **1 Hz** on each client
- Compares `local_video.currentTime * 1000` to the host's last received `media_position_ms + elapsed since that command's target_server_ts - local_offset_to_server`
- Drift is computed only when the local player is `PLAYING`

**Thresholds.**

| Drift (absolute) | UI | Toast blocking |
|---|---|---|
| `< 200 ms` | None (green dot in status bar) | No |
| `200 ms - 2 s` | Yellow indicator | No (subtle) |
| `2 s - 5 s` | Orange indicator + subtle toast | No (subtle, auto-dismiss 3 s) |
| `>= 5 s` | Red indicator + warning toast | No (warning, auto-dismiss 5 s, with "Sync to Host" action) |

Toast is **non-blocking**: the local video continues to play; the user is notified but not interrupted. There is no auto-pause, no auto-seek.

### 12.5 Manual Sync (Sync to Host)

- User-initiated action (button or shortcut)
- Performs an **instant seek** to the host's last reported `media_position_ms`
- Issues a `SEEK` command with `reason: 'manual_sync'` if the user has the `playback.issue_commands` capability
- If the user lacks that capability, the manual sync is local-only: the local player seeks but no command is sent
- The `manual_sync` reason is broadcast to the room for presence (so others see "Alice synced to host") but does **not** trigger others to seek (see section 13)

### 12.6 Forbidden Patterns

- **No auto-seek ever.** The local player is never seeked except by a user action or by an authorized `PLAY`/`PAUSE`/`SEEK` command.
- **No forced playback rate adjustment.** The host's playback rate is not propagated.
- **No P2P frame sync.** There is no WebRTC data channel for media frames.
- **No client clock as authoritative.** All `server_ts` values are server-authored.

### 12.7 Subtitle-Sync Independence

Subtitle selection and rendering are independent of playback synchronization (see section 17). A user with subtitles on sees them aligned to the local `<video>.currentTime`, which is the local player's time. Drift between local and host does not affect subtitle timing for the local user.

### 12.8 What the Server Does

- Validates and orders commands
- Enforces capability checks (see section 14)
- Assigns `server_seq` for total ordering
- Rejects duplicate, out-of-order, or unauthorized commands
- Relays `POSITION_REPORT` messages without modification
- Does **not** interpret playback timing; the server has no concept of "is the room currently playing"

## 13. Exact Synchronization Semantics

### 13.1 Message Types

All messages are signed by the sender's Ed25519 key. The server validates the signature before forwarding.

**PLAY.**

```json
{
  "type": "PLAY",
  "sender_id": "<user_id>",
  "monotonic_seq": <u64>,
  "media_position_ms": <u64>,
  "server_ts": <u64>,
  "signature": <ed25519>
}
```

**Semantics:**

1. Server validates: sender has playback.issue_commands, room is in READY or PAUSED, monotonic_seq is exactly last_acked_seq + 1 for that sender.
2. Server stamps server_ts (clients cannot set this) and assigns server_seq.
3. Server broadcasts to all authorized viewers.
4. Each receiving client:
   - Computes local_playback_start = server_ts - offset_to_server - media_position_ms
   - If local player is currently paused at any position, calls video.play() and sets currentTime = media_position_ms / 1000 before or in the same tick
   - If local player is currently playing, seeks to media_position_ms / 1000 and continues playing
   - Records the command as the new playback reference
5. Room state transitions to PLAYING.

**PAUSE.**

```json
{
  "type": "PAUSE",
  "sender_id": "<user_id>",
  "monotonic_seq": <u64>,
  "media_position_ms": <u64>,
  "server_ts": <u64>,
  "signature": <ed25519>
}
```

**Semantics:**

1. Server validates as for PLAY, plus room must be in PLAYING.
2. Server stamps and broadcasts.
3. Each receiving client:
   - Sets currentTime = media_position_ms / 1000
   - Calls video.pause()
4. Room state transitions to PAUSED.

**SEEK.**

```json
{
  "type": "SEEK",
  "sender_id": "<user_id>",
  "monotonic_seq": <u64>,
  "media_position_ms": <u64>,
  "server_ts": <u64>,
  "reason": "host_seek" | "manual_sync",
  "signature": <ed25519>
}
```

**Semantics:**

1. Server validates: sender has playback.issue_commands, room is in PLAYING or PAUSED, monotonic seq is next.
2. Server stamps and broadcasts.
3. Each receiving client:
   - If reason == "host_seek": seeks to media_position_ms / 1000 and resumes previous play/pause state.
   - If reason == "manual_sync": **does not auto-apply.** The local player is left at its current position. Other clients see a presence event ("Alice synced to host") but their own players are not seeked.
4. The sender's own client applies the seek locally immediately.

**POSITION_REPORT.**

```json
{
  "type": "POSITION_REPORT",
  "user_id": "<user_id>",
  "media_position_ms": <u64>,
  "playing": <bool>,
  "server_ts": <u64>
}
```

**Semantics:**

1. Sent by each client at ~1 Hz while connected.
2. Not signed (server uses session token to attribute).
3. Server stamps server_ts as the time of reception, broadcasts to room.
4. Receiving clients use this only to compute drift (see section 12.4) and to render the "X seconds behind" indicator.
5. **Never** applied to the local player.

### 13.2 Deduplication

**Per-sender state.** Each client maintains last_applied_seq: Map<user_id, u64>.

**Rules.**

- A command with monotonic_seq <= last_applied_seq[sender_id] is dropped as a duplicate.
- A command with monotonic_seq == last_applied_seq[sender_id] + 1 is applied and last_applied_seq is incremented.
- A command with monotonic_seq > last_applied_seq[sender_id] + 1 indicates a gap. The client buffers it for up to **5 seconds** and requests a replay from the server for the missing range. If the gap is not filled within 5 seconds, the client applies the held command and accepts the gap (this may cause a visible desync; the user can manual-sync).

**Sliding window for late arrivals.**

- Late commands (arriving after a newer command was applied) are kept in a 5-second sliding window per sender.
- A late SEEK after a more recent SEEK is dropped (out-of-order).
- A late PAUSE after a more recent PAUSE is dropped.
- A late PAUSE after a more recent PLAY is dropped (the newer command supersedes).
- A late PLAY after a more recent PAUSE is dropped.
- Late commands outside the 5-second window are dropped unconditionally.

### 13.3 Clock-Skew Handling

**Measurement.**

- Each client periodically (every 60 seconds) measures offset to the server using an NTP-style request/response.
- Method: 4 round-trip samples, take the median offset, track jitter (standard deviation of samples).
- The sample is rejected if the round-trip time exceeds 500 ms.

**Storage.**

- skew_ms = server_time - local_time (positive if server is ahead)
- jitter_ms tracked separately
- Persisted locally; not part of room state

**Application.**

- All server_ts values in received messages are interpreted as local_ts = server_ts - skew_ms for local computation.
- The expected media position at local time t is: `expected_position_ms = command.media_position_ms + (t - (command.server_ts - skew_ms))`
- If jitter_ms > 200 ms, the client increases its drift-detection threshold to avoid spurious warnings (2s becomes 3s, 5s becomes 7s).

**Skew changes.**

- A change in skew_ms of more than 500 ms is treated as a significant event and logged; it does **not** trigger an auto-seek.
- The user may click "Recalibrate clock" in settings to force a fresh measurement.

### 13.4 Sequence Number Encoding

- monotonic_seq: u64, big-endian on the wire
- server_seq: u64, big-endian, assigned by the server
- server_ts: u64, big-endian, Unix milliseconds
- media_position_ms: u64, big-endian, milliseconds, 0 to media duration

### 13.5 Failure Modes

| Failure | Behavior |
|---|---|
| Server unreachable for > 60 s | Local UI shows offline banner; local playback continues from last command; commands queue locally and are sent on reconnect (subject to monotonic_seq continuity) |
| Client clock jumps backward (system time change) | Re-measure skew; if the jump is > 2 s, the client pauses local playback and prompts user to re-sync |
| Replay request fails | Apply held command after 5 s, accept the gap |
| Signature verification fails | Server drops the message and logs; client never sees it |
| Capability revoked mid-flight | Server checks capabilities at receive time; subsequent commands from that sender are dropped |

## 14. Permission Model

### 14.1 Design Principle

Locast uses a **capability-based** model, not a role-based one. Each participant has an explicit set of capabilities per room. The host has all capabilities by default; viewers have a configurable subset.

### 14.2 Permission Struct

```
Permission {
  scope: "playback" | "drawing" | "room" | "sync",
  action: Action,
  granted: bool
}
```

### Scopes and actions

| Scope | Action | Meaning |
|---|---|---|
| playback | issue_commands | Send PLAY/PAUSE/SEEK |
| playback | manual_sync | Use the "Sync to Host" button |
| playback | view | See the video at all (no capability = cannot render) |
| drawing | draw | Emit drawing strokes |
| drawing | undo_own | Undo own strokes |
| drawing | undo_any | Undo anyone's strokes |
| drawing | clear_all | Issue clear_all |
| room | invite | Generate invite links (out of scope v1) |
| room | kick | Remove a participant |
| room | transfer_host | Transfer host role |
| room | end | Issue END command |
| sync | receive_position | Receive others' POSITION_REPORT (privacy opt-out) |

A capability is granted or revoked per (room, user, scope, action) tuple. The full set is the participant's **capability set**.

### 14.3 Authoritative Storage

- **Server** (authoritative): table room_participant_capabilities(room_id, user_id, scope, action, granted, updated_at, updated_by)
- **Client** (mirror): same shape, in locast.db
- Server is consulted for every authorization decision; client mirror is used for UI rendering and as a fallback when offline
- Conflicts: server wins. Client mirror is reconciled on reconnect.

### 14.4 Capability Changes

- Capability changes are **signed room events** of type CAPABILITY_GRANT and CAPABILITY_REVOKE
- Must be issued by a user with room.transfer_host capability (or be the host)
- Server validates signature, applies the change, and broadcasts a CAPABILITY_UPDATE to the room
- The CAPABILITY_UPDATE event includes the new full capability set for the affected user
- All clients update their local mirror

### 14.5 Enforcement

**Server-side (authoritative).**

- **Command gating:** before forwarding any PLAY/PAUSE/SEEK, the server checks the sender has playback.issue_commands for that room. If not, the command is dropped and a COMMAND_REJECTED event is sent back to the sender (and logged).
- **Event gating:** before forwarding drawing events, the server checks drawing.draw. Before forwarding clear_all, the server checks drawing.clear_all.
- **Position-report gating:** the server respects sync.receive_position; if a user has revoked it, others' clients will not include their reports in the stream sent to that user. The user with the revoked capability still receives the rest of the room state.

**Client-side (defense in depth).**

- Before rendering any event (stroke, laser, position report), the local UI checks the event sender's capability set against the local mirror
- If the sender lacks the required capability, the event is dropped locally as well
- This is a safety net: the server is the source of truth, but a compromised or buggy client cannot render forged events

### 14.6 can() Function

```
can(participant: Participant, scope: Scope, action: Action) -> bool
```

**Algorithm:**

1. If participant.is_host, return true (host has all capabilities)
2. Look up (scope, action) in participant.capabilities
3. If present, return granted
4. If absent, return false (default deny; capabilities must be explicitly granted)

This function is the single chokepoint for all permission decisions. It is called on the server for every authorization check and on the client for every render decision.

### 14.7 Default Capability Presets

| Preset | Capabilities granted (non-host) |
|---|---|
| Viewer | playback.view, sync.receive_position |
| Editor | Viewer + drawing.draw, drawing.undo_own, playback.manual_sync |
| Co-host | Editor + playback.issue_commands, room.end, drawing.clear_all |

- The host can apply a preset to a participant as a single action
- The host can also grant/revoke individual capabilities via a per-user capability editor
- Per-user overrides always take precedence over the preset; the preset is just shorthand

### 14.8 Host Capabilities

The host has all capabilities by default, including room.transfer_host. The host's capability set is implicit and not stored as a row; can() short-circuits to true for the host.

The host can voluntarily relinquish a capability (e.g., for collaborative control), but the implicit-all behavior is restored automatically if the host later issues a command that requires a capability they had revoked - a revocation of a host capability is treated as a host-transfer signal in some cases (see below).

### 14.9 Special Cases

**Revoking playback.issue_commands from the host.**

- Treated as an implicit request to end the room
- Server transitions the room to ENDED (host transfer is deferred to v1.1; see section 11.3)

**Revoking playback.view from a participant.**

- Their client stops rendering the <video> and all overlays (drawing, laser, subtitles)
- They remain connected as a "lurker" for chat/drawing-only participation if that feature is added later
- They can be re-granted playback.view to resume

**Revoking drawing.draw.**

- The participant's in-flight strokes are not aborted
- New stroke events from that participant are rejected by the server
- Their existing strokes remain on the canvas for all viewers

### 14.10 Audit Trail

- All capability changes are persisted in room_events with full payload, signatures, and timestamps
- The host can view a per-room audit log of who granted/revoked what and when
- Audit log is retained for the lifetime of the room (and the local mirror forever)

## 15. Drawing Protocol

### 15.1 Overview

Drawing in Locast is a **vector overlay** above the `<video>` element. Only vector events are exchanged; no images, no bitmaps, no base64-encoded drawings. The wire format is JSON for v1, with a binary fallback path planned.

### 15.2 Local Rendering

- A transparent `<canvas>` is layered above the `<video>` element
- Canvas size matches the video's intrinsic dimensions
- Canvas is re-rendered from the local stroke history on every relevant state change (resize, new stroke, undo, clear)
- The canvas has `pointer-events: auto` for the local user and `pointer-events: none` for remote strokes (drawn into a separate layer to prevent input conflicts)

### 15.3 Tools

The following tools are first-class and rendered locally as code that generates vector primitives from input events:

| Tool ID | Behavior |
|---|---|
| `pen` | Freehand stroke. Polyline with width and color. |
| `arrow` | Straight line with an arrowhead at the end. |
| `rect` | Axis-aligned rectangle (dragged corners). |
| `circle` | Ellipse fitting the dragged bounding box. |
| `text` | A text label at the click point; uses the local font. |
| `eraser` | A stroke that subtracts from the canvas using `globalCompositeOperation = 'destination-out'`. |

Each tool is a pure function of its input events and a configuration (color, width). The renderer does not know the difference between "the user drew this locally" and "this stroke arrived from the network" - it just renders strokes.

### 15.4 Wire Format

All drawing events are signed by the sender and relayed by the server. They are NOT commands; they are unordered, best-effort events. The server validates capability (`drawing.draw` for stroke events, `drawing.clear_all` for clear) and broadcasts to all authorized viewers.

**`stroke_begin`.**

```json
{
  "type": "stroke_begin",
  "id": "<uuid v7>",
  "user_id": "<user_id>",
  "tool": "pen" | "arrow" | "rect" | "circle" | "text" | "eraser",
  "color": "#rrggbb",
  "width": <u16, px>,
  "ts": <u64, client ms>,
  "server_ts": <u64, server ms, set by server>,
  "signature": <ed25519>
}
```

Emitted by the client when the user begins a stroke (pointer down). `id` is the unique stroke identifier used by all subsequent `stroke_point` and `stroke_end` events for this stroke.

**`stroke_point`.**

```json
{
  "type": "stroke_point",
  "id": "<uuid v7>",         // matches stroke_begin.id
  "x": <f32, 0..1>,          // normalized to canvas width
  "y": <f32, 0..1>,          // normalized to canvas height
  "pressure": <f32, 0..1>,   // optional, omit for non-pressure devices
  "ts": <u64, client ms>
}
```

Emitted as the user moves the pointer. Coalesced within a single animation frame on the client before being sent. Not individually signed (the stroke as a whole is bound to the signed `stroke_begin`).

**`stroke_end`.**

```json
{
  "type": "stroke_end",
  "id": "<uuid v7>",
  "ts": <u64, client ms>
}
```

Emitted when the user releases the pointer. Closes the stroke.

**`stroke_undo`.**

```json
{
  "type": "stroke_undo",
  "id": "<uuid v7, unique to this event>",
  "user_id": "<user_id>",
  "target_stroke_id": "<uuid of the stroke to remove>",
  "ts": <u64, client ms>,
  "server_ts": <u64, server ms>,
  "signature": <ed25519>
}
```

Undoes a specific stroke. The `target_stroke_id` may be the user's own stroke (requires `drawing.undo_own`) or any stroke (requires `drawing.undo_any`).

**`clear_all`.**

```json
{
  "type": "clear_all",
  "user_id": "<user_id>",
  "ts": <u64, client ms>,
  "server_ts": <u64, server ms>,
  "signature": <ed25519>
}
```

Clears the entire drawing layer. Requires `drawing.clear_all`. Signed and capability-checked.

### 15.5 Eraser Semantics

- The eraser is implemented as a special stroke with `tool: "eraser"`
- The renderer draws the eraser stroke with `globalCompositeOperation = 'destination-out'`, which removes pixels along the stroke path
- The eraser stroke is a real vector object in the stroke list (so it can be undone via `stroke_undo`)
- When a user undoes an eraser stroke, the erased content reappears
- Erased regions are not stored as negative space; they are computed at render time by the order of strokes and erasers

### 15.6 Undo

- **Stroke-level only.** There is no per-point undo.
- Each user maintains a per-user ring buffer of the last N strokes they have emitted (N = 50 by default; configurable)
- `stroke_undo` can target any stroke in the room (subject to capability), not just the sender's own
- Undo is a destructive event; once undone, the stroke is removed from the local stroke list and a `stroke_undo` event is broadcast
- The receiving client also removes the stroke from its local list
- There is no redo in v1

### 15.7 Sync

- Strokes are broadcast to all authorized users via server-relayed room events
- The server is a dumb relay for stroke events (it does not interpret or coalesce them)
- The server assigns `server_ts` to `stroke_begin`, `stroke_undo`, and `clear_all` events at reception time
- Late-arriving `stroke_point` events for an unknown `id` are dropped (the stroke was never begun or has been undone)
- A `stroke_end` arriving without preceding points renders the stroke as a single-point dot (a "tap")

### 15.8 Throttling

- **Maximum 120 points per second per user.** A client sending faster than this coalesces points within a single animation frame.
- `stroke_begin` and `stroke_end` are not throttled.
- A single stroke may have at most 10,000 points; clients truncate beyond this (and emit a `stroke_end`).
- The server enforces a hard cap: any client exceeding 500 events per second across all types is temporarily disconnected with a `RATE_LIMITED` reason.

### 15.9 Persistence

- All drawing events are stored in the client's local SQLite for the lifetime of the room
- On room rejoin (e.g., after a restart), the drawing layer is reconstructed from local history
- The server does not retain drawing history beyond the active session (the room's command log may include drawing events for replay during the session, but the server has no obligation to retain them after `ENDED`)

### 15.10 Performance Budgets

- Stroke render budget: 16 ms per frame for 1,000 active strokes
- Stroke point in-memory storage: ring buffer of last 5,000 points per active stroke; older points are downsampled (every 2nd, then 4th, then 8th point) for `pen` strokes
- Canvas resolution: matches video; HiDPI handled by scaling the canvas backing store

## 16. Laser Pointer Protocol

### 16.1 Distinction from Drawing

The laser pointer is a **transient effect**, not a persistent stroke. It is not stored in the stroke list, not undoable, and not persisted across sessions. It is a presence indicator showing where a user is currently pointing.

### 16.2 Local Rendering

- Each user has at most one active laser
- The laser is rendered on the drawing canvas, above strokes
- The dot is **8-12 px** in radius (configurable; default 10 px)
- A **fading polyline trail** of the last N positions (N = 20 by default) is drawn behind the dot
- Trail fade: oldest point fully transparent, newest point fully opaque, linear fade over 400 ms
- Fade animation uses requestAnimationFrame; the trail is recomputed every frame from a timestamped position history
- When the user releases the pointer (or laser_off is received), the dot and trail fade out over 200 ms and are then removed

### 16.3 Wire Format

**laser_move.**

```json
{
  "type": "laser_move",
  "user_id": "<user_id>",
  "x": <f32, 0..1>,          // normalized to canvas
  "y": <f32, 0..1>,
  "ts": <u64, client ms>,
  "server_ts": <u64, server ms>
}
```

Emitted while the pointer is active, at **<= 60 Hz** (one event per animation frame at most). Not individually signed; the server uses the session token to attribute.

**laser_off.**

```json
{
  "type": "laser_off",
  "user_id": "<user_id>",
  "ts": <u64, client ms>,
  "server_ts": <u64, server ms>
}
```

Emitted when the pointer is released. Triggers the fade-out on all clients.

### 16.4 Color Assignment

- The **active "drawing" user** (the one currently emitting drawing strokes, or the most recent) renders their laser in **red** by default
- Other users' lasers are colored by a **hash of their user_id** mapped to a fixed hue palette (12 visually distinct hues, e.g., red, orange, yellow, lime, green, teal, cyan, blue, indigo, purple, pink, magenta)
- Hue assignment is stable per user across the session
- The local user always sees their own laser in red (the "you are here" cue)
- If a non-drawing user begins drawing, their laser color transitions to red on the next stroke_begin

### 16.5 Activation

- Activated by holding a modifier (e.g., Ctrl or right-mouse-button) and moving the pointer
- Released on pointer-up or modifier-up
- A click without modifier does **not** activate the laser
- The laser is suppressed if the local user is currently drawing (the two modes are mutually exclusive locally)

### 16.6 Sync

- The server is a dumb relay; it does not interpret laser events
- laser_move events older than 1 second are dropped at reception (they are too stale to be useful for trail rendering)
- The server may rate-limit laser_move per user to **120 events/sec** (hard cap; above this the excess is dropped)
- There is no acknowledgment, no replay, no persistence; if a laser_move is lost, the next one supersedes it

### 16.7 Capability

- Lasers require drawing.draw capability (same as drawing)
- A user with drawing.draw revoked mid-session will have their laser suppressed on the next server check
- A user who never had drawing.draw cannot emit laser events (server drops them at the gate)

### 16.8 Performance

- Trail buffer is a ring of the last 20 positions with timestamps
- Trail fade is computed once per frame using requestAnimationFrame
- Up to 16 simultaneous lasers (one per participant) must render within the 16 ms frame budget

## 17. Subtitle Architecture

### 17.1 Core Principle

Subtitles are **independent of room synchronization**. Each viewer picks their own subtitle track (or none). The host's selection does not propagate.

### 17.2 Distribution

Subtitles are **separate files in the manifest**, not embedded in the media. The manifest lists available subtitle tracks as discrete files, each with:

- id: stable identifier
- format: vtt | srt | ssa | ass
- language: BCP-47 tag
- label: human-readable label
- file_path: relative path within the media bundle
- size_bytes
- hash: BLAKE3 for verification

Subtitle files are downloaded alongside the media file and verified via the hash. They are subject to the same Core Rule: **complete local file required before playback**.

### 17.3 Parsing

Two paths are supported; the choice is made at build time per format:

**Path A: In-browser parsing.**

- The subtitle file is read as text and parsed by a JS parser
- Parsers: webvtt-parser for VTT, a custom SRT/SSA/ASS parser in TS
- Output: an in-memory list of cues { start_ms, end_ms, text, style? }
- Parsing happens once on load and is cached in memory

**Path B: Rust pre-parse via IPC.**

- For performance with large subtitle files, the Rust backend can pre-parse the subtitle file and ship the cue list as JSON over the Tauri IPC bridge
- The Rust parsers are the source of truth; the TS parsers are a fallback
- Path B is preferred for ssa/ass (complex styling) and for SRT files > 500 KB

The choice is transparent to the renderer; both paths produce the same cue list shape.

### 17.4 Viewer Selection

- Each viewer independently chooses: **Off**, or one of the provided tracks
- The selection is stored locally in locast.db per (room, user)
- The selection is **not** part of room state and is **not** broadcast
- The host's selection is invisible to viewers

### 17.5 Rendering

**WebVTT.**

- Use a <track> element bound to the <video> element
- Mode: track.mode = 'showing' when the track is active
- Browser handles timing, positioning, and defaults
- WebVTT styling (cue settings) is honored

**SRT / SSA / ASS.**

- Render on an overlay <div> positioned over the <video>
- Sync via the <video>.timeupdate event (or requestVideoFrameCallback when available)
- The overlay reads video.currentTime and shows the active cue
- Cues are positioned using a configurable layout (see 17.6)

**timeupdate rate.**

- timeupdate fires at ~4 Hz on most browsers, which is insufficient for fast cue transitions
- For high-precision cue boundaries, the renderer also uses requestVideoFrameCallback (Chrome/Edge) or a 60 Hz requestAnimationFrame loop that checks video.currentTime against cue boundaries
- Fallback: a 30 Hz setInterval poll for cue transitions

### 17.6 Local Preferences

The following are per-user, per-device, and never synced:

- **Subtitle size**: small (16 px), medium (20 px), large (28 px), x-large (36 px) - configurable in 1 px steps
- **Color**: text color, background color, outline color
- **Font**: a fixed list of safe fonts (system-ui, sans-serif, monospace, serif) plus user-installed fonts discoverable via the OS font API
- **Position**: bottom (default), top, middle, custom Y offset
- **Default offset**: a per-track offset in milliseconds, range -10000 to +10000, default 0
- **Background opacity**: 0 to 1, default 0.5

Preferences are stored in locast.db under a subtitle_prefs table. They apply to all rooms for that user on that device.

### 17.7 Sync with Playback Drift

- Subtitles are timed against the **local <video>.currentTime**, not the host's position
- If the local player drifts from the host, the local user's subtitles stay aligned to their local playback (they are not "ahead" or "behind" - they are correct for what the user sees)
- A manual sync (seek) re-aligns the local player to the host, and the next cue change will reflect the new local time

### 17.8 Persistence

- Parsed cue lists are cached in memory only; they are re-parsed on each room open
- The user's selection is persisted per (room_id) so re-opening a room restores the choice
- Subtitle files themselves are persisted on disk in the media bundle; they are not re-downloaded on re-open

### 17.9 Unsupported Features (v1)

- No closed captions for live or streamed content (Locast has no live mode in v1)
- No subtitle authoring or editing
- No machine translation
- No forced subtitles (always-on tracks based on language match)
- No picture-in-picture subtitles
## 18. Network Protocol Design

### 18.1 Scope

> **v1 Transport Split:** WebSocket via the server is authoritative for room control, sync, drawing, laser, chat, presence, permissions. WebRTC DataChannels are used ONLY for complete-file transfer during the download phase. After download, no DataChannel traffic is required for playback.

This section defines the single wire format used over **both** transports:

- **WebSocket** (WSS) - signaling, presence, authoritative command relay, control traffic. This is the authoritative path for room control, sync, drawing, laser, chat, presence, and permissions.
- **WebRTC DataChannel** - complete-file (bulk) transfer between peers ONLY. No room-control, sync, drawing, laser, chat, presence, or permission traffic flows over a DataChannel in v1.

All room events (room create/join/leave, manifest publish, playback commands, drawing, laser, presence, permissions) flow over the WebSocket via the server. The only thing that varies per transport is framing (WS binary frames vs. DataChannel binary messages) and authentication context.

### 18.2 Encoding choice: MessagePack

We pick **MessagePack** (via `msgpackr` on the web and `rmp-serde` on Rust) rather than CBOR. Justification:

- `msgpackr` is small (~12 KB gzipped), has excellent TypeScript types, and preserves `Uint8Array` without forcing users to manually base64-wrap binary signatures. CBOR tooling in the browser is heavier and has worse TS inference.
- Round-trip performance is comparable to CBOR for our message sizes (median envelope ~250 B; largest realistic message is a SDP blob ~10 KB).
- Canonical MessagePack (via `canonicalize: true` in msgpackr, with sorted map keys) is sufficient for the small subset of messages we sign; we do not require full RFC 7049 canonical CBOR, so the cost of CBOR is not justified.
- Both libraries have stable, audited Rust and JS implementations with no unsafe and no native deps.

Every signed message is canonicalized to deterministic bytes before signing (see 18.7). The on-the-wire encoding is binary MessagePack; CBOR-style diagnostic notation is used in this document for readability.

### 18.3 Common envelope

Every message, on every transport, has the following outer envelope:

```
{
  v:        1,                       // protocol version
  type:     "<TYPE>",                // string, see 18.4
  id:       "<uuid v7>",             // unique id for this message instance
  room_id:  "<uuid v7>" | null,      // null only for HELLO / WELCOME / CHALLENGE / AUTH_*
  sender: {
    user_id: "<uuid v7>",
    pubkey:  "<32 bytes Ed25519>",
    sig:     "<64 bytes Ed25519>"    // signature over canonical payload
  } | null,                          // null for server-originated messages
  ts_ms:    <i64>,                   // sender wall clock at send
  seq:      <u64>,                   // monotonic per-sender
  payload:  { ... }                  // type-specific, see 18.4
}
```

Rules:

- `v` is checked on receive; messages with `v != 1` are rejected.
- `id` is the message's idempotency key for server-side dedup (TTL 10 min).
- `room_id` is required for every message whose `type` carries a room context.
- `sender` is present and **must** be verified for every message except:
  - HELLO, WELCOME, CHALLENGE, AUTH_OK, AUTH_FAIL (handshake)
  - server-originated broadcasts (e.g. ROOM_JOINED echoed by the server)
- `ts_ms` is informational; replay protection is by `seq` (18.6) and freshness window (18.7).
- `seq` is per-sender monotonic. First message after connect is 1. Never reused. Per-room the server tracks the last applied `seq` per `(room_id, user_id)` for dedup.
### 18.4 Message types (by category)

All types are upper snake case. Required vs. optional fields are noted; payload keys not listed are rejected (unknown-field rejection; see 21.7).

**18.4.1 Handshake / auth.**

| Type | Direction | Payload fields |
|---|---|---|
| HELLO | C -> S | `{ client_version: string, platform: "win"\|"mac"\|"linux", device_id: string }` |
| WELCOME | S -> C | `{ session_id: uuid, server_ts_ms: i64, config: { max_room_size: u8, rate: { msgs_per_sec: u16, bytes_per_sec: u32 } } }` |
| CHALLENGE | S -> C | `{ nonce: bytes(32), expires_ms: i64 }` |
| AUTH_OK | S -> C | `{ user_id: uuid, bearer: { token: bytes(32), expires_ms: i64 }, pubkey: bytes(32) }` |
| AUTH_FAIL | S -> C | `{ reason: "bad_sig"\|"expired"\|"banned"\|"rate" }` |

**18.4.2 Room lifecycle.**

| Type | Direction | Payload fields |
|---|---|---|
| ROOM_CREATE | C -> S | `{ code: string(6), title: string, password?: string(<=128), capabilities: { default_participant: cap_set, host: cap_set } }` |
| ROOM_JOIN_REQUEST | C -> S | `{ code: string(6), password?: string, display_name: string(<=32) }` |
| ROOM_JOINED | S -> C | `{ room: { id, code, title, host_user_id, created_ms, participants: [Participant] }, you: { cap_set, joined_ms } }` |
| ROOM_LEAVE | C -> S | `{}` |
| ROOM_END | S -> C | `{ reason: "host_left"\|"host_transfer"\|"empty"\|"admin" }` |

`Participant = { user_id, display_name, joined_ms, cap_set, status: "joining"|"ready"|"reconnecting"|"disconnected", last_seen_ms, p2p: { candidate_user_ids: [uuid] } }`.

**18.4.3 Manifest.**

| Type | Direction | Payload fields |
|---|---|---|
| MANIFEST_PUBLISH | host C -> S -> all | `{ manifest: SignedManifest }` (see draft 06) |
| MANIFEST_REQUEST | C -> S | `{ media_id: uuid }` |
| MANIFEST_UPDATE | host C -> S -> all | `{ reason: "added"\|"removed"\|"replaced", media_id: uuid, new_manifest?: SignedManifest }` |
| MANIFEST_RESPONSE | S -> C | `{ signed: SignedManifest }` |

**18.4.4 Peer announce / P2P setup.**

| Type | Direction | Payload fields |
|---|---|---|
| PEER_ANNOUNCE | S -> C | `{ user_id, pubkey, display_name, joined_ms, p2p_hint?: { direct: bool, relay: bool } }` |
| PEER_LEAVE | S -> C | `{ user_id, reason: "leave"\|"timeout"\|"kick" }` |

**18.4.5 Signaling (SDP/ICE relay).**

| Type | Direction | Payload fields |
|---|---|---|
| SIGNAL | C -> S -> C | `{ to_user_id: uuid, kind: "offer"\|"answer"\|"ice", sdp?: string, candidate?: { candidate, sdpMid, sdpMLineIndex } }` |

**18.4.6 Chat (server-relayed only in v1).**

| Type | Direction | Payload fields |
|---|---|---|
| CHAT_SEND | C -> S | `{ text: string(<=2000), reply_to_msg_id?: uuid }` |
| CHAT_MSG | S -> C | `{ from: { user_id, display_name }, text, sent_ms, id, reply_to_msg_id? }` |
| CHAT_REACT | C -> S -> C | `{ msg_id, emoji: string(<=8) }` (v1.1, optional) |

**18.4.7 Playback control.**

| Type | Direction | Payload fields |
|---|---|---|
| PLAYBACK_CMD | C -> S -> all | `{ action: "play"\|"pause"\|"seek"\|"rate"\|"stop", media_id: uuid, position_ms?: i64, rate?: f32, client_ts_ms: i64 }` |
| POSITION_REPORT | C -> S (host only fans out) | `{ media_id, position_ms, playing: bool, ts_ms }` |

Only the host may issue PLAYBACK_CMD; server rejects non-host PLAYBACK_CMD unless the sender has capability `playback_control` (see draft 05).

**18.4.8 Drawing / laser (server-relayed only in v1).**

| Type | Direction | Payload fields |
|---|---|---|
| DRAW_BEGIN | C -> S -> all | `{ media_id, color: u32(RGBA), width_px: f32, tool: "pen"\|"highlighter"\|"eraser", ts_ms }` |
| DRAW_POINT | C -> S -> all | `{ stroke_id: uuid, x: f32, y: f32, pressure: f32 }` |
| DRAW_END | C -> S -> all | `{ stroke_id: uuid }` |
| DRAW_UNDO | C -> S -> all | `{ scope: "self"\|"all", target: { last: true } \| { stroke_id } \| { count: u16 } }` |
| DRAW_CLEAR | C -> S -> all | `{ scope: "self"\|"all", media_id }` |
| LASER_MOVE | C -> S -> all | `{ x: f32, y: f32 }` |
| LASER_OFF | C -> S -> all | `{}` |

Drawing and laser are server-relayed in v1 (WebSocket via the server); they are never carried over WebRTC DataChannels. This is the only correct placement for permission enforcement and audit.

**18.4.9 Permissions.**

| Type | Direction | Payload fields |
|---|---|---|
| PERMISSION_SET | host C -> S -> all | `{ target_user_id, cap_set, expires_ms?: i64 }` |
| PERMISSION_QUERY | C -> S | `{ target_user_id }` |
| PERMISSION_REPLY | S -> C | `{ target_user_id, cap_set, source: "default"\|"host"\|"server" }` |

**18.4.10 Presence.**

| Type | Direction | Payload fields |
|---|---|---|
| PRESENCE | C -> S (and S -> others as needed) | `{ status: "ready"\|"reconnecting"\|"away", media_id?, position_ms?, since_ms }` |

Heartbeats are not a separate message; presence is piggy-backed on the client's regular outbound traffic plus an explicit PRESENCE every 5 s (see draft 20). A pure-keepalive WS ping is a protocol-level frame, not a message.

**18.4.11 File transfer (downloads).**

The download protocol is the same wire format, used over both WS (fallback) and the dedicated ordered DataChannel "files" (preferred). Messages are signed by the host (manifest) and by the source peer (chunk signature).

| Type | Direction | Payload fields |
|---|---|---|
| DOWNLOAD_REQ | viewer -> S | `{ media_id, sha256 }` |
| DOWNLOAD_OFFER | source -> viewer | `{ media_id, sha256, size: u64, chunk_size: u32, chunks: u32, transfer_id: uuid, via: "p2p"\|"relay" }` |
| DOWNLOAD_CHUNK | source -> viewer | `{ transfer_id, index: u32, hash: bytes(32), data: bytes }` |
| DOWNLOAD_ACK | viewer -> source | `{ transfer_id, index: u32 }` |
| DOWNLOAD_NAK | viewer -> source | `{ transfer_id, index: u32, reason: "hash_mismatch"\|"out_of_range" }` |
| DOWNLOAD_COMPLETE | viewer -> S | `{ transfer_id, sha256 }` |
| DOWNLOAD_FAIL | either | `{ transfer_id, reason: "too_many_naks"\|"io"\|"cancelled"\|"timeout" }` |
| DOWNLOAD_PAUSE | either | `{ transfer_id }` |
| DOWNLOAD_RESUME | either | `{ transfer_id, resume_index: u32 }` |

`data` size is exactly `chunk_size` bytes except the final chunk which is `size % chunk_size` (or `chunk_size` if it divides evenly). `chunk_size` is chosen by the source from the allowed set `{64 KiB, 256 KiB, 1 MiB}` and recorded in DOWNLOAD_OFFER. Default is 256 KiB.

**18.4.12 Error / rate limit.**

| Type | Direction | Payload fields |
|---|---|---|
| ERROR | S -> C | `{ code: "bad_version"\|"unauthorized"\|"forbidden"\|"bad_msg"\|"room_full"\|"banned"\|"internal", ref_id?: uuid, message?: string }` |
| RATE_LIMIT | S -> C | `{ scope: "conn"\|"room"\|"ip", retry_after_ms: u32, observed: u32, limit: u32 }` |

### 18.5 Size limits

- **Default message size limit:** 8 KiB (after MessagePack encoding, before transport framing). Applies to all messages except those in 18.5.1.
- **Signaling blobs (SIGNAL only):** 64 KiB. SDP offers occasionally exceed 8 KiB; ICE candidates are small.
- **Download chunks:** 1 MiB hard ceiling per chunk (i.e. `data` field plus envelope). DOWNLOAD_OFFER and control messages still capped at 8 KiB.
- **Hard ceiling at WS layer:** 1 MiB. Any WS frame exceeding this is dropped and the connection is closed with code 1009 (Message Too Big). This is a transport-level backstop; the app-layer caps are stricter.

| Type | App-layer max |
|---|---|
| All except below | 8 KiB |
| SIGNAL (SDP blobs) | 64 KiB |
| DOWNLOAD_CHUNK (with data) | 1 MiB |
| DRAW_POINT (per point) | 256 B |
| CHAT_MSG text | 2 KiB (utf-8) |

### 18.6 Rate limits

Two scopes:

- **Per-connection (per WS or per P2P DataChannel):**
  - 100 msg/sec sustained, 200 msg/sec burst (token bucket, refill 100/s, cap 200).
  - 1 MB/sec sustained bytes, 2 MB/sec burst.
- **Per-room aggregate (server-side):**
  - 1 Gbps aggregate signaling bandwidth (single-node v1 cap; see 20).
  - 1000 msg/sec across the room before RATE_LIMIT backpressure is applied to the loudest sender.

Drawing is throttled harder:

- DRAW_POINT: 120 Hz per user (token bucket, cap 120 tokens, refill 120/s). Excess points are coalesced client-side (last-point-wins within a 8 ms window) before transmission.

Heartbeats do not count against the message rate.

RATE_LIMIT is sent on the offender's connection; the server still relays other participants unaffected.

### 18.7 Monotonic seq and dedup

- Each sender maintains a strictly increasing `seq` (u64). The first message on a new connection is `seq = 1`. Gaps are tolerated (e.g. dropped heartbeat), but a `seq` that goes backward is a protocol violation and the connection is closed.
- The server tracks `last_applied_seq[(room_id, user_id)]` in memory. On receive, if `seq <= last_applied_seq` the message is dropped silently (a duplicate or a replay) and the server increments a counter.
- If `seq > last_applied_seq + 1` the server buffers up to **32** missing messages per `(room, user)` to allow out-of-order delivery, otherwise applies in arrival order. After 500 ms of waiting for a missing message, the gap is declared lost and `last_applied_seq` advances to the highest contiguous `seq` applied.
- The server stamps the message with `server_ts_ms` at the moment it accepts it (added as a transport-internal field, not part of the signed envelope).
- Messages with `ts_ms` older than 30 s (relative to `server_ts_ms`) are dropped (replay window). The exception is position reports and presence, which use a 5 s window.

### 18.8 Signing rules

Every command from a non-host user is signed; the host signs manifest publishes; the source peer signs each DOWNLOAD_CHUNK. The server verifies before relay (server is the gatekeeper; see threat model 20.9).

Signed scope:

- HELLO/CHALLENGE/AUTH_OK are not signed; the challenge-response proves possession of the private key.
- ROOM_CREATE is signed by the creator (they will be host). The server binds the room to their `user_id` (host impersonation; see 21.4).
- MANIFEST_PUBLISH is signed by the host's user_id, and the inner manifest carries a separate signature from the host over the canonical media metadata (this is the "media integrity" signature; see draft 06).
- PLAYBACK_CMD, DRAW_*, LASER_*, CHAT_SEND, PERMISSION_SET - always signed by the sender's `user_id` whose pubkey is in the envelope.
- DOWNLOAD_OFFER signed by source peer; DOWNLOAD_CHUNK signed by source peer over `transfer_id || index || hash || data` (the per-chunk signature lives on the inner payload, not the envelope, to keep the per-message cost low). The source peer's pubkey is in the OFFER's `sender` envelope, and the viewer checks `chunk.sig` against that pubkey.

### 18.9 Canonicalization for signing

To produce a stable byte string for Ed25519 signing, the payload is encoded with the following rules:

1. Serialize the **inner payload only** (not the full envelope) with `msgpackr` in canonical mode: `canonical: true`, `sortKeys: true`, no record extension types, no timestamps, no `bin`/`str` ambiguity (we use `bin` for raw bytes, `str` for utf-8).
2. Prepend a domain separation tag (16 bytes) to the payload: `locast/v1/<message_type>` padded/truncated to 16 bytes, where `<message_type>` is the UTF-8 type name. The first 8 bytes are the ASCII string `locast/v1` (padded with NULs to 8), the next 8 bytes are the type name truncated or NUL-padded.
3. The Ed25519 signature is over `domain_tag || canonical_payload`.
4. The signature is stored in `sender.sig`; the pubkey in `sender.pubkey`.
5. The server reconstructs the same bytes from the wire payload (re-encoding with canonical mode) and verifies with `ed25519-dalek` (Rust) or `@noble/ed25519` (JS, audited).

Why not sign the whole envelope? Because `id`, `ts_ms`, and `seq` are sender-injected counters and signing them would make every message uniquely non-replayable across the protocol but also unreproducible for debugging/logging. Signing the payload and using `seq` for replay protection separates the concerns.

### 18.10 Compression

**Recommendation: no per-message compression in v1.** We do not enable zstd or gzip on individual messages. Reasons:

- Median message is 250 B; zstd would barely shrink it and adds a per-message CPU cost on every send/receive.
- SDP blobs in SIGNAL are already 10-30 KB; compressing 64 KiB MessagePack blobs with zstd is cheap but the gain is ~30%, not worth a new dependency on the wire.
- Browsers' built-in `permessage-deflate` is **off by default** in our axum WS config for security (CRIME/BREACH on a chat surface) and the same reason applies to our app-layer zstd.

If v1.5 measurements show WS bandwidth > 30% of NIC capacity on the busiest rooms, we revisit and add an opt-in `compress: "zstd"` envelope flag.

### 18.11 Versioning

- `v = 1` is current. The server announces supported `v` in WELCOME.
- Additive changes (new optional field, new `type`) bump to `v = 1.x` and are negotiated via WELCOME; old clients ignore unknown fields.
- Breaking changes (removed field, semantic change) bump `v = 2`; old clients are refused at HELLO.
- Clients send `v = highest_supported`; server picks the highest mutually supported and echoes in WELCOME.

### 18.12 Wire format example (CHAT_MSG)

```
{
  v: 1,
  type: "CHAT_MSG",
  id: "018f3b1a-7c4e-7d2a-9b3c-1a4f2c9e8b21",
  room_id: "018f3b1a-0000-7000-8000-000000000001",
  sender: { user_id: "018f3b1a-...", pubkey: <32 bytes>, sig: <64 bytes> },
  ts_ms: 1764180612000,
  seq: 42,
  payload: {
    from: { user_id: "018f3b1a-...", display_name: "Alice" },
    text: "hello",
    sent_ms: 1764180612000,
    id: "018f3b1a-7c4e-7d2a-9b3c-1a4f2c9e8b21"
  }
}
```

This message is exactly 25 bytes for the payload text alone + envelope overhead. The 8 KiB cap is comfortable for normal chat; only binary abuse or pasted blobs will hit it, and those are rejected at the schema layer.


## 19. WebRTC and Signaling Architecture

### 19.1 Scope

> **v1 Transport Split:** WebSocket via the server is authoritative for room control, sync, drawing, laser, chat, presence, permissions. WebRTC DataChannels are used ONLY for complete-file transfer during the download phase. After download, no DataChannel traffic is required for playback.

This section defines the peer-to-peer topology, the signaling flow that establishes it, and the chunked-file DataChannel that runs over it. It is deliberately conservative in v1: a full mesh with a hard participant cap and P2P reserved for complete-file transfer. Control traffic (playback, drawing, laser, chat, permissions, presence) stays on the WebSocket server.

### 19.2 Topology

**19.2.1 Full mesh for v1.**

Every participant opens a `RTCPeerConnection` to every other participant in the room. With N participants this is `N * (N-1) / 2` connections.

- **Hard cap: 8 participants per room.** Above 8, the server refuses ROOM_JOIN_REQUEST with `ERROR("room_full")`. This cap is below the practical limit of a browser's `RTCPeerConnection` count and well below the limit where mesh bandwidth becomes a problem.
- **Soft cap recommendation: 6 participants.** At 6 the mesh is comfortable on consumer connections; at 7-8 the host's outbound upload (sending chunks to 7 others) is the bottleneck on asymmetric links.
- **v1.5: SFU for > 8 participants is explicitly out of scope.** A future revision may introduce a server-side SFU for control and a TURN-relay for media bytes; the current design assumes no media bytes.

**19.2.2 Why full mesh, not SFU.**

- We never relay media bytes through the server (see 1.x in draft 01). An SFU's value is in fan-out of media; we have no media bytes to fan out.
- The only thing we need to fan out is control traffic, which is cheap and already in our WS path.
- Mesh simplifies the trust model: there is no server-side media plane to harden.

**19.2.3 Connection order.**

Connections are established **in deterministic order: by `user_id` lexicographic (UUID string compare)**. Each new participant, on receiving ROOMS_JOINED or PEER_ANNOUNCE, iterates the current participant list in sorted order and calls `createOffer()` to everyone who has not yet called them. The first in the sort (the lowest UUID) always initiates; ties on identical UUIDs are impossible.

This avoids both peers calling `createOffer` at the same time (glare) and guarantees a stable, debuggable connection graph.

### 19.3 Signaling flow

The signaling channel is the same WebSocket used for control. There is no separate signaling socket.

**19.3.1 End-to-end sequence (two clients joining an existing room).**

1. **Client A connects to `wss://server/ws`** and completes the auth handshake (HELLO -> WELCOME -> CHALLENGE -> AUTH_OK; see draft 08 section 18.4.1).
2. **Client A sends `ROOM_JOIN_REQUEST{ code, password? }`.**
3. **Server replies `ROOM_JOINED{ room, you }`** with the current participant list (A is now in it).
4. **Server broadcasts `PEER_ANNOUNCE{ A }`** to all existing participants.
5. **Client B receives PEER_ANNOUNCE for A.** B inserts A into its participant list. B sees A's UUID and its own UUID; whoever sorts first is the offerer. (Assume A sorts first; then A will call B.)
6. **A creates an `RTCPeerConnection` with the configured ICE servers** (19.5) and a single `RTCDataChannel("files", { ordered: true })` for complete-file transfer. No other DataChannel is created; all room control is server-relayed over the WebSocket.
7. **A calls `createOffer()` and sends the SDP to the server in `SIGNAL{ to: B, kind: "offer", sdp }`.**
8. **Server relays SIGNAL to B** (capability check: any room member may signal).
9. **B calls `setRemoteDescription(offer)`, then `createAnswer()`, then sends the answer back in `SIGNAL{ to: A, kind: "answer", sdp }`.**
10. **Both sides exchange ICE candidates** via `SIGNAL{ kind: "ice", candidate }` until both `iceConnectionState` reach `connected` or `completed`.
11. **The DataChannel "files" emits `open`.** A confirms to the server `PEER_ANNOUNCE{ A, p2p_hint: { direct: true } }` (optional, optimistic).
12. **The same flow repeats for every other participant.** With N existing participants, the joiner establishes N peer connections.

**19.3.2 SDP constraints.**

- `bundlePolicy: "max-bundle"` - single transport for all m-lines.
- `rtcpMuxPolicy: "require"`.
- `iceTransportPolicy: "all"` (STUN + TURN).
- `sdpSemantics: "unified-plan"`.
- `iceServers`: see 19.5.
- No `addTrack`; we use only DataChannels, so no media m-lines appear in the SDP. This keeps the SDP tiny and rules out codec negotiation surprises.

**19.3.3 ICE candidate exchange.**

- Trickle ICE: each `onicecandidate` is forwarded as `SIGNAL{ kind: "ice" }`.
- `end-of-candidates` is signaled by a `SIGNAL{ kind: "ice", candidate: null }` message.
- The server does not inspect or rewrite candidates; it is a pure relay.

**19.3.4 Connection state observation.**

- `pc.connectionState`: if `failed`, the client tears down and attempts one ICE restart (new offer/answer) before giving up. After give-up, the peer is marked `p2p: "disconnected"` and downloads pause; see draft 22.
- `pc.iceConnectionState`: used to drive UI only.

### 19.4 Deliberate design choice: control stays on the server

In v1 the following are **server-relayed only**, not P2P:

- Playback commands (PLAYBACK_CMD, POSITION_REPORT).
- Drawing (DRAW_BEGIN, DRAW_POINT, DRAW_END, DRAW_UNDO, DRAW_CLEAR).
- Laser (LASER_MOVE, LASER_OFF).
- Chat (CHAT_SEND, CHAT_MSG).
- Permissions (PERMISSION_SET, PERMISSION_QUERY, PERMISSION_REPLY).
- Presence (PRESENCE).

**P2P DataChannels are used for: (a) complete-file transfer only, (b) nothing else in v1.** No "control" DataChannel is allocated; all control traffic is server-relayed over the WebSocket.

**19.4.1 Why not P2P for control.**

- **Authoritative permission enforcement.** The server is the single arbiter of "may this user issue PLAYBACK_CMD right now?". Putting that check on every P2P hop would mean every participant must be a permission enforcer, which is the kind of "client-side auth" we want to avoid (see draft 21).
- **Audit.** A server-relayed PLAYBACK_CMD is one append to the audit log and the host cannot dispute "I never issued that" (their signature is on it). A P2P message has no such anchor.
- **Presence consistency.** The server is the source of truth for "who is connected" (heartbeats; see draft 20). Fanning presence out P2P leads to eventual-consistency anomalies.
- **Reconnect simplicity.** A reconnecting client resumes by talking to the server, not by re-establishing N peer connections first.
- **Bandwidth is not the problem.** Control traffic is a few KB/s even with 8 participants and 120 Hz drawing; the WS pipe handles that with orders of magnitude of headroom on a 1 Gbps link.

This is deliberate and documented; do not "optimize" control traffic to P2P without revisiting the threat model in draft 21.

### 19.5 STUN and TURN

**19.5.1 STUN (always used).**

Public STUN servers, used in order:

- `stun:stun.l.google.com:19302`
- `stun:stun.cloudflare.com:3478`
- `stun:stun.nextcloud.com:3478`

For ~80% of participants on the public Internet, STUN alone is enough to discover the public IP/port mapping. We do not need our own STUN for v1.

**19.5.2 TURN (fallback, self-hosted coturn).**

For symmetric NATs (corporate firewalls, some mobile carriers), STUN fails and we need a TURN relay. v1 deploys a self-hosted **coturn** server alongside the WS server.

- TURN credentials are **short-lived** (TTL 1 hour) and **per-session**.
- The server mints a TURN credential on `ROOM_JOINED` and includes it in the participant object:
  ```
  p2p: { candidate_user_ids: [...], turn: { urls: ["turn:turn.locast.local:3478?transport=udp","turns:turn.locast.local:5349?transport=tcp"], username: string, credential: string, ttl_s: 3600 } }
  ```
- The credential is `username = "<unix_ts>:<user_id>"` and `credential = HMAC-SHA256(TURN_SECRET, username)` (coturn's `use-auth-secret` mode). The `TURN_SECRET` is a server-only config; clients never see it.
- We do **not** support third-party TURN providers in v1 (Twilio, etc.) to keep the operational story simple. The coturn instance is the operator's responsibility.
- TURN bandwidth counts against the 1 Gbps aggregate signaling cap (draft 20 section 20.6) since it is signaling-adjacent.

**19.5.3 ICE transport policy per peer.**

- Default: `iceTransportPolicy: "all"` (direct STUN/ICE preferred; TURN used only when ICE connectivity checks fail).
- For known-restrictive networks (operator-configured allowlist), an optional `iceTransportPolicy: "relay"` can be set per-room; this forces TURN. We do not implement this in v1 (it is a v1.5 flag on ROOM_CREATE).

TURN bandwidth costs are an operator concern; the server logs TURN usage (e.g. via `locast_turn_relay_bytes_total`) for capacity planning.

### 19.6 The chunked-file DataChannel

A single ordered reliable DataChannel labeled `files` per peer connection. We use one DC for all in-flight transfers with that peer (not one DC per transfer) to avoid the per-DC overhead of `datachannel` events and SCTP associations.

**19.6.1 Channel properties.**

- `ordered: true`
- `reliable: true` (the only mode in modern browsers; SCTP is reliable by default for DataChannels)
- `protocol: "locast-files-v1"`
- `negotiated: false` (one side creates; the other accepts in `ondatachannel`)

The SCTP `maxMessageSize` is left at default (typically 256 KiB at the SCTP layer, fragmented to 16 KiB chunks over the wire). Our DOWNLOAD_CHUNK payloads are designed to fit within this.

**19.6.2 Backpressure.**

The DC has a built-in buffer (`dc.bufferedAmount`). The source peer uses `bufferedAmountLowThreshold = 256 KiB` and the `onbufferedamountlow` event to pace sends. The protocol's per-message chunk size is 256 KiB by default, which keeps the high-water mark at "one outstanding chunk above the threshold".

```
const HIGH = 1 * 1024 * 1024;          // pause above 1 MiB buffered
const LOW  = 256 * 1024;               // resume below 256 KiB

dc.bufferedAmountLowThreshold = LOW;
let paused = false;
dc.onbufferedamountlow = () => {
  paused = false;
  pump();
};
function pump() {
  while (!paused && hasMoreChunks()) {
    const chunk = nextChunk();
    dc.send(encodeMessage(chunk));
    if (dc.bufferedAmount > HIGH) { paused = true; break; }
  }
}
```

The viewer applies the same backpressure on its `DOWNLOAD_ACK`s: it only emits an ACK after it has fully written the chunk to disk and updated the bitmap. This is the only reliable flow control we have; we do not implement BBR or similar.

**19.6.3 Transfer lifecycle (per `transfer_id`).**

1. **Offer.** Source -> viewer: `DOWNLOAD_OFFER{ media_id, sha256, size, chunk_size, chunks, transfer_id, via: "p2p" }` over the DataChannel.
2. **Accept.** Viewer validates `sha256` against the signed manifest (refuses if mismatched). Viewer allocates a partial file, records `downloads` row with status `in_progress`, and starts requesting chunks in order (sequential) or with a small window of N=4 in-order outstanding requests. v1 ships sequential; windowed is v1.1.
3. **Chunk.** Source sends `DOWNLOAD_CHUNK{ transfer_id, index, hash, data, sig }`. Viewer verifies: per-chunk `hash` (sha256 of `data`) and per-chunk `sig` (Ed25519 over `transfer_id || index || hash || data`). Mismatch -> NAK up to 5 times then fail.
4. **Ack.** Viewer writes chunk to disk, fsyncs every 16 chunks, updates `download_chunks` bitmap, sends `DOWNLOAD_ACK{ transfer_id, index }`.
5. **Complete.** When all chunks ACKed, viewer computes final `blake3` of the assembled file, compares to manifest's `sha256` (or `blake3` if the manifest records it; v1 uses sha256). On match, atomic rename `*.partial` -> final, status `complete`, and `DOWNLOAD_COMPLETE` to the server.
6. **Fail.** Either side may send `DOWNLOAD_FAIL{ transfer_id, reason }`. The server logs and the viewer reverts the partial.

**19.6.4 Hash and signature on chunks.**

Each DOWNLOAD_CHUNK carries:

- `hash`: sha256 of `data` (32 bytes), so a corrupted chunk is detected even if SCTP somehow delivered a wrong byte.
- `sig`: Ed25519 signature of `transfer_id || index || hash || data` by the source peer's pubkey (recorded in DOWNLOAD_OFFER). The viewer verifies before writing to disk.

Why both? `hash` catches transport corruption; `sig` catches a malicious source peer substituting chunks from a different file with the same intended content (defense in depth; the host-signed manifest is the primary guarantee, but the per-chunk sig means even the source's collaborator cannot poison the download).

**19.6.5 Failure handling.**

- NAK budget: 5 NAKs per chunk. Exceeding -> `DOWNLOAD_FAIL("too_many_naks")`.
- Stall timeout: if no chunk or ack within 30 s on either side, `DOWNLOAD_FAIL("timeout")`.
- Viewer's host disconnect: source pauses sends, viewer pauses ACKs; on host return, downloads resume.
- Viewer's own disconnect: see draft 22; the chunk bitmap is persisted in SQLite, the transfer is paused, on reconnect the viewer sends `DOWNLOAD_RESUME{ transfer_id, resume_index }` to the new source (which may be a different peer if the original source is gone).

### 19.7 What runs on the server vs. P2P - summary table

| Concern | Transport | Authority |
|---|---|---|
| Room lifecycle (create/join/leave) | WS | server |
| Manifest publish/update | WS | server (host signs) |
| Playback commands | WS | server (enforces host cap) |
| Drawing | WS | server (enforces draw cap) |
| Laser | WS | server (enforces laser cap) |
| Chat | WS | server |
| Permissions | WS | server (host + server policy) |
| Presence | WS | server |
| SDP/ICE relay | WS | server (pure relay, no inspection) |
| File chunk transfer | WebRTC DC "files" | host's signed manifest |
| Optional chat (v1.5) | WebRTC DC "chat" | (not used in v1) |


## 20. Server Architecture

### 20.1 Scope

This section describes the Locast server: a single Rust binary, the components it contains, the data it persists, the limits it enforces, and the deployment story. The server is **authoritative for control, presence, and permissions**; it **never carries media bytes** (a hard rule, see draft 01 section 1.3 and the threat model in 20.9).

### 20.2 Runtime

- **Language:** Rust (stable, 1.78+).
- **Async runtime:** tokio (multi-thread runtime; default 1 worker per physical core; configurable via `LOCAST_WORKERS`).
- **HTTP framework:** axum 0.7 (used both for `/ws` upgrade and REST endpoints).
- **WebSocket:** `axum::extract::ws` with `tungstenite` underneath; we use the `Message::Binary` frame for MessagePack envelopes and `Message::Ping`/`Message::Pong` for keepalive.
- **Database:** SQLite for v1 (single-node, file-backed at `./var/locast.db`); Postgres is a v1.5 option for multi-node deployments (see 20.10). Migrations via `sqlx migrate` checked into `migrations/`.
- **Configuration:** env vars + a TOML file (`./config/server.toml`); no runtime config reload in v1 (restart to apply).
- **Metrics:** `prometheus` crate, exposed on `/metrics`.
- **Logging:** `tracing` + `tracing-subscriber` with JSON output; log levels via `RUST_LOG`.

### 20.3 Endpoints

| Method | Path | Purpose |
|---|---|---|
| GET | `/health` | liveness; returns `200 {"status":"ok"}` if the event loop is responsive |
| GET | `/version` | returns `{ name, version, git_sha, protocol_v }` |
| GET | `/metrics` | Prometheus exposition |
| GET | `/ws` | WebSocket upgrade; the only way clients interact |
| GET | `/rooms/:code/info` | unauthenticated metadata: exists, title, participant count, locked; never returns participant user_ids |

**No** REST endpoint accepts commands, returns media metadata, or returns participant identities without a session. The `/rooms/:code/info` endpoint is the one exception and is deliberately minimal (does not leak `user_id` or `pubkey`).

### 20.4 Connection lifecycle (per WS)

Each accepted WebSocket spawns a single `Connection` task with two halves:

**20.4.1 Read pump.**

- Reads `Message::Binary` frames up to 1 MiB (axum's `MaxMessageSize`).
- Decodes MessagePack; rejects on decode failure with `ERROR("bad_msg")` and closes the connection after 3 such failures in 60 s.
- Dispatches to the `dispatch` state machine in the same task (no cross-task message passing for protocol messages; only for room-broadcasts which go through a `tokio::mpsc` to the room's writer).
- Tracks per-connection token bucket (18.6) and per-connection seq.

**20.4.2 Write pump.**

- A `tokio::mpsc::UnboundedSender<Bytes>` per connection; the read pump and the room's writer both push to it.
- Frames each outbound message as `Message::Binary`.
- Applies backpressure: if the channel's pending count exceeds 1024 messages or 8 MiB, the connection is marked slow and shed (closes with code 1011, "slow consumer").

**20.4.3 Heartbeat.**

- Server sends WS `Ping` every **15 s**.
- Expects WS `Pong` within **60 s** of the last `Ping` (i.e. 4 missed pings = disconnect). On timeout, the read pump is dropped, the connection is closed, and the participant is marked `disconnected` (see 22.5).
- Pong handling is done in the read pump via the `Message::Pong` variant; we do not need application-level heartbeat messages on top of WS ping/pong.

**20.4.4 Authentication.**

- On `HELLO` the server sends `WELCOME` and a 32-byte `CHALLENGE` nonce with a 30 s expiry.
- Client signs the nonce with their Ed25519 key, returns `AUTH{ user_id, pubkey, sig }` (not part of the public protocol types in 18.4; this is the internal handshake; v1 may move this into the public types).
- Server verifies; on success, generates a 32-byte bearer token (via `rand::thread_rng`), stores `{ user_id, pubkey, bearer_hash, expires_ms }` in SQLite, returns `AUTH_OK{ token, expires_ms }`.
- Bearer tokens are required for **all** subsequent messages (a v1.1 consideration; v0 may allow pubkey-signed messages only - deferred).
- Refresh: client may send `AUTH_REFRESH{ bearer }` within 60 s of expiry; server issues a new token. Refresh tokens are the same shape; no separate long-lived token exists.

### 20.5 Room registry

**20.5.1 In-memory state.**

```rust
struct RoomState {
    id: RoomId,
    code: String,             // 6 chars, unambiguous alphabet
    title: String,
    host_user_id: UserId,
    password_hash: Option<Argon2idHash>,
    created_ms: i64,
    last_activity_ms: i64,
    participants: HashMap<UserId, Participant>,
    capabilities_default: CapSet,
    capabilities_host: CapSet,
    manifest: Option<SignedManifest>,
    recent_msgs: RingBuffer<(user_id, seq)>, // last 32 for dedup
}

type RoomRegistry = Arc<DashMap<RoomCode, Arc<RwLock<RoomState>>>>;
```

- `DashMap` for the outer map; per-room `RwLock` for the inner state.
- Read-mostly path (relay) takes a read lock; write paths (join/leave/manifest) take a write lock.
- Room cleanup: rooms with no participants for 60 s are removed from memory; their DB row remains (history).

**20.5.2 Persistence.**

The following are persisted to SQLite:

- `rooms` table: id, code, title, host_user_id, password_hash, created_ms, ended_ms, last_manifest_id.
- `participants` table: room_id, user_id, joined_ms, left_ms, cap_set.
- `server_events` table: append-only audit log (20.8).
- `media_index` table: optional, denormalized last-known manifest for fast `/rooms/:code/info` queries (otherwise that endpoint reads the manifest from the `manifests` table).

The live `RoomState` is **not** persisted on every change. On crash recovery, the server reads the most recent `server_events` and rebuilds the in-memory state. This is acceptable because the only state that matters at runtime is who's currently in a room, and that information is also derivable from the most recent presence events.

### 20.6 Limits and capacity

| Limit | Value | Notes |
|---|---|---|
| Max participants per room | 8 | hard; enforced at ROOM_JOIN |
| Max concurrent rooms per node | 50 | soft; tunable |
| Max WS connections per node | 800 | soft; tunable (8 conn/room * 50 + buffer) |
| Aggregate signaling bandwidth | 1 Gbps | soft; measured via `/metrics` |
| Per-room msg rate | 1000 msg/s | aggregate, RATE_LIMIT applied |
| Per-conn msg rate | 100 msg/s, 200 burst | token bucket |
| Per-conn bytes rate | 1 MB/s, 2 MB burst | token bucket |
| DRAW_POINT rate per user | 120 Hz | token bucket |
| Single message size | 8 KiB (default) | 64 KiB SIGNAL, 1 MiB DOWNLOAD_CHUNK |
| WS frame size | 1 MiB | hard ceiling; close 1009 if exceeded |
| Heartbeat ping | 15 s | server-initiated |
| Heartbeat pong timeout | 60 s | 4 missed pings |
| Stale participant removal | 5 min after DISCONNECTED | see 22.5 |
| Database | SQLite (WAL mode) | single writer, many readers |

When any of these limits is approached (>80%), the server logs a `WARN` and increments a `locast_limit_*` Prometheus counter. At >95% the server applies shedding (refuses new ROOM_CREATE; existing rooms continue).

### 20.7 Presence

- Client sends `PRESENCE{ status, ... }` every **5 s** while connected, and additionally as a state change (e.g. `away`).
- The server records `last_seen_ms[user_id]` per room.
- **3 missed PRESENCE windows (15 s) = DISCONNECTED.** The participant is marked `disconnected` in memory and `PEER_LEAVE` is broadcast.
- After 5 min in `disconnected`, the participant is removed from the room (draft 22 section 22.5).
- If the same `user_id` reconnects within the 5 min window with a valid bearer, the server reinstates them in the same room slot (capabilities preserved) and broadcasts `PEER_ANNOUNCE` again.

### 20.8 Rate limiter and capability enforcement

- **Rate limiter:** token bucket per connection (see 20.6). Implemented in-process; no external store. On exceed, the connection receives `RATE_LIMIT` and the message is dropped. The server does **not** disconnect for rate-limit hits; the offender is throttled for 1 s.
- **Capability enforcement:** every command message passes through `check_capability(sender, room, command)` before the server will relay or apply it. The capability map is defined in draft 05 section 5.4. Examples:
  - `ROOM_END` requires `cap_manage_room`.
  - `PLAYBACK_CMD` requires `cap_playback_control` (host has this by default; the host can delegate it).
  - `DRAW_*` requires `cap_draw`.
  - `LASER_*` requires `cap_laser`.
  - `DOWNLOAD_REQ` is implicitly allowed for any room member.
  - `PERMISSION_SET` requires `cap_manage_room`.
  - `MANIFEST_PUBLISH` requires `cap_publish_manifest` (host only in v1).

Violations yield `ERROR("forbidden")` to the sender only; the message is not relayed.

### 20.9 Threat model

**20.9.1 What the server is trusted to do.**

- Enforce capabilities correctly.
- Maintain accurate presence.
- Not censor messages (we are not building a moderation system in v1; no message is dropped for content reasons; rate limits are the only exception).
- Not selectively deny service to a particular user beyond ban flags.
- Mint TURN credentials and not collude to recover their HMAC.
- Sign nothing on behalf of users.

**20.9.2 What the server is NOT trusted to do.**

- **Media integrity.** The server does not validate chunk bytes; the host-signed manifest is the source of truth, and the client refuses any file whose final hash does not match the manifest. A malicious server could refuse to relay DOWNLOAD_OFFER, but cannot substitute bytes that pass the manifest hash check.
- **Clock.** Clients use `server_ts_ms` only as a reference; the authoritative clock for ordering is per-sender `seq`.
- **Routing decisions affecting correctness.** The server can refuse to relay (which is a denial of service, not a correctness violation); it cannot inject a message that the client will accept as authentic, because every message is sender-signed.

**20.9.3 Why not P2P-only for control.**

We considered running rooms as pure P2P with the server only as a signaling bootstrap. We rejected this for v1 because:

- **Permissions would be enforced by N peers, not one.** Every client would need to verify the host's signature on every command. A bug in one client's enforcement is exploitable.
- **Audit would be unreliable.** A peer can claim "I never received that PLAYBACK_CMD" with no server log to refute it.
- **Presence would be eventually consistent.** Disagreement among peers about "who is connected" is a real source of bugs.
- **Bootstrap is not free anyway.** The server must hand out TURN credentials, mint bearer tokens, and serve `/rooms/:code/info` for clients that just want to find a room. So we have a server. We might as well use it.

In v1.5 we may move control traffic P2P for a "low-trust" deployment mode; the WS path remains available for those who want server authority.

### 20.10 Deployment

**20.10.1 Single Docker image.**

```
FROM rust:1.78-slim AS builder
...
FROM debian:bookworm-slim
COPY --from=builder /app/locast-server /usr/local/bin/
COPY config/server.toml /etc/locast/server.toml
EXPOSE 443
ENTRYPOINT ["/usr/local/bin/locast-server"]
```

- Single static binary; ~30 MB compressed.
- Reverse-proxied by Caddy for TLS (Caddy handles cert via Let's Encrypt; we do not implement ACME in the Rust binary).
- The coturn instance is a separate container; the server mints credentials for it.

**20.10.2 Stateless horizontal scaling (v1.5, optional).**

For v1 we deploy a single node. The architecture allows horizontal scaling via a Redis pub/sub fan-out in v1.5:

- Each node subscribes to `locast/room/<room_id>` on connect.
- Local in-memory state holds participants currently connected to this node.
- A room's participants may be split across N nodes; broadcasts go through Redis.
- SQLite is replaced with Postgres for the multi-node case.

We are not building this in v1. The single-node 800-connection / 50-room / 1 Gbps ceiling is enough for our target usage in v1.

### 20.11 Audit log

Append-only `server_events` table:

```
CREATE TABLE server_events (
    id           INTEGER PRIMARY KEY,
    ts_ms        INTEGER NOT NULL,
    node_id      TEXT NOT NULL,
    room_id      BLOB,
    user_id      BLOB,
    type         TEXT NOT NULL,    -- ROOM_CREATED, ROOM_JOINED, PLAYBACK_CMD, etc.
    ref_id       BLOB,             -- message id, if applicable
    payload_json TEXT,             -- redacted; see 20.11.1
    sig_chain    BLOB              -- optional: signed batch for tamper-evidence
);
```

- Retention is operator-tunable; default 90 days, max 365 days. Older rows are pruned by a background task at 03:00 local time.
- 20.11.1: `payload_json` is a redacted view of the message payload; signatures, raw bytes, and any field marked `sensitive` are omitted. We never log bearer tokens, password hashes, raw media paths, or display names longer than 32 chars (truncated).

### 20.12 Metrics (Prometheus)

Exposed on `GET /metrics`:

- `locast_connections_total` (counter)
- `locast_connections_active` (gauge)
- `locast_rooms_active` (gauge)
- `locast_messages_in_total{type}` (counter)
- `locast_messages_out_total{type}` (counter)
- `locast_bytes_in_total`, `locast_bytes_out_total`
- `locast_rate_limit_drops_total{scope}`
- `locast_forbidden_total{type}`
- `locast_p2p_signal_inflight` (gauge)
- `locast_db_query_seconds{query}` (histogram)
- `locast_audit_events_total{type}`

Default scrape interval target: 15 s.

### 20.13 Operational concerns

- **Backups:** SQLite is copied to S3 (or local equivalent) every 6 hours via `sqlite3 .backup`. The `server_events` and `media_index` tables are the priority.
- **Migrations:** `sqlx migrate run` at startup; migrations are forward-only; no down-migrations in production.
- **Graceful shutdown:** SIGTERM -> stop accepting new connections -> send `ERROR("server_shutdown")` to all clients -> close after 5 s.
- **Restart:** client WS reconnect logic (draft 22 section 22.3) covers this transparently.


## 21. Security Model

### 21.1 Threat surface

Locast's security posture is built around the following assumptions:

- **The server is semi-trusted.** It enforces permissions and presence correctly, but is **not** trusted for media integrity or for the confidentiality of media bytes (which it never sees).
- **Other room participants are untrusted.** Any participant may be malicious. The host is trusted only to the extent that their manifest signature is verified; they are not trusted to not act in bad faith with regard to other clients (e.g. serving a different file than the manifest claims is impossible because the manifest is signed and chunks are hash-checked).
- **The local filesystem and OS are trusted.** The library directory is user-owned; no other process tampers with it (we are not defending against local malware).
- **The network is hostile.** MITM, packet injection, eavesdropping, replay are all in scope. TLS is mandatory on every transport.

### 21.2 Room-code security

- **Alphabet:** unambiguous, 32 characters: `ABCDEFGHJKLMNPQRSTUVWXYZ23456789` (no `0`, `O`, `1`, `I`, `L`). Excludes easily-confused characters.
- **Length:** 6 characters. Space = 32^6 = **1,073,741,824** (~1 B). At the v1 limit of 50 concurrent rooms, collision probability is negligible; the server still rejects duplicates.
- **Distribution:** users share the code out-of-band (DM, voice, etc.); no server-side "search for room" feature exists in v1.
- **Rate limit on join attempts:**
  - 5 attempts per minute per source IP, per room code.
  - On exceed: 1-minute cooldown.
  - On 3 consecutive cooldowns: 1-hour cooldown.
  - On 5 consecutive cooldowns: 24-hour cooldown.
  - On 10 consecutive cooldowns: permanent IP ban (operator-overridable).
- **Optional password:** an `Argon2id` hash of a user-supplied password is stored server-side at ROOM_CREATE. The plaintext is never sent to the server over the wire **after** ROOM_CREATE (the client hashes client-side and the server hashes again with its own salt? No - we do it once: the client sends the password over WSS in the ROOM_JOIN_REQUEST, the server runs Argon2id and compares). The password is held in memory on the server only for the comparison (zeroized after); only the hash is persisted. Maximum password length: 128 chars; minimum: 8. We do not require complexity rules (NIST 800-63B); we require length.

### 21.3 Authentication

- **Algorithm:** Ed25519 (RFC 8032) for client identity.
- **Key generation:** the client's first launch generates a keypair in the OS keystore (Windows DPAPI, macOS Keychain, Linux libsecret). The private key never leaves the device.
- **Challenge:** on connect, server sends a 32-byte random nonce with a 30 s expiry. Client returns `AUTH{ user_id, pubkey, sig(nonce) }`. Server verifies with the `pubkey` (which becomes the canonical identifier for the user; `user_id` is just a UUID wrapper).
- **Bearer tokens:** on success, server issues a 32-byte bearer token with a **15 minute** TTL. The token's `sha256` is stored in SQLite; the plaintext is only ever held in the client's memory.
- **Refresh:** the client may send `AUTH_REFRESH{ bearer }` before expiry (within the last 60 s of the TTL). The server issues a new token; the old token is invalidated atomically.
- **Banned pubkeys:** a server-side `banned_pubkeys` table; any connect attempt from a banned key gets `AUTH_FAIL("banned")`.

### 21.4 Authorization

- **Capabilities** are an `enum set` per participant (see draft 05 section 5.4): `playback_control`, `draw`, `laser`, `manage_room`, `kick`, `publish_manifest`, `invite`, `chat`.
- The host's capability set is `playback_control | draw | laser | manage_room | publish_manifest | invite | chat` (everything) at ROOM_CREATE.
- The default participant set is `chat` only.
- The host may PERMISSION_SET any cap to any participant with an optional `expires_ms`. Permissions are server-validated and signed by the host's pubkey (already in the envelope).
- Every command is checked: `check_capability(sender, room, command_type) -> Result<(), Error>`. Failures are `ERROR("forbidden")` to the sender only (no relay).

### 21.5 Host impersonation

- The room's `host_user_id` is **bound at ROOM_CREATE** and signed by the creator. The server stores it in the `rooms` table.
- Any subsequent command that requires host authority (e.g. MANIFEST_PUBLISH) is checked against this `host_user_id`. The `user_id` in the message's `sender` must match.
- **Host transfer:** Not implemented in v1. A `HOST_TRANSFER` message is reserved for v1.1; the v1 way to change host is to ROOM_END and have someone else create a new room.
- A non-host cannot MANIFEST_PUBLISH or PERMISSION_SET, even with a spoofed envelope; the server verifies the signature against the declared `pubkey` in `sender` and against the room's `host_user_id`.

### 21.6 Reconnect authentication

- On disconnect, the client's local state includes a `bearer_token` and a `recent_nonce` (the last 32 bytes the server sent).
- On reconnect (within 15 min), the client sends `AUTH_RESUME{ user_id, bearer, signed_nonce }` where `signed_nonce` is the Ed25519 signature over the most recent server nonce. This proves possession of the private key without a full challenge round-trip.
- After 15 min of disconnect, the bearer expires and the client must re-authenticate with a fresh CHALLENGE.

### 21.7 Path traversal and malicious filenames

This is one of the highest-risk surfaces because it touches the user's filesystem.

- **Filenames are never used as paths.** The `locast://` URI scheme resolves an opaque `media_id` (UUID) to a record in `media_items`; the actual filesystem path is a server-validated `rel_path` under the library root. Clients never see raw paths in URLs.
- **`rel_path` validation (server-side, for any host-published manifest):**
  - Must be a relative path.
  - All path separators are forward slashes only; backslashes are rejected.
  - No `..` segments (rejected before normalization).
  - After normalization, the resolved path must lie inside the library root (`realpath` check). Any other resolution is rejected.
  - ASCII-only; non-ASCII characters are rejected (manifest is JSON/CBOR and should be ASCII; if non-ASCII sneaks in, the manifest is malformed).
  - No control characters, no NUL.
  - Not a Windows reserved name (`CON`, `PRN`, `AUX`, `NUL`, `COM1-9`, `LPT1-9`) at any segment.
  - Each segment <= 255 bytes; total path <= 4096 bytes.
  - Unicode-normalized to NFC.
- **Symlinks:** the library root is walked at startup; symlinks pointing outside the root are recorded in a `denied_paths` table and excluded from manifests.
- **Case sensitivity:** on Windows the filesystem is case-insensitive; the server canonicalizes to lowercase for comparison and refuses manifests that would produce case-conflicts.

### 21.8 Malformed protocol

- **Strict schemas:** every message is deserialized via `serde` with `#[serde(deny_unknown_fields)]`. Any field not in the schema is a protocol error and yields `ERROR("bad_msg")`; the message is dropped.
- **Type-checked enums:** `type` must be a known string; unknown types are rejected.
- **Range checks:** integers, lengths, and counts are bounds-checked at deserialization (e.g. `text: String` with `length_max = 2000`, `chunk_size` in the allowed set).
- **Oversized messages:** rejected at the WS layer (frame size) and at the app layer (per-type max; see 18.5). The 1 MiB WS frame cap closes the connection on violation.

### 21.9 Corrupted downloads

- **Per-chunk sha256:** every DOWNLOAD_CHUNK's `data` is hashed client-side on receive and compared to the `hash` field. Mismatch -> NAK.
- **Final blake3:** the assembled file's `blake3` is compared to the manifest's `sha256` (we use sha256 for v1 manifest, but compute both sha256 and blake3 in flight; the manifest is the source of truth).
- **Retry budget:** up to 5 NAKs per chunk; on 6th failure the transfer is marked failed and a new DOWNLOAD_REQ is needed.
- **Source-of-truth signature:** the host's signature on the manifest binds the file's hash. The client cannot be tricked into accepting bytes that don't hash to the manifest value.

### 21.10 Replay protection

- **Per-sender seq:** monotonic u64; never reused. Server tracks `last_applied_seq[(room_id, user_id)]` and drops anything <= last applied.
- **Time window:** `ts_ms` older than 30 s (relative to `server_ts_ms`) is dropped. Exception: presence/position reports have a 5 s window.
- **Idempotency:** `id` (UUID v7) is recorded in `recent_msg_ids` for 10 min; duplicates are dropped even if `seq` is fresh.
- **No timestamp-only authentication:** `ts_ms` is informational; the `seq` and signature are the actual replay guards.

### 21.11 Drawing spam

- **Per-user 120 Hz cap:** token bucket (see 18.6). Excess points trigger `RATE_LIMIT` to the sender.
- **Server-side coalescing:** the server keeps the **last** point received within a 50 ms window for each `(room, user, stroke_id)` to avoid amplifying minor jitter.
- **Stroke limits:** no more than 1024 active strokes per room; no more than 4096 points per stroke. Excess triggers `DRAW_END` and a new `DRAW_BEGIN` is required.
- **Per-user backpressure:** the server records the time of the last DRAW_POINT per user; if more than 500 ms elapse between DRAW_BEGIN and the first point, the stroke is auto-closed.

### 21.12 TLS

- **Server:** requires HTTPS/WSS. The Rust binary does not terminate TLS in v1; a reverse proxy (Caddy) does. The proxy is configured with a real cert (Let's Encrypt) and HSTS (max-age 1 year, include subdomains, preload-eligible).
- **Client:** verifies the server's certificate against the system trust store; refuses to connect on cert error. There is no "ignore TLS errors" option in v1 (we are not enterprise-managed proxy-friendly; v1.5 may add CA pin override for operators).
- **TURN:** TLS (`turns:`) on port 5349; UDP TURN is unauthenticated by transport but uses short-lived HMAC credentials (see 19.5.2).
- **WebRTC:** DTLS-SRTP for any media (we don't have media, but the browser still negotiates DTLS); certificates are self-generated by the browser per RFC 8122.

### 21.13 Supply chain

- **`cargo-deny`** runs in CI on every PR; checks:
  - License allowlist (MIT, Apache-2.0, BSD-2, BSD-3, ISC, MPL-2.0, Unicode-DFS, Unicode-3.0, Zlib, OpenSSL, curl, libpng, no copyleft).
  - Advisory database (RustSec).
  - Banned crates list.
  - Source allowlist (only crates.io).
- **`npm audit`** (and `pnpm audit`) runs in CI for the web app; high severity blocks the PR.
- **SBOM:** generated at release time via `cargo-cyclonedx` and `@cyclonedx/cyclonedx-npm`. Stored as a release artifact.
- **Reproducible builds:** the Docker image is built with `--build-arg SOURCE_DATE_EPOCH=$(git log -1 --pretty=%ct)` to keep builds deterministic.

### 21.14 Logging

- We **never** log:
  - Bearer tokens (even hashed).
  - Passwords (plaintext or hashed).
  - Private key material.
  - Raw media paths (only `media_id` UUIDs).
  - Display names longer than 32 chars (truncated).
  - TURN credentials.
  - Ed25519 signatures (the inputs are logged, the signatures are not).
- We **do** log:
  - `user_id` (UUID).
  - Room codes.
  - Message types and counts.
  - Capability changes.
  - Errors with `request_id` and `user_id`.
  - IP addresses (with a 24-hour TTL, then truncated to /24 for IPv4 and /48 for IPv6; this is a privacy compromise that is debatable and may be removed in v1.5).
- Log retention: 30 days local, 365 days S3-archived, encrypted with KMS.

### 21.15 Disk hygiene

- **Library directory** must be user-owned and writable only by the user (mode 700 on Unix, ACL `BUILTIN\Users:(R,X)` denied on Windows).
- **Temp directory** is the OS default (`%TEMP%\locast` on Windows, `$TMPDIR/locast` on Unix). Files are written with mode 600.
- **Atomic writes:** every file is written to `*.partial` and renamed on success. No partial files are ever opened for playback (the manifest's `status` field is checked).
- **No writes outside the library root** by the application. We assert this in code: the file-write API takes a `library_root: &Path` and refuses to write outside.
- **Disk-space checks:** before download, the client verifies `available >= size + 100 MB` headroom. If not, the download is refused with a user-facing error.
- **Quota:** see draft 13 (deferred to media lifecycle).

### 21.16 Capabilities and trust matrix

| Capability | Required for | Held by default |
|---|---|---|
| `manage_room` | ROOM_END, HOST_TRANSFER, PERMISSION_SET, KICK | host only |
| `publish_manifest` | MANIFEST_PUBLISH, MANIFEST_UPDATE | host only |
| `playback_control` | PLAYBACK_CMD | host only |
| `draw` | DRAW_BEGIN, DRAW_POINT, DRAW_END, DRAW_UNDO, DRAW_CLEAR | host; grantable |
| `laser` | LASER_MOVE, LASER_OFF | host; grantable |
| `chat` | CHAT_SEND, CHAT_REACT | everyone |
| `invite` | (server-side: more permissive rate limit on ROOM_CREATE) | host only |

### 21.17 Hardening checklist (for v1 release)

- All env vars marked `sensitive` (TURN_SECRET, DB_KEY) are read once and zeroized.
- Compiled with `RUSTFLAGS="-C target-feature=+crt-static -C link-arg=-fstack-protector-strong"`; warnings are CI failures.
- No `unsafe` in our code (allow `unsafe` only in vendored deps, reviewed case-by-case).
- Stack overflow protection on; `cargo-audit` clean; `cargo-deny` clean.
- No panics on the server hot path (every `unwrap` in the WS layer is replaced with `expect` and a log; every `expect` is reviewed).
- The server binary runs as a non-root user inside the container.
- Resource limits via `cgroups`: 2 GB RAM cap, 1 CPU cap per container.


## 22. Reconnection and Failure Handling

### 22.1 Goals

Locast must be **robust to network failure** without compromising the "playback is local" property. The two failure classes are:

1. **Local file playback is decoupled from the network.** Once a file is fully present locally, playback continues regardless of network state. The `<video>` element reads the local `file://` URL; no buffering depends on a network round-trip.
2. **Downloads may pause, but must resume.** A partial download must be resumable from the exact byte boundary on reconnect, even after a process restart.

The reconnect story is the same whether the failure is a 2-second Wi-Fi blip, a 30-minute ISP outage, or a server restart.

### 22.2 Disconnect sources

The client has **four** independent connection surfaces:

- **WebSocket** to the server (control plane).
- **WebRTC DataChannel "files"** to each peer (chunk transfer).
- **Local SQLite** (always available; the source of truth for state).
- **Local media files** (the playback source).

A failure on the WS, on a peer's DataChannel, or on the server is recoverable from local state. A failure on the local filesystem is unrecoverable (and rare).

### 22.3 WS reconnect

**22.3.1 Backoff.**

- Schedule: 1 s, 2 s, 4 s, 8 s, 16 s, 30 s, then **30 s** sustained.
- Add jitter: +/-20% of the delay.
- Maximum: 30 s between attempts (we do not give up; the room is recoverable indefinitely as long as the server is up).
- The `ROOM_JOIN` re-issued on reconnect uses the **resume token** path (see 21.6) to land back in the same room with the same capabilities if the server has not yet pruned the participant.

**22.3.2 State preserved.**

The following is preserved locally and does not need a server round-trip to restore:

- The room code, host's `user_id`, and host's pubkey.
- The signed manifest.
- The chunk bitmap for each in-flight download.
- The local playback position (last seen `position_ms`).
- The local media files (the `media_items` table).

The following **is** lost and must be re-derived from the server:

- The current `last_seen_ms` of every other participant.
- The set of peers currently in `peers_alive` (P2P connections).
- Any drawing/laser state in flight (the server is the WebSocket relay for drawing/laser; on reconnect, the client requests a recent snapshot from the server, deferred to v1.1; v1 starts a fresh canvas on reconnect).

**22.3.3 P2P during WS outage.**

P2P DataChannels **remain up** during a WS outage as long as the underlying UDP path survives. Downloads continue. The client UI shows "WS reconnecting..." but does not pause transfers. If a chunk needs to be re-requested, the NAK goes over the still-open DataChannel.

If the WS stays down for >60 s and the user is mid-download, the client attempts to **switch the transfer's source peer** to one that is still WS-connected (any other room member can serve chunks). This is a v1.1 feature; v1 pauses the download until WS is back.

### 22.4 Reconnect during playback

- **No auto-seek.** When the WS reconnects, the local player is wherever the user's `<video>` element currently is; we do not attempt to "catch up" to the host's position.
- The user may click a "Sync to host" button (in the UI) which issues a `PLAYBACK_CMD{ action: "seek", position_ms: <host's last POSITION_REPORT> }` if the user has `playback_control` or if the host has explicitly enabled "follow host position" for non-controllers (v1.1; v1 only the controller can sync).
- During the WS outage, the user's playback continues; the host's position is irrelevant to the user's local file.

### 22.5 Stale participants

A participant's lifecycle on the server:

```
joining -> ready <-> reconnecting -> disconnected -> (removed)
```

- **`ready`:** heartbeat observed within 15 s (3 windows of 5 s).
- **`reconnecting`:** server has not heard from the participant in 15 s but the bearer has not yet expired; the participant is shown as "reconnecting" to other room members.
- **`disconnected`:** 30 s without a heartbeat; bearer is invalidated; PEER_LEAVE is broadcast.
- **Removed:** 5 min after `disconnected`; the row is deleted from `participants` and the room's in-memory state.

If the same `user_id` reconnects within 5 min, the server re-instates them in the same room slot, restores their capabilities, and broadcasts PEER_ANNOUNCE.

### 22.6 Host disconnect

The host is special. Behavior on host loss:

1. **30 s grace.** During this window the room continues; viewers see "host reconnecting...". No transfers pause unless they depend on the host.
2. **If the host returns within 30 s:** nothing changes. The host's `last_seen_ms` resumes.
3. **If the host does not return within 30 s:** the server ends the room. All viewers are notified (`ROOM_END{ reason: "host_left" }`) and returned to the library. Host transfer is explicitly deferred to v1.1.

In v1, host transfer is **not implemented**. The room ends when the host's grace expires. This is the simplest correct behavior and avoids a class of host-transfer race conditions.

### 22.7 Command dedup and ordering

- The server's `last_applied_seq[(room_id, user_id)]` drops out-of-order or duplicate commands before relay (see 18.7).
- The client tracks `last_applied_seq` locally for each remote user; out-of-order messages are reordered up to a window of 32; older are dropped with a log.
- A "duplicate" is "I have already applied this `seq`". The client does not need an exact-match dedup table; `seq` is sufficient.

### 22.8 Clock differences

- **Source of truth: `server_ts_ms`.** Each message carries the sender's `ts_ms` and the server stamps its own at relay time (an internal field, not on the wire).
- **Client skew estimation:** on each WELCOME, the client records `(local_ms, server_ts_ms)`. Over the first 60 s of the connection, it estimates `skew = median(server_ts - local_ts)` and applies it to local timestamps used in `POSITION_REPORT.client_ts_ms`.
- The skew estimate is **advisory** for the host's UI only; the authoritative ordering is `seq`, not time.

### 22.9 Idempotency

- All room state changes are idempotent given `(room_id, seq)`. The server can apply a ROOM_JOIN_REQUEST twice with the same `seq` and the result is the same.
- `media_items` upserts are by `media_id` (UUID); a duplicate insert is a no-op.
- `downloads` upserts are by `(transfer_id, index)`; a duplicate chunk is detected and dropped.

### 22.10 Failure summary table

| Failure | Local playback | Downloads | UI state | Recovery |
|---|---|---|---|---|
| WS 2-30 s blip | unaffected | continue over P2P | "reconnecting" | auto on backoff |
| WS > 30 s | unaffected | paused (v1) or switch source (v1.1) | "offline" | manual retry or server return |
| Server restart | unaffected | paused | "server unavailable" | auto on server back |
| P2P peer loss (one) | unaffected | paused for that source only; switch | "peer offline" | pick new source |
| P2P peer loss (all) | unaffected | paused; viewer can wait for host | "offline" | new P2P negotiation |
| Host disconnect | unaffected | paused (until host returns or new host) | "host reconnecting" | host returns or room ends |
| Process crash + relaunch | resumes from last state | resumes from chunk bitmap | "restoring..." | local SQLite is enough |
| Disk full | stops at next chunk write | paused | "out of space" | user frees space; resume |

## 23. Media Lifecycle (Temporary and Permanent)

The `media_items` table is the single source of truth for every media file Locast knows about, whether downloaded for a room or imported manually. Lifecycle is governed by a small set of state machines.

### 23.1 Media states

```
discovered -> (downloading) -> complete -> (used) -> (delete or keep)
                  \-> failed
                                  \-> temporary -> (keep) -> permanent
                                                  \-> (auto-cleanup) -> deleted
```

Field summary:

```
media_items {
  id              uuid pk,
  sha256          blob(32) unique,
  blake3          blob(32),
  size_bytes      u64,
  mime_type       text,
  rel_path        text,                  -- relative to library root
  origin          text,                  -- "download" | "import" | "scan" | "manual_add"
  status          text,                  -- "discovered" | "downloading" | "complete" | "failed" | "temporary" | "permanent" | "deleted"
  last_room_id    uuid,                  -- null if permanent or never in a room
  last_used_ms    i64,                   -- updated on open/play
  acquired_ms     i64,
  source_user_id  uuid,                  -- who provided the file (host)
  source_room_id  uuid
}
```

`status` is the user-visible concept; the implementation column is indexed.

### 23.2 Default state on download

When a viewer completes a download via the WebRTC DataChannel:

1. The chunked file is written to `<library>/.incoming/<media_id>.partial`.
2. On hash verification, it is renamed to `<library>/<rel_path>` (from the manifest). The `status` is set to **`temporary`**.
3. `last_room_id` is set to the room it was downloaded for.
4. `last_used_ms` is initialized to `acquired_ms`.

**Default state = `temporary`.** A file is only `permanent` if the user explicitly chose "Keep" (see 23.4) or imported it manually.

### 23.3 Recognized by hash (dedup)

- A download's `sha256` is computed at the **end** of transfer (we cannot compute it before; we receive it as part of the manifest).
- Before writing, we check `media_items` for an existing row with the same `sha256`:
  - If the existing row is `complete` or `permanent` and its `rel_path` file is present and matches the hash, we **use the existing file directly**: the new download is replaced with a **hardlink** (or copy-on-write reference) to the existing `rel_path`. No bytes are written twice. (On Windows we use `fsutil hardlink create` or fall back to a copy with `reflink`-style CoW if the FS supports it; for NTFS in v1 we fall back to a regular copy and accept the temporary disk-space cost. The hardlink path is preferred when both files are on the same volume.)
  - If the existing row is `temporary` and the file is present and matches, we **promote** it: leave the file in place, set `status = "permanent"`, copy the `acquired_ms` and `last_used_ms` from the existing row, and the new download completes immediately without re-fetching chunks.
  - If the existing row's file is missing (user deleted it manually), we fall through to the normal download path.

### 23.4 On room leave

When the user clicks "Leave room" (or is kicked, or the room ends), the client queries `media_items WHERE last_room_id = <room_id> AND status = 'temporary'` and shows a confirmation dialog:

```
You have 3 temporary files from room "Saturday Bible Study":
  - sermon-2026-08-23.mp4  (412 MB, downloaded 12 min ago)
  - slides.pdf              (8 MB, downloaded 12 min ago)
  - notes.txt               (2 KB, downloaded 12 min ago)

[ Delete all ]  [ Keep all ]  [ Cancel ]
```

Behavior:

- **Delete:** for each item, move the file to the OS trash (see 23.6), then `UPDATE media_items SET status = 'deleted'`. The DB row is kept for 30 days (so the user can recover by re-downloading without collision), then purged.
- **Keep:** for each item, `UPDATE media_items SET status = 'permanent', last_room_id = NULL`. The file is now indistinguishable from a manually-imported file. The dialog follows with "Add to library? [Yes (default) / No]"; "Yes" sets `last_used_ms = now` and indexes it for the library sidebar.
- **Cancel:** does not leave the room. The user must make a choice to leave. (v1: if the room is ending for external reasons, the dialog is forced before teardown.)

The dialog is per-room; if a user leaves room A and has temp files from room B, those are unaffected.

### 23.5 Auto-cleanup of stale temporary files

- Every 24 h (at app startup, plus on a 24h timer), the client runs:
  ```
  SELECT id, rel_path FROM media_items
   WHERE status = 'temporary'
     AND last_used_ms < (now_ms - 7 * 86400_000)
  ```
- For each, the client **schedules deletion in 24 h** and shows a notification: "3 temporary files will be deleted in 24 hours. [Review] [Delete now] [Keep]."
- After 24 h, the files are moved to the OS trash; DB rows go to `deleted` status and are purged 30 days later.
- A file that the user opens (double-clicks in the library, watches, or that is referenced by a new room's manifest within the 7-day window) has its `last_used_ms` updated to `now()` and is not subject to cleanup.

### 23.6 OS-specific deletion

- **Windows:** use `IFileOperation::DeleteItems` via the Windows API (or PowerShell's `Shell.Application` `Namespace.ParseName(folderItem).InvokeVerb("Delete")`). This sends the file to the Recycle Bin with original path metadata, allowing the user to restore.
- **macOS:** `NSWorkspace.activateFileViewerSelectingURLs` with `NSFileManager.trashItem(at:resultingItemURL:)` via the Swift bridge, or `osascript` with `tell application "Finder" to delete`. Sends to Trash.
- **Linux:** `gio trash <path>` (uses `org.freedesktop.Trash` via GIO; if GIO is unavailable, fall back to `mv` into `~/.local/share/Trash/files/`). The XDG Trash spec is followed.
- **Fallback:** if any of the above fails (e.g. headless Linux without GIO), the file is `unlink`-ed. The user is informed that the file could not be moved to trash.

### 23.7 Permanent files

- **Stored indefinitely** until the user explicitly deletes them via the library UI ("Delete from library" - same OS-trash mechanism).
- Recognized by `sha256` (unique index). A second download of the same hash reuses the existing file (23.3).
- Survives room end; `last_room_id` is `NULL` after promotion.
- The user can convert `temporary -> permanent` via:
  - The "Keep" button in the leave dialog (23.4).
  - A "Keep" button in the library context menu on a temporary item.
  - Right-click "Make permanent".
- Re-download avoidance: when joining a new room, the client pre-filters the host's manifest against `media_items WHERE sha256 IN (...) AND status IN ('complete','permanent') AND file_present()`. Matching items are marked "already have locally" in the download list; the user can choose to download anyway (e.g. to refresh a temp file) or skip. `file_present()` is a function that re-hashes a small prefix-and-suffix sample of the file at scan time and verifies the full hash on first access after a scan.

### 23.8 Quota

- The user sets a storage quota in Settings (default 50 GB, hard cap 4 TB, min 1 GB).
- The library size is computed on startup and refreshed every 10 min: `SELECT SUM(size_bytes) FROM media_items` (all statuses) **plus** the on-disk size of in-flight `.partial` files under `tmp/incomplete/` and `tmp/staging/`.
- The configured storage cap includes ALL of the following: (a) permanent media, (b) temporary media, (c) active and paused download staging files. Nothing is exempt.
- On download, the client checks `quota_used + size <= quota_max - 100 MB`. If not, the download is refused with a "Quota exceeded" error; the user must free space or raise the quota. The cap is checked atomically before each chunk fetch begins; a download that would exceed the cap is paused with a clear UI error.
- `temporary` files count against the **same** quota as `permanent` files. The auto-cleanup of stale temporaries (23.5) frees quota.

### 23.9 Re-download avoidance on join

When the client receives a `MANIFEST_PUBLISH` from a host, it processes each media item:

1. Compute the canonical local record (by `sha256`).
2. If `media_items.sha256 = X` exists, `status IN ('complete','permanent')`, and `file_present(X) == true`: mark the item as **"local"** in the UI. No download. The item is immediately available for the local player.
3. If `media_items.sha256 = X` exists with `status = 'temporary'` and `file_present(X) == true`: mark the item as **"local (temp)"**. No download. The user can promote it to permanent from the room UI.
4. If the file is missing or the hash mismatches: download as usual.
5. If the user wants to force re-download (e.g. the local copy is corrupt or they want a fresh copy), the library context menu offers "Re-download" which deletes the local record and triggers a new download.

### 23.10 Storage layout on disk

```
<library_root>/
  <rel_path>                       -- canonical content-addressed path
  .incoming/
    <media_id>.partial             -- in-flight downloads
  .trash/
    <timestamp>_<media_id>.partial -- soft-deleted (pre-purge window)
  .meta/
    thumbnails/
      <sha256>.jpg                 -- per-file thumbnail cache
    metadata/
      <sha256>.json                -- sidecar metadata
  locast.db                        -- SQLite
  locast.db-wal, locast.db-shm     -- WAL files
```

`rel_path` is content-addressed for the canonical library but may be overridden by the manifest's `rel_path` if provided. We use `<library_root>/<2-char-prefix>/<sha256>.<ext>` for content-addressed imports, and `<library_root>/<manifest.rel_path>` for room downloads (exact paths come from the manifest, validated per 21.7).

## 24. (Combined with section 23; see draft 12 Part B)

The originally planned section 24 ("permanent media details") was combined with section 23 by the drafting subagent because they share the same `media_items` table and state machine. See section 23.7 for permanent-file behavior.


## 25. UI / UX Structure

This section describes the React application's top-level structure for the Locast Tauri 2 client, including routes, page layouts, state management, keyboard interactions, and accessibility commitments. The UI exists to operate a binary whose non-negotiable rule is: a media file is not playable until it is complete on local disk. The UI must surface that state honestly at all times.

### 25.1 Top-Level Application

The client is a Tauri 2 desktop application that renders a single-page React app in a webview. The top-level shell is responsible for:

- Mounting the Tauri bridge and registering IPC event listeners.
- Providing the router, global stores, and React Query client.
- Rendering a persistent `AppShell` that contains the title bar region, the main route outlet, a status footer (connection, sync, drift), and modal/overlay portals.
- Loading persisted settings on boot and applying theme (system / light / dark) before first paint to avoid flash.
- Initializing background services: library watcher, presence listener, and the download scheduler.

The shell is intentionally thin. All domain logic lives in Rust and is reached through `tauri::invoke` (request/response) or emitted Tauri events (streams).

### 25.2 Routes

The router is a single React Router tree. All routes are local; there is no server-rendered HTML. Routes that are not yet unlocked (e.g. opening `/rooms/:id` before the manifest is locally complete) are guarded with a typed loader that returns a discriminated union `{ ok: true, room } | { ok: false, reason }`.

| Path | Purpose | Loader preconditions |
| --- | --- | --- |
| `/library` | Local media library (grid view, search, filter by permanent/temporary, sort) | Settings loaded; library dir configured |
| `/rooms` | List of active and recent rooms | None |
| `/rooms/new` | Host flow: pick media, pick subtitles, generate code | Library has at least one complete file |
| `/rooms/join` | Join by code | None |
| `/rooms/:id` | The watch room (the main view) | Manifest fetched and verified; every chunk present locally |
| `/settings` | Library dir, storage cap, temporary behavior, identity, network | None |
| `/downloads` | Active and historical downloads | None |

Default route is `/library`. Deep links to `/rooms/:id` from outside the app (e.g. a `locast://` URI) are accepted only when the room can be hydrated; otherwise the user is routed to `/rooms/join` with an explanatory toast.

### 25.3 `/rooms/:id` Layout

This is the primary view and is the most carefully designed. The layout is a CSS grid with named regions: `top`, `main`, `right`, `bottom`, `overlay`. The main region contains the `<video>` element and a drawing `<canvas>` overlay positioned absolutely on top of the video. The canvas is independent of the video element and is composited using `transform` and `will-change: transform` to keep pointer input responsive.

**25.3.1 Top bar.**

- Left: back arrow (returns to `/rooms` after a "Keep or Delete" prompt if the viewer is the host and the room is still open).
- Center: room code, formatted as `XXXX-XXXX`, with a copy-to-clipboard button that surfaces a transient confirmation.
- Right: room state badge (`inviting` / `syncing` / `playing` / `paused` / `ended`), a connection status indicator (LAN/WAN, latency, and a colored dot: green for direct, amber for relay, red for degraded), and a participant count.

**25.3.2 Main region.**

- `<video>` element with `playsInline`, `preload="auto"` only when the file is complete, `crossorigin="anonymous"` (irrelevant for `asset://` but kept for safety).
- A drawing `<canvas>` overlay sized to the video's intrinsic dimensions, scaled by `ResizeObserver` to match. The canvas is the source of truth for drawings; it does not manipulate the video element.
- A laser layer rendered as a separate absolutely positioned element so its cursor can be composited at the topmost z-index without invalidating the drawing canvas.
- A drift indicator that is hidden by default and only becomes visible when the smoothed offset between the local playback clock and the median participant clock exceeds 2.0 seconds. It shows the current drift in milliseconds and a "Resync" button that seeks to the median position.

**25.3.3 Right rail.**

- Tabbed: Participants and Chat.
- Participant list: avatar, display name, connection quality bar, and a small status icon (host, drawing, speaking via system audio if available).
- Chat: chronological list, input at the bottom, supports text and a single reaction emoji per message. The right rail is collapsible to a 32-pixel handle pinned to the right edge to maximize video area.

**25.3.4 Bottom transport.**

- Play/pause toggle.
- Seek bar with buffered ranges painted underneath the played range, and a thin marker for the median participant position. The bar shows `(mm:ss / mm:ss)` and the local time offset vs. the room clock.
- Volume slider and a mute toggle.
- Track selectors: audio track, subtitle track, and quality (informational; v1 is single quality).
- Local-only controls, visually separated by a divider: download progress for any non-complete media (only relevant when the viewer joined mid-room), "Remove local copy" (temporary only), and "Open source folder".

**25.3.5 Floating elements.**

- Laser pointer layer (toggled by the `l` key or the laser button). The layer is always mounted but its `pointer-events` is `none` unless the laser is active.
- Drawing toolbar (toggleable by the `d` key). It contains tool selection (pen, eraser, arrow, rectangle, freehand), color, stroke width, and an "Undo last" action scoped to the local user.
- Drift indicator (only when smoothed offset > 2.0 s, as above).

**25.3.6 Modals.**

Modals are rendered through a single portal and trap focus. Each has a typed backing store and a typed close reason.

- `InviteModal` - copyable code, copyable URL (`locast://room/<id>`), and a QR code.
- `PermissionsModal` - microphone/camera are out of scope for v1, but screen-share and clipboard are surfaced here if available.
- `ManifestConfirmationModal` - shown to the host before generating a room: file, size, hash, subtitle list, expected disk usage, and a "Start" button. The room cannot be created without explicit confirmation.
- `DownloadProgressModal` - non-dismissable while a media file is downloading because the file is required to be complete before playback. Shows per-file progress, aggregate throughput, ETA, and a "Pause" button. The modal blocks route transitions to `/rooms/:id` for that room.
- `YouAreBehindModal` - shown when the local media is complete but the participant is more than 10 seconds behind the median clock. Offers "Resync (seek to room position)" or "Keep my position".
- `LeaveRoomModal` - shown on close/leave. If the user is host and the room is still open, prompts for "Keep room open" or "End room". If the local file is temporary, prompts "Keep local copy" or "Delete local copy".

### 25.4 State Management

Zustand is chosen as the primary client store. It is small, has no provider boilerplate, supports selectors and shallow equality, and integrates cleanly with React 18 concurrent rendering. Redux Toolkit is unnecessary at this scale; the per-store surface is small and the state is mostly UI. Server data (rooms list, presence, manifest fetch progress) is handled with React Query because it provides caching, invalidation, retry, and background refresh for free.

**25.4.1 Stores.**

Each store is a single file under `src/stores/` exporting a hook (`useXStore`) and a typed selector. Stores do not import from each other directly; cross-store coordination is done in hooks.

`useRoomStore`
- The currently joined room id, the manifest reference, the local playback state (playing, position, rate), the smoothed clock offset, the participants map, the drawings layer (keyed by stroke id), and the laser pointer state.
- Actions: `joinRoom`, `leaveRoom`, `applyManifest`, `applyPlaybackEvent`, `addDrawingStroke`, `undoLastStroke`, `setLaser`, `setOffset`.
- Persistence: ephemeral only. On leave, the store is reset.

`useDownloadStore`
- The full download queue, per-task progress (bytes, throughput, ETA, source peer, attempt count), and aggregate stats.
- Actions: `enqueue`, `cancel`, `pause`, `resume`, `prioritize`. The store is hydrated from the SQLite download ledger on boot.
- Persistence: a thin mirror in SQLite; the store is the source of truth for UI, the DB is the durable record.

`useMediaStore`
- The library list (id, path, size, duration, kind, permanent/temporary, hash, complete flag, added timestamp, last played timestamp).
- Actions: `refresh`, `setPermanent`, `requestDelete`, `applyScanResult`. Backed by SQLite; the store mirrors the DB for fast rendering.
- Selection state for batch operations lives here too.

`useSettingsStore`
- All user settings: library dir, storage cap, default temporary behavior, identity (display name, color), network (TURN servers, max concurrent transfers, chunk size override), UI (theme, reduced motion), and accessibility (subtitle size, color, edge style).
- Persistence: mirrored to `tauri-plugin-store` and rehydrated on boot.

`useUIStore`
- Cross-cutting UI state: which modal is open, right rail tab, drawing toolbar visibility, laser active flag (mirror of `useRoomStore` for the floating layer), and the toasts queue.
- Persistence: none.

**25.4.2 Server data (React Query).**

- `useRoomsQuery` - list of active and recent rooms, polled with backoff.
- `usePresenceQuery` - keyed by room id; refetched on visibility and on participant count change.
- `useManifestQuery` - keyed by room id; cached, with `staleTime` of 5 minutes and a manual invalidation on room events.

Mutations are also React Query when they call the server, and a thin wrapper around `tauri::invoke` when they call local Rust. Errors are typed and surface as toasts.

### 25.5 Keyboard Shortcuts

Shortcuts are scoped to the room view unless noted. The shortcut service is a single registry that listens on `window` keydown and dispatches to the active scope. The scope is set by the router and by the currently focused element. All shortcuts are case-insensitive and ignore modifier keys unless explicitly listed.

| Key | Action | Scope |
| --- | --- | --- |
| `Space` | Play / pause | Room |
| `f` | Toggle fullscreen on the video element | Room |
| `m` | Mute / unmute local audio output | Room |
| `j` / `k` / `l` | Skip back 10s / pause / skip forward 10s | Room |
| `c` | Toggle chat focus (focus the chat input or unfocus) | Room |
| `d` | Toggle drawing toolbar | Room |
| `l` (lowercase L) | Toggle laser pointer (3s auto-release) | Room |
| `Esc` | Exit fullscreen, close top-most modal, or leave drawing mode | Global |
| `/` | Focus library search | `/library` |
| `?` | Open keyboard cheatsheet | Global |

When the user is typing in the chat input, drawing toolbar inputs, or any form field, shortcuts are suppressed.

### 25.6 Accessibility

Accessibility is a first-class requirement, not a polish step.

- Full keyboard reachability for every interactive control. Tab order is logical and follows the visual order. Focus rings are visible against both light and dark themes.
- ARIA roles and properties on the transport, the chat list, the drawing toolbar, the participant list, and all modals. Live regions announce room state changes (joins, leaves, drift) and download progress milestones at a measured cadence (no more than once per 5 seconds).
- Subtitle styling respects `prefers-reduced-motion`: when set, subtitles do not animate; they appear and disappear with an instant transition. The subtitle color, background opacity, and size are user-controlled in settings.
- Color contrast targets WCAG AA across the default themes. The drawing palette and the laser color are chosen to remain distinguishable for the three most common forms of color vision deficiency; the drift indicator uses an icon plus a number, not color alone.
- The video element exposes `controls` only when keyboard-only mode is detected; otherwise the custom transport is used. Captions are toggled via the `c`+`t` chord and via the transport.
- The download progress modal is announced as a critical live region because the file must be complete before playback; the announcement is configurable in settings and is off by default for users who have set `prefers-reduced-motion` to `reduce`.
- The application is tested with screen readers (NVDA on Windows, VoiceOver on macOS) as part of the manual checklist in section 27.

### 25.7 Routing and Navigation Rules

- The user cannot navigate to `/rooms/:id` for a room whose media is not locally complete. Attempting to do so either through deep link or by completing a download during navigation routes to the `DownloadProgressModal` and remains on the previous page.
- Leaving a room always returns to `/rooms`. The "Keep or Delete" prompt runs before the route transitions.
- Settings and library navigation never require a reload; navigation is purely client-side.

### 25.8 Visual Language

The visual language is deliberately neutral and media-first. The room view is dark by default with a single accent color driven by the user's identity color. The library is light or dark based on the system theme. The drawing canvas uses a small, curated palette; custom palettes are a v2 candidate. Typography is a single system stack for v1 to keep the binary small; a custom font is a v2 candidate.


## 26. Suggested Project / Repository Structure

This section specifies a monorepo layout for Locast. The goal is to keep the boundaries between the desktop client, the signaling/server, and the protocol shared by both explicit and enforceable by the toolchain (workspaces, lint, format, test). The structure is implementation-ready: every directory has a stated purpose, and the recommended toolchain choices below are justified against the alternatives.

### 26.1 Top-Level Layout

The repository is a single Git repository with a `pnpm` workspace at the root and a Cargo workspace that spans the Rust crates. The pnpm workspace owns the TypeScript side; the Cargo workspace owns the Rust side. The two are decoupled by the `shared/protocol` crate/package, which is the only place where types cross the language boundary.

```
locast/
  apps/
    client/
      src-tauri/        # Rust for the Tauri 2 app
      src/              # React app
    server/             # Rust signaling + room coordination
  shared/
    protocol/           # MessagePack schemas + generated TS and Rust types
    crypto/             # Ed25519, blake3, manifest signing/verification
    manifest/           # Manifest data model + serde implementations
  docs/
    ARCHITECTURE.md     # This document
    ROADMAP.md
  scripts/              # Local dev scripts (run, reset-db, sign-manifest)
  .github/workflows/    # CI
  README.md
  AGENTS.md
  .gitignore
  LICENSE
  pnpm-workspace.yaml
  Cargo.toml            # Workspace root
  rust-toolchain.toml
  .editorconfig
  .nvmrc
```

### 26.2 `apps/client/`

The Tauri 2 desktop app. It is the only app that ships to end users. The structure is split along the Tauri convention: `src-tauri/` for Rust, `src/` for the webview.

**26.2.1 `apps/client/src-tauri/`.**

```
src-tauri/
  src/
    main.rs
    lib.rs
    commands/           # Tauri command handlers (one file per domain)
    core/               # Domain logic, no IO, no Tauri
    storage/            # SQLite (sqlx), filesystem operations
    net/                # WebSocket, WebRTC, signaling
    media/              # Manifest, hashing, chunking, subtitles
    events.rs           # Typed IPC events (specta)
  migrations/           # sqlx migrations
  tests/                # Integration tests
  Cargo.toml
  tauri.conf.json
  capabilities/
    default.json        # Scoped permissions
  build.rs
```

Notes:

- `main.rs` is intentionally minimal. It calls into `lib.rs` so the library can be exercised by integration tests.
- `commands/` exposes the IPC surface. Each command is a thin function that validates its input and delegates to a `core/` or `storage/` function. Commands are listed in `events.rs` and registered in `lib.rs`.
- `core/` holds pure domain logic: manifest construction, chunk planning, signature verification, room state machine, download scheduling policy. It has no `tauri` dependency and no filesystem dependency; it takes trait objects for IO. This is what makes it fast to test.
- `storage/` contains all SQLite and filesystem access. It exposes a typed repository API consumed by `core/` and `commands/`.
- `net/` contains the WebSocket client, the WebRTC peer management, and the signaling client. It depends on `core/` for protocol types but is otherwise isolated so it can be fuzzed in isolation.
- `media/` contains the media pipeline: hashing (blake3), chunk planning, subtitle parsing, and manifest serialization.
- `events.rs` is the single source of truth for IPC. Every event and every command is declared with `specta::Type` and `tauri_specta` collects them into a TypeScript declaration.
- `migrations/` are sqlx-managed. Each migration is a single SQL file and is named `<timestamp>_<slug>.sql`.
- `capabilities/default.json` defines the least-privilege set of Tauri capabilities: filesystem (scoped to the library dir and the cache dir), dialog, notification, store, and a custom asset protocol for media playback. Network capabilities are not granted; all network access is through Rust.

**26.2.2 `apps/client/src/`.**

```
src/
  app/                  # AppShell, providers, error boundary, router
  pages/                # One folder per route
    library/
    rooms/
    rooms.new/
    rooms.join/
    rooms.$id/
    settings/
    downloads/
  components/           # Cross-cutting UI (Button, Modal, Toast, etc.)
  hooks/                # Custom hooks
  stores/               # Zustand stores
  services/             # Thin wrappers around tauri::invoke
  styles/               # Tokens, themes, global styles
  i18n/                 # Strings (English only in v1; structure ready)
  main.tsx
  vite-env.d.ts
```

Notes:

- `pages/` mirrors the routes. Each page folder contains an `index.tsx`, a `route.tsx` (loader/component for the router), and page-local components.
- `services/` is the only place that calls `tauri.invoke` or `tauri.event.listen`. Components and hooks go through services so the IPC surface can be mocked in tests.
- `styles/` uses CSS modules plus a small token file. Tailwind is intentionally not used in v1 to keep the dependency surface small and to make the bundle easier to reason about.
- `main.tsx` mounts the React tree, sets up the router, the React Query client, the Zustand stores, the global error boundary, and the IPC event dispatcher.

**26.2.3 `apps/client/` root files.**

- `package.json` - dependencies and scripts: `dev`, `build`, `test`, `test:e2e`, `lint`, `typecheck`, `tauri` (delegates to `@tauri-apps/cli`).
- `vite.config.ts` - Vite config with the React plugin, the Tauri env file, and the path aliases (`@app`, `@components`, `@stores`, etc.).
- `tsconfig.json` - strict TypeScript with `noUncheckedIndexedAccess`, `exactOptionalPropertyTypes`, and `verbatimModuleSyntax`. Path aliases match the Vite config.
- `index.html` - single HTML entry.

### 26.3 `apps/server/`

The signaling and room coordination server. It is a Rust binary that exposes a WebSocket endpoint and a thin HTTPS admin endpoint.

```
server/
  src/
    main.rs
    lib.rs
    config.rs           # Env-driven config
    ws/                 # WebSocket handlers, frame codec
    rooms/              # Room registry, presence, rate limits
    auth/               # Identity tokens, challenge/response
    ratelimit/          # Token bucket per-IP and per-room
    audit/              # Append-only audit log
    metrics.rs          # Prometheus exporter
  migrations/           # sqlx migrations
  tests/                # Integration tests
  Cargo.toml
  Dockerfile
  docker-compose.dev.yml
```

Notes:

- The server is stateless with respect to media. It does not proxy or store media. It only coordinates manifests, room membership, and signaling.
- `audit/` writes structured events to stdout in JSON and optionally to a file. It is the operational record.
- `metrics.rs` exposes `/metrics` for Prometheus. The dashboard is out of scope for v1.
- The Dockerfile is multi-stage and produces a minimal runtime image (`distroless` or `debian-slim`).
- `docker-compose.dev.yml` stands up the server plus a local Postgres for integration tests. The production deployment uses SQLite for v1; the Postgres dependency is dev-only.

### 26.4 `shared/`

Cross-language code. This is the single place where types and wire formats live. Keeping it here prevents drift between client and server.

```
shared/
  protocol/
    src/                # Rust crate
      lib.rs
      messages.rs       # MessagePack message definitions
      room.rs
      manifest.rs
    ts/                 # Generated TypeScript
      index.ts
      messages.ts
      room.ts
      manifest.ts
    Cargo.toml
    package.json
  crypto/
    src/                # Ed25519, blake3 helpers
    Cargo.toml
  manifest/
    src/                # Manifest data model, validation, signing
    Cargo.toml
```

The `shared/protocol/ts/` directory is checked in. It is generated by a build script (`scripts/gen-protocol.sh` and the equivalent pnpm script) and committed to avoid forcing every developer to install the Rust toolchain to do frontend work. The generator is run in CI to verify the committed output matches what would be generated.

### 26.5 Toolchain Recommendations

**26.5.1 ts-rs for shared types.**

`ts-rs` is the recommended tool for generating TypeScript types from Rust. Justification:

- It produces idiomatic TypeScript (no runtime, no classes, no decorators).
- It supports `serde` attributes, so the Rust model is the source of truth.
- It is small, has no runtime dependencies, and is fast.
- It covers the use case we have: shared data models with a strict shape. It is not a full IDL and does not try to be.

Alternatives considered and rejected:

- `specta` alone - excellent for IPC but not designed as a cross-language schema tool. It does not produce stable, committable TypeScript for the protocol crate.
- `protobuf` / `flatbuffers` - heavier, requires a separate schema file, and brings runtime dependencies on both sides. The performance benefit is not needed for our message volumes.
- Hand-written types - guarantees drift.

`ts-rs` is used for the protocol models only. It is not used for IPC commands, where `specta` is preferred (see below).

**26.5.2 specta for IPC commands.**

`specta` (with `tauri-specta` for the Tauri binding) is the recommended tool for the client IPC surface. Justification:

- It generates a TypeScript declaration for every `#[tauri::command]` and every event, with input and output types.
- It is the de facto standard in the Tauri 2 ecosystem and is well-maintained.
- It allows the React side to import the generated bindings as a normal TypeScript module and call typed functions instead of stringly-typed `invoke('name', { ... })`.
- It supports v8 isolates as a side benefit if we ever need a JS engine for subtitling, but that is not the v1 use case.

`specta` is used only inside the client. The server has no IPC in the Tauri sense.

**26.5.3 pnpm workspaces for TypeScript.**

`pnpm` is recommended over `npm` and `yarn` for workspaces because:

- It enforces a strict dependency graph and refuses to resolve a dependency that is not declared in the workspace root or in the package that needs it.
- It is fast on Windows (where `node_modules` layout matters more).
- It supports `workspace:*` protocol out of the box.

**26.5.4 Cargo workspaces for Rust.**

A single `Cargo.toml` at the repo root lists the workspace members: `apps/client/src-tauri`, `apps/server`, `shared/protocol`, `shared/crypto`, `shared/manifest`. `rust-toolchain.toml` pins the toolchain to stable. Features are centralized in workspace inheritance so a single `cargo build` builds everything.

### 26.6 CI and Tooling Files

- `.github/workflows/ci.yml` - matrix build for Linux, macOS, Windows. Runs `pnpm install`, `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, `pnpm test`, `pnpm typecheck`, `pnpm lint`, and the protocol generation check.
- `.github/workflows/release.yml` - triggered on tag; builds the Tauri bundles per OS and pushes to the GitHub release.
- `scripts/dev.sh` / `scripts/dev.ps1` - start the server, the Vite dev server, and the Tauri dev window in the right order.
- `scripts/gen-protocol.sh` - runs `ts-rs` and writes the generated TypeScript.
- `scripts/reset-db.sh` - wipes local SQLite and re-runs migrations.

### 26.7 Build and Bundle Outputs

- `apps/client/src-tauri/target/` - Cargo build output, ignored.
- `apps/client/dist/` - Vite build output, ignored.
- `apps/server/target/` - Cargo build output, ignored.
- Bundle outputs (`*.dmg`, `*.exe`, `*.AppImage`, `*.deb`) are produced by `tauri build` and published by the release workflow; they are not committed.

### 26.8 Versioning

The repo uses a single semantic version for the whole product. The server and the client are released together to keep the protocol and the IPC surface in lockstep. The protocol crate has its own `Cargo.toml` version that is bumped only on a breaking wire-format change, and that bump forces a coordinated release.

### 26.9 Why Not a Single Crate

A single Rust crate for client and server was considered. It is rejected because the client and server have non-overlapping dependencies (Tauri vs. axum) and non-overlapping lifetimes. The shared crates keep the protocol as a small, focused dependency that both sides pin, and they let the server be deployed as a container without dragging the Tauri build chain into the image.

## 27. Testing Strategy

This section defines the testing layers, the "done" criteria for each layer, the CI matrix, and the manual testing checklist. The testing strategy is shaped by the project's non-negotiable rule: a media file must be complete on local disk before playback. Tests must therefore exercise the full path from manifest to local file to playback, not just the happy path of an in-memory stream.

### 27.1 Layered Approach

The strategy is layered from fastest to slowest: unit, integration, contract, end-to-end, and manual. Each layer has a "done" criterion that is enforced in CI. Layers are not skipped; a feature is not considered done until its unit, integration, and end-to-end coverage exists and is green.

### 27.2 Rust Unit Tests

Location: alongside the code, in `#[cfg(test)] mod tests` blocks within each module, or in `core/tests/` for cross-module tests.

Scope:

- `core/` - domain logic with no IO: manifest construction, chunk planning, signature verification, room state machine, download scheduling policy.
- `media/` - hashing, chunking, subtitle parsing, manifest serialization.
- `crypto/` - Ed25519 sign/verify, blake3 hash determinism.
- `storage/` - SQL queries against an in-memory SQLite; repository functions.
- `net/` - protocol codec, frame parser, message envelope.

Framework: built-in `cargo test`. Assertions use `pretty_assertions` for diff readability.

"Done" means:

- Every public function in `core/`, `media/`, and `crypto/` has at least one happy-path test and one failure-path test.
- Every error variant returned by a public function has a test that exercises it.
- Property-based tests exist for: hash determinism, chunk boundary handling (sizes that are exact multiples, plus one, minus one), and manifest round-tripping.
- No test in this layer touches the filesystem, the network, or the system clock. Time is injected.

### 27.3 Rust Integration Tests

Location: `apps/client/src-tauri/tests/` and `apps/server/tests/`, plus `shared/*/tests/` for cross-crate tests.

Scope:

- Client integration: a Tauri app booted in test mode, with mocked `tauri::AppHandle` where needed, exercising the command surface end-to-end through `tauri::test`.
- Server integration: a running axum instance on a random port, exercised with a real WebSocket client.
- Database: `sqlx` migrations run against a fresh SQLite per test, in a `tempfile::TempDir`. The test asserts schema and seed data.
- Storage: filesystem operations run against a `tempfile::TempDir`. The test asserts atomicity (no partial files on interruption), permissions, and error mapping.
- Net: WebRTC peer connections established between two in-process peers with a loopback transport. The test asserts that a chunked transfer completes and verifies the result.

"Done" means:

- Each public command in `commands/` has at least one integration test that invokes it and asserts the typed return value.
- Each migration has a test that runs the migration forward and backward (or, for irreversible migrations, asserts the forward state and the rejection of the downgrade).
- Each error returned over IPC has a test that triggers it and asserts the typed error variant on the receiving side.
- The WebRTC test exercises at least one packet loss and one re-order scenario using a faulty transport.

### 27.4 TypeScript Unit Tests

Framework: Vitest. Run with `pnpm test`.

Scope:

- All Zustand stores: actions produce the expected state transitions, selectors return memoized results, and reset semantics work.
- All hooks: especially `useRoomClock`, `useDriftSmoother`, `useKeyboardScope`, and `useDownloadProgress`.
- All parsers: subtitle parsers, manifest JSON parsers, room code formatters.
- All formatters: timestamps, byte sizes, durations.
- Pure functions in `services/` (the parts that don't call `tauri.invoke`).

"Done" means:

- Every store has tests for its initial state, every action, and the reset.
- Every hook has a test using `@testing-library/react`'s `renderHook` with a controlled environment.
- Every parser has tests for valid input, empty input, malformed input, and at least one adversarial input (e.g. subtitle file with a BOM, a negative timestamp, a 4 GB file size string).
- Code coverage threshold is set to 80 percent lines and branches for `stores/`, `hooks/`, and `services/` parsers/formatters. Coverage is reported in CI; a drop below the threshold fails the build.

### 27.5 Component Tests

Framework: Vitest + `@testing-library/react` + `@testing-library/user-event`. The webview-specific APIs (Tauri) are mocked through a thin `services/` abstraction.

Scope:

- Every page in `pages/` has at least one render test and one interaction test for its primary action.
- Every modal is tested for: open, close, focus trap, escape-to-close, and the typed close reason.
- The transport is tested for keyboard shortcuts, including the suppression when an input is focused.
- The drawing canvas is tested for: pointer down/move/up, undo, and tool switching.
- The download progress modal is tested for the "cannot dismiss while downloading" rule.

"Done" means:

- All accessibility assertions required by section 25.6 are covered: ARIA roles, focus management, live regions, and the `prefers-reduced-motion` branch.
- No component test relies on snapshot diffs of the entire tree. Snapshots are allowed only for stable, non-interactive artifacts (e.g. SVG icons).

### 27.6 End-to-End Tests

Framework: Playwright. Two harnesses:

1. **Tauri WebDriver harness** - drives the actual Tauri 2 webview via WebDriver. Used for smoke tests and for the critical "join a room, wait for download, play" flow. Slow, so it runs on a schedule (nightly) and on PRs that touch the room or download paths.
2. **Vite harness** - drives the same React app served by Vite in a regular browser. Tauri IPC is mocked at the `services/` layer. Used for the full suite of UI flows on every PR. Fast and platform-agnostic.

Scope of E2E:

- Host flow: create a room, confirm manifest, copy code, see the room view.
- Join flow: enter a code, see the download modal, wait for completion, enter the room.
- Playback: host plays, viewer seeks, drift indicator appears at the threshold, "You are behind" modal appears at the deep-behind threshold.
- Drawing: viewer draws, host sees the strokes; undo removes the last stroke.
- Chat: message round-trips, reactions work.
- Leave: "Keep or Delete" prompt fires for temporary media, and the right action takes effect.
- Settings: changing the library dir triggers a rescan, changing the storage cap triggers an eviction.

"Done" means:

- All flows above are green on the Vite harness for every PR.
- The four most critical flows (host create, join, wait-for-download, leave) are green on the Tauri harness on the nightly schedule.
- A failure in the Tauri harness opens a GitHub issue automatically with the run log and the relevant traces.

### 27.7 Protocol Tests

Goal: prevent wire-format drift.

- **Golden MessagePack vectors.** For every message in `shared/protocol/`, a binary fixture in `shared/protocol/tests/fixtures/` is checked in. A test serializes the Rust struct with `rmp-serde` and asserts byte equality. The same fixture is decoded by the generated TypeScript code in a Vitest test. A mismatch fails CI on both sides.
- **Fuzzing.** `cargo-fuzz` targets the protocol decoder, the manifest parser, the subtitle parser, and the room code parser. Each target has a corpus seeded from the golden vectors and is run for a fixed number of inputs per PR that touches the parser (default: 60 seconds per target; longer on nightly).
- **Round-trip property tests.** For every message type, `proptest` generates a random instance, serializes it, deserializes it, and asserts equality. Combined with the golden vectors, this catches both format changes and parser bugs.

"Done" means:

- All golden vectors are checked in and green.
- All fuzz targets build and run for the configured duration on nightly.
- A change to a message type forces a version bump in `shared/protocol/Cargo.toml` and a coordinated release note in `CHANGELOG.md`.

### 27.8 WebRTC Tests

Scope:

- Two headless clients in CI, connected through the server's signaling endpoint, exchange a chunked media file. The test asserts completion, integrity (blake3), and that the throughput exceeds a floor (set conservatively to avoid flakiness).
- A scripted lossy transport injects 5 percent packet loss and 50 ms jitter. The test asserts that the transfer still completes within a bounded time and that the hash verifies.
- A scripted relay path (one client is forced through the TURN relay) is tested for correctness, not throughput.

"Done" means:

- The two-client in-CI test is green and runs in under 60 seconds on the CI matrix.
- The lossy transport test is green and runs in under 120 seconds.
- The TURN path test is green and is part of the nightly suite.

### 27.9 Database Tests

- Every migration is tested with a fresh SQLite per test. The test runs the migration forward, asserts the schema, and runs a representative query.
- The repository layer is tested for: insert, update, delete, and the failure modes of each.
- Concurrency: a test asserts that WAL mode is enabled and that `busy_timeout` is set. A multi-threaded test asserts that 100 concurrent writes do not produce `SQLITE_BUSY` errors.

"Done" means:

- All migrations are tested.
- WAL and `busy_timeout` assertions are part of the boot test.

### 27.10 Security Tests

- `cargo-deny` is run in CI to flag advisories, banned licenses, duplicate dependencies, and source violations.
- `npm audit` is run in CI; high and critical findings fail the build unless explicitly waived.
- A `bandit`-equivalent Rust audit (via `cargo-deny`'s advisory database) runs in CI.
- Manual secret scanning: `gitleaks` runs in CI on every push and PR.
- Tauri capabilities are reviewed on every PR that touches `capabilities/`; a PR that broadens a capability must include a justification in the description.
- The IPC surface is fuzzed in the protocol fuzz targets.

"Done" means:

- All security scanners are green in CI.
- No high or critical advisories are open.
- Capabilities changes are reviewed.

### 27.11 Manual Testing Checklist

Manual testing covers scenarios that are hard to automate reliably, especially poor-network behavior and platform-specific behavior.

**Poor-network simulation:**

- Use `clumsy` on Windows and `network Link Conditioner` on macOS to simulate 100 ms latency, 5 percent loss, and 1 Mbps throughput.
- Verify that the download progress modal updates smoothly, that the drift indicator does not flicker, and that the "You are behind" modal appears at the right threshold.
- Verify that a forced server disconnect is recovered within 10 seconds when the network returns.

**Cross-platform:**

- Windows: NVDA screen reader smoke test on the room view and the library.
- macOS: VoiceOver smoke test on the same views.
- Linux: GNOME Orca smoke test if available; otherwise a keyboard-only navigation pass.

**Browser autoplay:**

- Verify that a media file does not auto-play on a fresh launch. The first play requires a user gesture.
- Verify that the muted-autoplay exception works for the `<video>` element (used for the drawing overlay alignment).

**Disk and filesystem:**

- Library on a slow external drive (USB 2.0 SSD). Verify scan and read throughput are reasonable.
- Library on a path with spaces, with Unicode, and on a junction/symlink.
- Disk full: verify that the download is paused, the user is notified, and no partial file is left behind.

**Permissions:**

- Library dir without read permission: error message is clear and offers to pick a new dir.
- Tauri denied a capability: the UI degrades gracefully and surfaces an explanation.

"Done" means:

- The manual checklist is run before each release and signed off in the release notes.
- Any finding becomes a tracked issue.

### 27.12 CI Matrix

- **Operating systems:** Ubuntu LTS, macOS latest, Windows latest.
- **Rust:** stable, pinned in `rust-toolchain.toml`. `clippy` with `-D warnings`. `rustfmt` check.
- **Node:** 20 LTS, pinned in `.nvmrc`. `pnpm` version pinned in `package.json` `packageManager`.
- **Tauri:** version pinned in the workspace root.
- **Caches:** Cargo registry and target, pnpm store, and Playwright browsers.
- **Job graph:** `lint` and `typecheck` run first and in parallel; `test` runs after they pass; `e2e` runs after `test`; `release` runs only on tag.

### 27.13 Test Data and Fixtures

- A small set of public-domain media files is checked in under `apps/server/tests/fixtures/media/` for use in integration and E2E tests. Sizes are capped at 100 MB total.
- Manifests for those files are checked in as golden vectors.
- Subtitles in SRT, VTT, and ASS are checked in for parser tests.

### 27.14 Coverage and Quality Gates

- Rust: `cargo llvm-cov` produces a coverage report. The threshold is 80 percent for `core/`, `media/`, and `crypto/`; 70 percent overall. Drops fail CI.
- TypeScript: `vitest --coverage` with the thresholds in 27.4.
- E2E: a flaky-test quarantine exists. A test is marked `@quarantine` only with an issue link and is fixed within seven days or removed.

### 27.15 What "Done" Means for a Feature

A feature is not done until:

1. Unit tests for its core logic are present and green.
2. Integration tests for its IO and IPC are present and green.
3. Component tests for its UI are present and green.
4. The relevant E2E flow is green on the Vite harness.
5. The relevant golden vectors are updated.
6. The manual checklist entries for the feature are added.
7. The feature flag (if any) is removed or its removal is scheduled.


## 28. Performance Considerations

This section commits the project to concrete performance numbers and identifies the hot paths that must be measured and protected as the codebase grows. Numbers are first-version targets, not aspirations; they are chosen to be measurable on commodity hardware in 2026 and to leave headroom for the actual product.

### 28.1 Targets

The targets are written as p99 (99th percentile) unless otherwise noted. "Local" means a single desktop user with no other significant load. "LAN" means a single switch, < 1 ms RTT. "Good WAN" means < 50 ms RTT and > 10 Mbps up/down.

| Area | Target |
| --- | --- |
| Library scan, incremental (watch event) | < 2 s for 1000 files |
| Library scan, full | < 30 s for 1000 files |
| SQLite query, library | < 5 ms p99 |
| SQLite query, room | < 20 ms p99 |
| Hash throughput (blake3) | 1+ GB/s on a modern x86_64 core, 250+ MB/s on ARM |
| WebRTC chunk throughput, LAN | 50 MB/s |
| WebRTC chunk throughput, good WAN | 5 MB/s |
| UI frame budget | 16 ms (60 fps) |
| Drawing tool | 60 fps under sustained input |
| Memory, baseline | < 200 MB |
| Memory, active download | < 1 GB |
| Disk write chunk size | 256 KiB |
| Disk fsync cadence | on finalization only, plus on graceful shutdown |
| WebSocket keepalive | 15 s |
| Message envelope, typical | < 200 bytes on the wire |
| IPC requests | invoke (request/response) |
| IPC streams | events (push) |

### 28.2 Library Scan

The library scan walks the configured root directory and produces a manifest entry for each playable file. The two hot paths are the filesystem walk and the per-file hash.

The walk is incremental-first. The scan keeps a content-addressed cache keyed by `(path, size, mtime)`. If a file's size and mtime match the cache, the hash is reused and the file is not re-read. The cache lives in SQLite. The full scan runs only on first launch, on a manual user request, or when the library root changes.

The per-file hash is blake3 in 1 MiB streaming mode. For v1, hashing is parallelized across files but not across cores within a file; intra-file parallelism is a v2 candidate.

Performance work for the scan:

- Use `read_dir` rather than glob; cache `read_dir` results in memory for the duration of the scan.
- Use `tokio::fs` on a bounded pool to keep the queue hot without swamping the disk.
- On Windows, use `FILE_FLAG_SEQUENTIAL_SCAN` and a 1 MiB buffer.
- On macOS and Linux, use `pread` with `O_DIRECT` only when the file is large enough to amortize the syscall cost (threshold: 64 MiB).
- Hash in parallel with the read pipeline: a chunk is hashed as soon as it is read, before the next chunk is requested. This overlaps CPU and IO.

### 28.3 SQLite

The database is the source of truth for the library, the download ledger, the room cache, and the settings mirror. Performance work:

- WAL mode is enabled at boot. The WAL file is checkpointed on graceful shutdown and on a low-water mark.
- `busy_timeout` is set to 5 seconds.
- Indices: `media(path UNIQUE)`, `media(complete)`, `media(kind)`, `media(added_at)`, `downloads(state)`, `downloads(room_id)`, `rooms(state)`, `rooms(last_seen_at)`.
- Queries that return large lists are paginated; the UI uses keyset pagination, not `OFFSET`.
- The download ledger writes are batched: a single transaction flushes up to 100 progress updates or 250 ms of wall time, whichever comes first.
- Prepared statements are cached and reused. The `sqlx` query macro is used to verify queries at compile time.

Hot queries to watch: the library list (must be < 5 ms p99 for 10k entries), the room list (must be < 20 ms p99 for 1k entries), and the per-room manifest join (must be < 20 ms p99). A regression in any of these fails CI.

### 28.4 Hashing

blake3 is the only hash in v1. It is used for content addressing, integrity verification, and chunk boundaries. blake3 is chosen for its throughput, its parallelism story (Rayon-friendly), and its streaming API.

Chunking strategy: each file is split into 256 KiB content-defined chunks. The chunk boundaries are content-defined (a rolling hash) rather than fixed-offset, which gives better deduplication if a future build wants it and better resumption after interruption. The first and last chunk may be smaller to honor file size. The final hash is a blake3 Merkle tree root over the chunk hashes; this root is what the manifest signs.

Performance:

- blake3 throughput on a single modern x86_64 core is over 1 GB/s with SIMD; on ARM (Apple Silicon, modern Snapdragon) it is 250+ MB/s.
- Hashing overlaps with the disk read: a chunk is hashed while the next chunk is being read.
- The hash of a 4 GB file takes roughly 4 seconds on x86_64 and 16 seconds on ARM. This is acceptable because it is a one-time cost on the host and is cached.

### 28.5 WebRTC and Networking

WebRTC is the data path. Performance work:

- The data channel uses `unreliable` mode for chunks (we re-request on loss) and `reliable` mode for control messages (drawings, chat, presence). v1 does not use SCTP-level reliability for media; we accept that we are operating above it.
- The MTU is left at the default; we do not tune it in v1.
- The receive buffer is sized to absorb a 250 ms burst at the target throughput. On LAN, that is 12.5 MB. On good WAN, that is 1.25 MB.
- TURN is mandatory for v1 because NAT traversal fails for a non-trivial fraction of users. The TURN server is configured by the user; defaults are provided.
- The WebSocket keepalive is 15 s. Pongs are processed in the same task as incoming messages to avoid priority inversion.
- The message envelope is MessagePack. The typical control message is < 200 bytes on the wire including the framing. A chunk request is a fixed 32-byte header plus the chunk index.

Hot paths to watch:

- The chunk request scheduler. It must not issue more than `outstanding_chunks` requests per peer (default 32) and must prefer the peer with the lowest current latency.
- The reassembly buffer. It must be bounded by `outstanding_chunks * chunk_size` per peer.
- The event loop. It must not block on IO; all syscalls are async.

### 28.6 UI Frame Budget

The webview is held to a 16 ms frame budget. The video element is decoded by the browser's media stack, which uses GPU acceleration on every supported platform. The React tree must not contend with the video decode.

Performance work:

- The drawing canvas uses `requestAnimationFrame` for all rendering. Pointer events are coalesced. The canvas is sized to the video's intrinsic resolution and is scaled with `transform: scale()` to keep the GPU on the fast path.
- The transport bar is a single component that subscribes only to the slice of state it needs. Zustand selectors with shallow equality keep re-renders minimal.
- The participant list virtualizes; with more than 20 participants, only the visible rows are rendered.
- The chat list virtualizes; with more than 100 messages, only the visible rows are rendered.
- React Query is configured with conservative refetch intervals to avoid network thrash.
- The drift indicator is throttled to one update per 200 ms; the underlying state may change more often, but the DOM does not.
- Long lists use `content-visibility: auto` to keep off-screen work off the main thread where the browser supports it.

Hot paths to watch:

- Pointer event handlers on the drawing canvas. They are profiled with the Performance panel on every release.
- The transport bar's re-render frequency. It is asserted in a test that the transport bar re-renders at most once per 250 ms during playback.
- The chat list's append path. It is asserted in a test that appending 100 messages per second does not drop a frame.

### 28.7 Memory

The baseline is the Tauri shell plus the React app with no active room. Target: < 200 MB on Windows, macOS, and Linux. Measured with the OS task manager and the Performance panel.

An active download adds a bounded receive buffer per peer (default 32 chunks of 256 KiB = 8 MB) plus the file being written. The file is written through a memory-mapped region with a 4 MiB window to keep RSS predictable. Target during an active download: < 1 GB total.

The drawing canvas keeps the stroke buffer in memory. Each stroke is a list of points; a typical 30-second drawing session produces under 1 MB. The stroke buffer is reset on room leave.

The participant and chat lists are virtualized, so their memory cost is proportional to the visible window, not the total size.

### 28.8 Disk

Writes are chunked at 256 KiB. The finalization step is the only place that calls `fsync` on the data; the parent directory is fsynced after the atomic rename. This gives at-most one fsync per file completion, which is acceptable on SSDs and necessary on spinning disks.

Atomicity: a download writes to `<file>.part` and renames to `<file>` only after the blake3 root verifies. The rename is `rename(2)` (or `MoveFileEx` with `MOVEFILE_REPLACE_EXISTING` on Windows) and is atomic on every supported filesystem.

Eviction: when the storage cap is reached, the eviction policy removes the least-recently-played temporary files first, then the least-recently-played permanent files (with a confirm prompt). The policy is a single function in `core/` and is unit-tested.

### 28.9 Tauri IPC

The IPC surface is split by shape:

- `invoke` is used for request/response: start a download, fetch a manifest, persist a setting.
- `events` are used for streams: download progress, room state, chat messages, presence changes.

Events are emitted on a per-domain channel so the React side can subscribe selectively. The React side batches state updates from events using a microtask coalescer to avoid render storms.

The IPC bridge is the only place that calls into Tauri. The `services/` layer is the only place that calls the bridge. This makes the bridge mockable in component tests and replaceable in the Vite E2E harness.

### 28.10 Hot Paths (Documented)

The following are the hot paths in v1. Each has a benchmark, a test, and a regression gate.

1. **Library scan** - benchmarked with a fixture of 1000 files on a warm cache. Gate: p99 < 30 s full, < 2 s incremental.
2. **Library list query** - benchmarked with a fixture of 10k rows. Gate: p99 < 5 ms.
3. **Manifest serialization** - benchmarked with a 10 GB manifest (synthetic). Gate: serialize + blake3 < 200 ms.
4. **Chunk request scheduler** - benchmarked with 4 simulated peers at 1 Gbps. Gate: schedule 1 MiB of requests in < 1 ms.
5. **Chunk reassembly** - benchmarked with 1 GiB of incoming chunks in random arrival order. Gate: no drops, no out-of-order writes.
6. **Drawing canvas pointer pipeline** - measured in the browser Performance panel. Gate: p99 frame time < 16 ms under sustained input at 120 Hz.
7. **Transport re-render cadence** - asserted in a test. Gate: < 1 re-render per 250 ms during playback.
8. **Tauri event throughput** - benchmarked with a synthetic load of 1000 events per second. Gate: no drops, p99 dispatch < 1 ms.
9. **Settings load on boot** - measured from process start to first paint. Gate: < 500 ms on a warm OS cache.
10. **WebRTC chunk throughput** - measured in the in-CI two-client test. Gate: > 25 MB/s on loopback (CI floor, conservative).

### 28.11 Profiling and Measurement

- **Rust**: `cargo bench` for the hot paths, with criterion. Results are tracked over time; a > 10 percent regression on a tracked bench fails CI on the relevant PR if the PR touches the hot path.
- **TypeScript**: the Vite build is profiled with `vite-plugin-visualizer` on every release. Bundle size is tracked; a > 5 percent regression fails CI.
- **Webview**: the Performance panel trace is captured on every release and compared against a reference trace. A regression in long tasks (> 50 ms) or in script execution time is investigated.
- **End-to-end**: the Tauri WebDriver harness records a trace on every nightly run. The trace is uploaded as an artifact.

### 28.12 What This Section Does Not Promise

- Adaptive bitrate / transcoding: not in v1. A media file is played as-is.
- HDR tone mapping beyond the browser default: not in v1.
- Hardware-accelerated encoding for any future recording: out of scope.
- Sub-second cold start: not a target for v1. A 1-2 second cold start is acceptable.

## 29. Major Technical Risks

This section lists the technical risks that could prevent the project from shipping or from operating correctly in the field. Each risk has a severity, a likelihood, a concrete mitigation, and an owner-level trigger that escalates it. The list is not exhaustive; it is the floor.

### 29.1 Top 10 Risks

**Risk 1: WebRTC NAT traversal fails for some users.**

- **Severity**: high. Without a relay, a significant fraction of users on symmetric NATs and carrier-grade NATs cannot connect peer-to-peer. This is a deployment blocker, not a polish item.
- **Likelihood**: high in residential and mobile networks.
- **Mitigation**: TURN is mandatory. The TURN server is configured by the user in settings; sensible defaults are provided. The connection state machine always tries direct first, falls back to TURN on ICE failure, and surfaces a clear status in the top bar (green = direct, amber = relay, red = failed). The CI matrix includes a forced-relay test to keep the path honest.
- **Trigger to escalate**: any field report of a user who cannot connect on a network where TURN works for a different application.

**Risk 2: Hash collision (sha256/blake3).**

- **Severity**: low. A collision would let a malicious file masquerade as a legitimate one.
- **Likelihood**: astronomically low. blake3 has 256-bit collision resistance; brute force is infeasible. Cryptanalytic breaks have not been published.
- **Mitigation**: the manifest signs the blake3 root, not the file metadata. Clients verify the root against the host's Ed25519 signature and verify the file against the root. The combination is sufficient.
- **Documentation**: the manifest format documents the hash and the signature scheme, and a section in the architecture explains why this is not a practical concern.

**Risk 3: Disk corruption mid-download.**

- **Severity**: high. A corrupted file is not playable and is the failure mode the product exists to avoid.
- **Likelihood**: medium on spinning disks, low on SSDs, non-zero on power loss or USB disconnect.
- **Mitigation**: downloads write to `<file>.part` and rename only after the blake3 root verifies. The verification is run again on every play; a corrupted file is detected and re-downloaded. The rename is atomic on every supported filesystem. The library scan treats `.part` files as invisible.
- **Test**: the integration suite includes a kill-the-process-mid-download test and a bit-flip-the-final-chunk test.

**Risk 4: Host abandons room mid-download.**

- **Severity**: medium. Viewers may be stuck waiting for chunks that will never arrive.
- **Likelihood**: medium. Hosts are people; people disconnect.
- **Mitigation**: the server keeps the manifest and a presence list for 30 minutes after the last heartbeat. A viewer can wait. After 30 minutes, the room is auto-ended and the viewer is shown a "host unavailable" message. A viewer may also leave and rejoin; the room code remains valid for the 30-minute window.
- **UX**: the room view shows a "host offline" badge and a countdown to the auto-end.

**Risk 5: Manifest tampering by malicious "host".**

- **Severity**: high. A tampered manifest could direct viewers to a different file or to a file the attacker controls.
- **Likelihood**: low in trusted friend groups, non-zero in any open deployment.
- **Mitigation**: every manifest is signed by the host's Ed25519 key. The public key is bound to the room code by the server at room creation and is fetched over the authenticated channel. Clients verify the signature before downloading any chunk. A signature mismatch aborts the join with a clear error.
- **Trust model**: the server is trusted to bind the key to the room code. The server is not trusted with the content; it never sees chunks. End-to-end verification of content is the client's job.

**Risk 6: SQLite lock contention under load.**

- **Severity**: medium. Lock contention manifests as UI stutter and download stalls.
- **Likelihood**: medium under heavy write load (many concurrent progress updates).
- **Mitigation**: WAL mode is enabled at boot. `busy_timeout` is set to 5 seconds. Progress writes are batched in a single transaction (100 updates or 250 ms). The download ledger is a separate database from the library to isolate the write-heavy path.
- **Test**: a multi-threaded integration test asserts that 100 concurrent writers do not produce `SQLITE_BUSY` errors.

**Risk 7: Tauri custom protocol caching of large media.**

- **Severity**: high. If the `asset://` protocol caches, playback of large files breaks the memory budget and produces confusing bugs.
- **Likelihood**: medium by default; the Tauri custom protocol has caching behavior that must be disabled.
- **Mitigation**: the custom protocol is configured with `cache: false` and supports HTTP range requests. The webview fetches media through the protocol with `Range` headers; the Rust side uses a bounded memory-mapped window and seeks on demand. The protocol is exercised in the integration suite with a 4 GB synthetic file.
- **Test**: an integration test asserts that a 4 GB file is playable from a cold start with a bounded memory footprint.

**Risk 8: Subtitle parsing performance for large files.**

- **Severity**: medium. A multi-megabyte SRT or ASS file is not fast to parse in the webview.
- **Likelihood**: low for typical files, high for edge cases (long movies with commentary tracks).
- **Mitigation**: subtitle parsing is done in Rust at library-scan time. The parsed cues are stored as JSON in the database and shipped to the webview ready to render. The webview does no parsing.
- **Test**: parser tests include a 10 MB SRT and a 5 MB ASS; both must parse in under 100 ms on the target hardware.

**Risk 9: Clock skew across participants causing drift indicator noise.**

- **Severity**: low. A noisy drift indicator trains users to ignore it.
- **Likelihood**: medium. NTP-synced clocks are usually within 50 ms, but the offset estimate from playback events can be noisy.
- **Mitigation**: the drift estimate is a low-pass filter (exponential moving average with a time constant of 5 seconds). The indicator only appears when the smoothed offset exceeds 2.0 seconds, and it shows the smoothed value. The threshold is a setting; the default is 2.0 s.
- **Test**: a unit test feeds a noisy signal and asserts the smoothing behavior and the threshold.

**Risk 10: Path traversal via crafted filenames.**

- **Severity**: critical. A path traversal in the file ops is a remote code execution risk in some configurations.
- **Likelihood**: low but non-zero if a manifest references a path outside the library dir.
- **Mitigation**: every filesystem operation validates that the resolved path is within the configured library dir using `canonicalize` and a prefix check on the canonical path. Validation is a single function in `core/` and is used by every entry point. The validation function is fuzzed in the protocol fuzz targets.
- **Test**: the validation function is tested against a battery of crafted paths (`..`, absolute paths, symlinks, junctions, UNC paths, NUL bytes).

### 29.2 Additional Risks (11-15)

**Risk 11: Bandwidth asymmetry.**

- **Severity**: medium. The host's upload is often the bottleneck; if it is much smaller than the viewers' download, the room's effective throughput collapses.
- **Likelihood**: high in residential networks.
- **Mitigation**: the chunk scheduler is host-aware. The host advertises its measured upload throughput; viewers request at a rate that does not exceed it. The UI shows the host's upload saturation and warns when it is the bottleneck. Multi-source downloads (a viewer pulling the same chunk from multiple peers) are a v2 candidate.

**Risk 12: P2P hostile peer sending corrupt chunks.**

- **Severity**: high. A peer could send corrupted chunks to waste the viewer's time or to attack the network.
- **Likelihood**: low in trusted groups, non-zero in any open deployment.
- **Mitigation**: every chunk is verified against its blake3 hash before it is written to disk. A peer that sends a corrupt chunk is rate-limited and, after a threshold, banned for the room. The threshold and the ban duration are settings.

**Risk 13: Server compromise.**

- **Severity**: high. A compromised server could lie about manifests or about room membership.
- **Likelihood**: low; the server is small and the surface is narrow.
- **Mitigation**: the server is not trusted with content. Clients verify manifest signatures end-to-end. The server is trusted with room membership and with the binding of host public keys to room codes; this trust is minimized and auditable. The server's audit log is append-only and shipped off-host. A compromise is detectable by signature mismatches in the field.

**Risk 14: Browser autoplay policies.**

- **Severity**: medium. Browsers block autoplay of media with audio without a user gesture.
- **Likelihood**: certain. Every modern browser enforces this.
- **Mitigation**: the room view never auto-plays with audio on first launch. The first play requires a click on the play button. Subsequent plays within the same session are allowed to resume. The muted-autoplay exception is used to align the drawing overlay on first entry without sound. The transport always has a visible play button even when keyboard shortcuts are the primary input.

**Risk 15: Large files > 4 GB on FAT32.**

- **Severity**: medium. FAT32 cannot store files > 4 GB; users who pick a FAT32 library will fail at the worst possible moment.
- **Likelihood**: low on modern systems, high on USB sticks and SD cards.
- **Mitigation**: the library dir picker inspects the filesystem type. If it is FAT32 (or any other filesystem that cannot hold the largest file in the library), the user is warned and asked to confirm. The warning is a modal with a clear explanation. The scanner can still index the library but cannot complete downloads to it; the UI makes this explicit.

### 29.3 Risk Register Maintenance

The risk register lives in `docs/RISKS.md` and is reviewed at every release. A risk is closed only when its mitigation is implemented and tested. A new risk is added during the PR review if a reviewer flags a concern that is not on the list.

## 30. Decisions That Should Be Deferred Until Later

This section lists the features and decisions that are intentionally out of scope for v1. For each, the rationale is given along with a v2 candidate flag and a reason. The list is meant to be defended; any item that the team wants to pull into v1 must be re-justified in the architecture document.

### 30.1 BitTorrent-style swarms

- **v1 explicitly does NOT include**: BitTorrent-style swarms with multiple swarms per file, piece-level tit-for-tat, or distributed hash table piece routing.
- **v2 candidate**: yes, conditional. The protocol can evolve toward a swarm model without breaking the v1 manifest format, but the complexity is not justified for v1's room sizes (6-8 people). Revisit when room sizes grow or when the user base asks for it.

### 30.2 DHT / trackerless discovery

- **v1 explicitly does NOT include**: distributed hash tables, trackerless BitTorrent-style discovery, or any peer discovery mechanism outside the server.
- **v2 candidate**: no for v2, possibly later. The server is the single point of coordination and is the simplest correct design. DHT is a scaling play that the project does not need.

### 30.3 Server-side recording / VOD

- **v1 explicitly does NOT include**: server-side recording of rooms, replay, or VOD.
- **v2 candidate**: yes, conditional. Recording has obvious appeal but is a product decision (storage, privacy, moderation) more than a technical one. Out of scope until the product story is clearer.

### 30.4 Transcoding / adaptive bitrate

- **v1 explicitly does NOT include**: server-side or client-side transcoding, adaptive bitrate streaming, or any re-encoding of the source media.
- **v2 candidate**: no. The whole point of Locast is local playback of local files. Transcoding contradicts the core rule and the threat model.

### 30.5 Mobile clients

- **v1 explicitly does NOT include**: iOS, iPadOS, or Android clients.
- **v2 candidate**: yes, after the desktop product is stable. Tauri 2 has mobile targets, and the React codebase is portable, but the desktop-first design (filesystem access, large media, high throughput) does not translate one-to-one. A mobile client would need a different scan and storage story.

### 30.6 HDR / Dolby Vision fancy rendering

- **v1 explicitly does NOT include**: HDR tone mapping, Dolby Vision rendering, or any media pipeline beyond the browser default.
- **v2 candidate**: yes, conditional. The browser is the limit here. If the target platforms improve their HDR support, v2 can revisit. Until then, the user gets whatever the browser provides.

### 30.7 Voice chat

- **v1 explicitly does NOT include**: voice chat, microphone capture, or audio mixing in the room.
- **v2 candidate**: yes. Voice chat is a frequent request and is a natural addition once the room product is stable. It requires server-side or P2P audio mixing, which is a significant subsystem.

### 30.8 E2E encryption of room events beyond signatures

- **v1 explicitly does NOT include**: end-to-end encryption of chat, drawings, or control messages. The P2P control path can be E2E in v2.
- **v2 candidate**: yes. The transport can be upgraded to DTLS for the data channel (it already is, by WebRTC) and the application layer can add an E2E key exchange. v1's signatures protect the manifest; chat and drawings are not encrypted at the application layer in v1.

### 30.9 Multiple libraries per user

- **v1 explicitly does NOT include**: multiple library roots, per-library settings, or library switching in the UI.
- **v2 candidate**: yes, after user research. The single-library assumption simplifies the scanner, the storage cap, and the settings model. A second library is a small step from there.

### 30.10 Cloud sync of library

- **v1 explicitly does NOT include**: cloud sync, cloud backup, or any server-side storage of media.
- **v2 candidate**: no for the core product. Cloud sync contradicts the local-first premise. A v2 "opt-in backup" feature is possible but would be a separate product line with its own threat model.

### 30.11 Plugin system

- **v1 explicitly does NOT include**: a plugin API, third-party extensions, or any extension point beyond the documented IPC surface.
- **v2 candidate**: yes, after the IPC surface stabilizes. A plugin system without a stable surface is a liability.

### 30.12 Custom themes

- **v1 explicitly does NOT include**: user-defined themes, custom accent colors beyond the identity color, or custom CSS injection.
- **v2 candidate**: yes, after the design system settles. The v1 design is a single, opinionated system; customization comes after the system is validated.

### 30.13 Federation between servers

- **v1 explicitly does NOT include**: federation, multi-server rooms, or cross-server signaling.
- **v2 candidate**: no. Federation is a multi-year project on its own. The v1 server is a single deployment per user (or per community).

### 30.14 Other Deferred Items (Brief)

- **Captions authoring / editing**: v1 displays captions; it does not author them. v2 candidate: yes.
- **Picture-in-picture**: v1 may include the browser-default PiP; a custom PiP is a v2 candidate.
- **Chromecast / AirPlay**: v1 explicitly does NOT include. v2 candidate: yes, conditional.
- **Hardware DRM (Widevine, PlayReady, FairPlay)**: v1 explicitly does NOT include. Out of scope; the content is local and not commercially licensed.
- **Subtitle translation / auto-translation**: v1 explicitly does NOT include. v2 candidate: yes, as an opt-in feature.
- **AI-assisted features (auto-chapters, summaries, scene detection)**: v1 explicitly does NOT include. v2 candidate: yes, as a separate product decision.


---

## Appendix A. Open Decisions to Confirm with Stakeholders

The items below are flagged in the draft sections as "Decisions deferred" and rose to the level where a stakeholder decision is needed before implementation. They are collected here so they can be reviewed in one place. Each is referenced to its source section.

### A.1 From section 2 (Stack)

- **HTTP fallback for NAT-blocked users.** Ship HTTP/3 range-request fallback to a host-side `axum` server, or stay DataChannel-only in v1? *Current draft: DataChannel-only.* v1 ships the simpler form.
- **Static `ffmpeg` bundling.** Bundle a static `ffmpeg` in the installer, or download on first run? *Current draft: download on first run* to keep the installer small.
- **`webrtc` vs `webrtc-sys` crate.** Pure Rust `webrtc` (no native deps) or `webrtc-sys` (libwebrtc bindings, faster)? *Current draft: pure Rust.* Revisit if DataChannel throughput is unacceptable.

### A.2 From section 3 (Architecture)

- **DataChannel framing for `media`.** Reliable+ordered (current) or reliable+unordered with per-chunk sequence? *Current draft: ordered.*
- **Shared `ephemeral` channel for chat and drawing, or separate channels?** *Current draft: shared.*
- **CLI/HTTP shim for headless downloads.** Out of scope for v1.

### A.3 From section 6 (Storage)

- **Quota for `temporary` items and in-flight download staging.** Count toward the same cap as permanent media. *Locked v1 decision:* permanent + temporary + in-flight bytes all count; nothing is exempt.
- **Multi-library roots.** Single root in v1.
- **`trash/` as junction/symlink to system Recycle Bin.** Skipped in v1.
- **Hardlink vs copy for "import existing file".** v1 moves; revisit if users complain.

### A.4 From section 8 (Manifest)

- **Merkle root for `chunk_hashes`.** Flat list in v1; revisit if manifests exceed ~5 MB.
- **v2 manifest with `url_hint` magnet link.** Punt.
- **Per-`media[]` entry signatures vs whole-manifest signature.** Whole-manifest in v1.
- **`host_signature.public_key` encoding.** base64 inside the signature object, hex `peer_id` values elsewhere; needs clear docs.

### A.5 From section 9 (Download / P2P)

- **TURN credentials for symmetric-NAT users.** *Locked v1 decision:* TURN is REQUIRED as a fallback when direct ICE fails; the server mints short-lived HMAC credentials. v2 may add a paid TURN provider option.
- **Multi-range HTTP `Range` requests on `locast://` protocol.** Single-range in v1.
- **Merkle root in manifest.** Punt (see A.4).
- **Pre-emptive push vs request-driven.** Request-driven in v1.
- **Per-room "seed budget" UI.** Deferred.
- **WebSeed (BEP-9) HTTP sources.** No in v2.
- **5 Hz progress event rate.** May feel laggy on huge files.

### A.6 From section 10 (Rooms)

- **Exact server message envelope** (length prefix, compression, framing) for the command channel.
- **Whether `monotonic_seq` is reset across reconnects or monotonic per session.**
- **Host transfer is deferred to v1.1.** *Locked v1 decision:* if the host does not return within the 30-second grace period, the server ends the room. There is no v1 host-transfer option to configure.
- **5-minute viewer reconnect window** is per-room or global.
- **NTP-style skew measurement interval and jitter threshold** specifics.
- **Server retention window for ended-room command logs** (indefinite or fixed).
- **Historical ended-rooms retained per client** before local pruning.
- **Throttling/replay strategy for the missing-range request** (windowed vs unbounded).
- **Exact format of PARTICIPANT_READY** (per-user, per-room, batched).
- **Whether ACCEPTING_VIEWERS is a distinct state** from READY.

### A.7 From section 12 / 13 (Sync)

- **Wire format for capability events** (JSON shape, version field).
- **Whether `playback.manual_sync` is implied by `playback.view`** or always separate.
- **Specifics of `room.invite` capability** (out of scope v1 but reserved).
- **Whether the host can grant a capability they themselves do not have.**
- **Maximum number of capability entries per participant** (currently unbounded).
- **User-defined capability presets** or only built-in.
- **Exact UI for the per-user capability editor** (matrix, list, search).
- **Whether a revoked capability can be re-granted** by the same host without an intermediate step.
- **Whether lurkers (no `playback.view`) appear in the participant list** or are hidden.
- **How `sync.receive_position` revocation interacts with server broadcast** (filter at send time vs receive time).
- **Whether the audit log is exportable** and in what format.
- **Exact rate limits for capability changes** to prevent thrash.
- **Whether `can()` is the only authorization chokepoint** or whether low-level actions (e.g. WS connect) have separate gates.

### A.8 From sections 15 / 16 / 17 (Drawing, Laser, Subtitles)

- **Maximum polyline length for `pen` strokes** (currently 10,000).
- **Eraser as separate canvas layer vs composite-operation subtraction on single canvas.**
- **Per-user undo ring buffer (N=50) configurable per room or only per user.**
- **Whether `stroke_undo` can undo a `clear_all`** (restore the canvas to its pre-clear state).
- **Hard vs soft `clear_all`** - v1 says hard.
- **Whether drawing events are persisted in the room's command log on the server** for in-session replay, or only in client SQLite.
- **Rate-limit cap (500 events/sec)** per user or per connection.
- **Laser color by user_id hash** uses deterministic hash or server-assigned color.
- **Whether the laser trail length and fade duration are user-configurable.**
- **Whether multiple modifiers (Ctrl+Shift etc.) activate different modes** (e.g. temporary eraser).
- **Whether the laser is rendered on the same canvas as strokes** or a separate canvas layer (separate recommended).
- **Whether subtitle preferences are per-track or global.**
- **Whether SSA/ASS styling overrides user font/color preferences** or user preferences always win.
- **Whether the Rust pre-parse path is opt-in per format** or default for SSA/ASS.
- **Whether the `timeupdate` fallback to setInterval at 30 Hz is sufficient** or 60 Hz is required.
- **Maximum subtitle file size before forcing the Rust pre-parse path.**
- **Whether WebVTT line and position cue settings are honored** when user's position preference is set.
- **Whether subtitle selection should persist across uninstall/reinstall** (probably no).
- **Whether the active drawing user (red laser) is the local user or the most recent remote stroker**, and how ties are broken.

### A.9 From section 18 (Network Protocol)

- **CBOR vs MessagePack final choice** after bundle-size benchmark.
- **`compress: "zstd"` envelope flag in v1.5** if WS bandwidth > 30% NIC.
- **Whether to sign `ts_ms` along with the payload.**
- **Exact "loudest sender" algorithm** when per-room aggregate is exceeded.
- **DOWNLOAD_CHUNK per-chunk signature** - Ed25519 vs HMAC-BLAKE3 keyed on a session secret.

### A.10 From section 19 (WebRTC)

- **Second DataChannel "chat" in v1.5** once control fan-out is moved P2P.
- **TURN provider choice** (self-hosted coturn vs Twilio) for hosted deployments.
- **"Windowed" chunk request algorithm** (N outstanding) - v1 sequential, v1.1 windowed.
- **ICE restart policy on `connectionState = "disconnected"`** - 1 in v1, 2-3 with backoff in v1.5.
- **Per-transfer vs shared DC for files** - ship shared, per-transfer is v1.5.
- **Exact `bufferedAmountLowThreshold` value** (256 KiB in v1, tune in v1.1).

### A.11 From section 20 (Server)

- **Postgres support** (v1 SQLite only; Postgres is v1.5).
- **Redis pub/sub for horizontal scaling** (v1.5).
- **TURN credential rotation cadence** beyond 1-hour TTL.
- **Database for `media_index`** (SQLite in v1).
- **Whether `/rooms/:code/info` returns the host's display name** (currently no).
- **Exact `MaxMessageSize` constant** on the WS upgrade (1 MiB in v1).
- **Whether to log the source peer of every DOWNLOAD_OFFER** for audit.

### A.12 From section 21 (Security)

- **Client-side CA pinning for enterprise proxy friendliness** (v1.5).
- **Exact IP-truncation TTL and policy** (24h in v1).
- **Whether to log display names at all** (v1 truncates to 32 chars).
- **2FA for the local app unlock** (out of v1 scope).
- **Exact `cargo-deny` license allowlist** (will be tightened in v1.5).
- **NAK retry budget** (5 in v1; may be 3 in v1.5).

