# Locast Roadmap

## About this roadmap

This document is the atomic, phased implementation plan for Locast. It is derived from `docs/ARCHITECTURE.md` and the open decisions collected in Appendix A of that document. The architecture is the source of truth; this roadmap breaks it into work units.

Each task is:

- **Atomic**: completable in a single focused coding-agent session.
- **Non-overlapping in file ownership** where possible; where overlap is required, tasks are sequenced with explicit prerequisites.
- **Verifiable**: each task has at least one acceptance criterion that can be checked by running a command or reading output.
- **Sized**: estimated complexity S / M / L.
- **Typed**: suggested agent type (frontend / backend / security / etc.) so the right tool can be picked.

Phases are independently shippable as internal milestones. Tasks within a phase are intended to be picked in order, but later tasks in a phase may be done in parallel with earlier tasks in the next phase.

Tasks are not created equal: a few critical-path tasks are M or L; the rest are S. Where a task must exceed "single session", it is explicitly called out and split.

Module paths referenced in the "Files / modules" column are from section 26 of the architecture.

## Phases

- **Phase 0 - Foundation.** Stand up the repo, toolchain, CI, and schema scaffolding. No product behaviour yet.
- **Phase 1 - Local storage + media library.** Everything that runs without any network. Library, scanner, probe, custom protocol, library UI.
- **Phase 2 - Identity, signaling, room lifecycle.** Server skeleton, keypair, auth, room CRUD, presence.
- **Phase 3 - Manifest publishing + viewer download flow.** Signed manifest, P2P DataChannel transfers, chunking, verification.
- **Phase 4 - Playback engine + room control.** Local `<video>` player, command application, drift UI, manual sync.
- **Phase 5 - Drawing + laser pointer.** Vector drawing canvas, laser overlay, undo.
- **Phase 6 - Permissions, chat, presence polish.** Capability enforcement, chat, presence, participant UI.
- **Phase 7 - Reconnection, host transfer, edge cases.** WS reconnect, stale participants, host disconnect, lifecycle cleanup.
- **Phase 8 - Security hardening, fuzzing, perf.** Fuzzing, supply chain, perf benches, security review.
- **Phase 9 - Beta packaging, docs, release.** Bundles, installers, release notes, final docs.

---

## Phase 0 - Foundation (repo, tooling, CI, schemas)

Goal: a buildable, linted, testable empty workspace with both Rust and TypeScript toolchains wired up, plus the database migrations that everything else will rely on.

- **P0-T01 - Monorepo skeleton + workspaces.**
  - Goal: pnpm workspace and Cargo workspace at the repo root with all packages and crates registered, even if empty.
  - Files / modules: `pnpm-workspace.yaml`, `Cargo.toml`, `rust-toolchain.toml`, `.nvmrc`, `.editorconfig`, empty `apps/client/`, `apps/server/`, `shared/protocol/`, `shared/crypto/`, `shared/manifest/` directories.
  - Prerequisites: none.
  - Acceptance: `pnpm install` and `cargo build` both succeed at the workspace root; `pnpm-workspace.yaml` and `Cargo.toml` are present; rust-toolchain pins stable; .nvmrc pins Node 20 LTS.
  - Complexity: S. Agent: general.

- **P0-T02 - Tauri 2 scaffold for the client.**
  - Goal: a Tauri 2 app that opens an empty webview window.
  - Files / modules: `apps/client/src-tauri/Cargo.toml`, `apps/client/src-tauri/tauri.conf.json`, `apps/client/src-tauri/src/main.rs`, `apps/client/src-tauri/src/lib.rs`, `apps/client/src-tauri/capabilities/default.json`, `apps/client/src-tauri/build.rs`, `apps/client/package.json`, `apps/client/vite.config.ts`, `apps/client/tsconfig.json`, `apps/client/index.html`, `apps/client/src/main.tsx`.
  - Prerequisites: P0-T01.
  - Acceptance: `pnpm tauri dev` opens a window with a "Hello, Locast" placeholder; capabilities list is the minimal one from section 5 of the architecture.
  - Complexity: M. Agent: frontend.

- **P0-T03 - Server skeleton (axum 0.7 + tokio).**
  - Goal: the signaling server compiles, starts, serves `/health` and `/version`, and is wired into `docker-compose.dev.yml` for local use.
  - Files / modules: `apps/server/Cargo.toml`, `apps/server/src/main.rs`, `apps/server/src/lib.rs`, `apps/server/src/config.rs`, `apps/server/src/metrics.rs`, `apps/server/Dockerfile`, `apps/server/docker-compose.dev.yml`.
  - Prerequisites: P0-T01.
  - Acceptance: `cargo run -p locast-server` serves `GET /health -> 200 {"status":"ok"}` and `GET /version`; `docker compose -f docker-compose.dev.yml up` brings the server up on a port; the metrics endpoint is empty but returns 200.
  - Complexity: S. Agent: backend.

- **P0-T04 - CI workflows (lint, typecheck, test, build).**
  - Goal: GitHub Actions matrix on Linux, macOS, Windows runs `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test`, `pnpm test`, `pnpm typecheck`, `pnpm lint`.
  - Files / modules: `.github/workflows/ci.yml`, `.github/workflows/release.yml`, scripts at `scripts/ci-check.sh` and `scripts/ci-check.ps1`.
  - Prerequisites: P0-T01.
  - Acceptance: a draft PR shows all matrix jobs green on a stub repo; cache steps for cargo and pnpm are configured; release workflow is a no-op stub that triggers only on tag.
  - Complexity: S. Agent: general.

- **P0-T05 - SQLx setup + initial migration 0001_init.**
  - Goal: SQLite database opens with the PRAGMAs from section 7, runs the `0001_init.sql` migration, and exposes the full schema to Rust.
  - Files / modules: `apps/client/src-tauri/src/storage/mod.rs`, `apps/client/src-tauri/src/storage/migrations/0001_init.sql`, `apps/client/src-tauri/Cargo.toml` (sqlx, sqlx-cli, dirs).
  - Prerequisites: P0-T02.
  - Acceptance: an integration test starts the storage layer on a `tempfile::TempDir`, runs the migration, and asserts all expected tables (`media_items`, `rooms`, `room_manifests`, `room_participants`, `downloads`, `download_chunks`, `room_events`, `presence`, `user_identities`, `room_invites`, `settings`, `media_subtitles`) and the FTS5 virtual table exist; PRAGMAs verify `journal_mode = WAL`, `foreign_keys = ON`, `busy_timeout = 5000`.
  - Complexity: M. Agent: backend.

- **P0-T06 - Empty specta IPC surface for the client.**
  - Goal: a single `#[tauri::command] greet() -> String` plus its TS binding generated by `tauri-specta` proves the IPC toolchain end-to-end.
  - Files / modules: `apps/client/src-tauri/src/events.rs`, `apps/client/src-tauri/src/commands/mod.rs`, `apps/client/src/services/ipc.ts`, `apps/client/src/bindings/` (generated).
  - Prerequisites: P0-T02.
  - Acceptance: the React app calls `greet()` via the generated typed binding and displays the returned string; the generated bindings are checked in (per section 26.4) and CI verifies the generator is reproducible.
  - Complexity: S. Agent: frontend.

- **P0-T07 - ts-rs protocol skeleton.**
  - Goal: one example struct in `shared/protocol/src/lib.rs` and its TS counterpart in `shared/protocol/ts/index.ts`, generated by `ts-rs` and checked in.
  - Files / modules: `shared/protocol/Cargo.toml`, `shared/protocol/src/lib.rs`, `shared/protocol/ts/index.ts`, `shared/protocol/package.json`, `scripts/gen-protocol.sh`, `scripts/gen-protocol.ps1`.
  - Prerequisites: P0-T01.
  - Acceptance: `pnpm gen:protocol` regenerates the same byte-for-byte TS file as is checked in; CI runs the generator and `git diff` exits 0.
  - Complexity: S. Agent: general.

---

## Phase 1 - Local storage + media library (no networking)

Goal: import a file, hash it, probe it, and play it locally through a `locast://` URL - all offline. The library UI shows what is on disk.

- **P1-T01 - `core::library` sanitization rules.**
  - Goal: pure-Rust filename sanitization (section 6) with zero I/O.
  - Files / modules: `apps/client/src-tauri/src/core/library/sanitize.rs`, `apps/client/src-tauri/src/core/library/mod.rs`.
  - Prerequisites: P0-T05.
  - Acceptance: unit tests cover Windows reserved names, control characters, leading/trailing dots, length cap, NFC normalization, and a `..` segment; all paths return either Ok(sanitized) or Err(InvalidFilename).
  - Complexity: S. Agent: backend.

- **P1-T02 - Filesystem operations + atomic rename.**
  - Goal: a `library::fs` module that places a file at the correct content-addressed path and atomically renames from staging.
  - Files / modules: `apps/client/src-tauri/src/library/fs.rs`, `apps/client/src-tauri/src/core/paths.rs`.
  - Prerequisites: P1-T01.
  - Acceptance: integration test in a `tempfile::TempDir` stages a file under `tmp/staging`, calls `complete_download(sha, src, dst)`, and asserts the file is at `library/<sha[0..2]>/<sha[2..4]>/<sha>/<sanitized>`; a second concurrent call is rejected.
  - Complexity: M. Agent: backend.

- **P1-T03 - Hashing primitives (blake3, sha256).**
  - Goal: streaming BLAKE3 and SHA-256 helpers used by both the scanner and the download verifier.
  - Files / modules: `apps/client/src-tauri/src/core/hashing.rs`, `shared/crypto/src/blake3.rs`, `shared/crypto/Cargo.toml`.
  - Prerequisites: P0-T01.
  - Acceptance: unit test that hashing the same 1 GiB random buffer twice produces identical digests; a chunked hash test that splits a buffer into 256 KiB chunks and asserts per-chunk SHA-256 + final BLAKE3 are stable.
  - Complexity: S. Agent: backend.

- **P1-T04 - `media_import` command + IPC binding.**
  - Goal: the `media_import(paths: string[])` Tauri command copies a user-picked file into the library, hashes it, inserts a `media_items` row, and returns the new media id.
  - Files / modules: `apps/client/src-tauri/src/commands/import.rs`, `apps/client/src-tauri/src/commands/mod.rs`, `apps/client/src/services/import.ts`.
  - Prerequisites: P1-T02, P1-T03, P0-T05.
  - Acceptance: an integration test (or a manual Tauri dev session) imports two files with identical bytes; the second one dedupes via hardlink/copy and the `media_items` table contains two rows pointing to the same on-disk file; the TS binding types match.
  - Complexity: M. Agent: backend.

- **P1-T05 - Disk-quota enforcement.**
  - Goal: quota key, recompute on import, reject new imports that exceed it.
  - Files / modules: `apps/client/src-tauri/src/core/quota.rs`, `apps/client/src-tauri/src/storage/settings.rs`.
  - Prerequisites: P1-T04.
  - Acceptance: with `library.quota_bytes` set to a small value, an import that would exceed it returns `AppError::QuotaExceeded { used, cap, needed }`; raising the cap allows the import.
  - Complexity: S. Agent: backend.

- **P1-T06 - Optional ffmpeg probe sidecar.**
  - Goal: a `probe::run(path) -> ProbeResult` that shells out to ffmpeg only if the binary is present, returns `None` otherwise.
  - Files / modules: `apps/client/src-tauri/src/probe/mod.rs`, `apps/client/src-tauri/src/probe/ffprobe.rs`.
  - Prerequisites: P1-T04.
  - Acceptance: with no ffmpeg on PATH, the probe returns `None` and the import still succeeds; with a stub ffmpeg that prints JSON, the parse populates `duration_ms`, `width`, `height`, `video_codec`, `audio_codec`, `container`.
  - Complexity: S. Agent: backend.

- **P1-T07 - Library scanner + FTS5 index.**
  - Goal: a `library_scan()` command walks the library root, hashes each media file, upserts `media_items`, and keeps the FTS5 index in sync.
  - Files / modules: `apps/client/src-tauri/src/library/scan.rs`, `apps/client/src-tauri/src/commands/scan.rs`.
  - Prerequisites: P1-T04, P1-T05.
  - Acceptance: integration test with a `tempfile::TempDir` containing 50 fixture files of varying sizes; after `library_scan`, `SELECT COUNT(*) FROM media_items = 50`; FTS5 returns the expected matches for `MATCH 'movie*'`.
  - Complexity: M. Agent: backend.

- **P1-T08 - `locast://` custom protocol + Range support.**
  - Goal: register the `locast://` URI scheme per section 5; serve media files with `Content-Type`, `Accept-Ranges: bytes`, and HTTP `Range` (single range, returns 206).
  - Files / modules: `apps/client/src-tauri/src/library/protocol.rs`, `apps/client/src-tauri/src/app.rs`.
  - Prerequisites: P1-T07.
  - Acceptance: integration test constructs a 4 GiB sparse file, requests `locast://media/<sha-prefix>/<name>`; the handler returns 206 with the requested range and the correct `Content-Type`; a request with an out-of-library path returns 403.
  - Complexity: M. Agent: backend.

- **P1-T09 - Library page UI (grid + search).**
  - Goal: a `/library` route shows the grid of media items, supports FTS5-backed search, and supports delete + permanent toggles.
  - Files / modules: `apps/client/src/pages/library/`, `apps/client/src/stores/useMediaStore.ts`, `apps/client/src/services/mediaCatalog.ts`.
  - Prerequisites: P1-T07, P0-T06.
  - Acceptance: component test asserts the grid renders N tiles for N items, search filters in < 50 ms, the "Make permanent" action flips a Zustand store + SQLite row, and an a11y test confirms the grid is keyboard navigable.
  - Complexity: M. Agent: frontend.

- **P1-T10 - Local `<video>` playback through `locast://`.**
  - Goal: a `/rooms/:id` route (no networking yet) mounts a `<video src={locast://...}>` and plays the file.
  - Files / modules: `apps/client/src/pages/rooms.$id/`, `apps/client/src/components/Player.tsx`.
  - Prerequisites: P1-T08, P1-T09.
  - Acceptance: manual test in `pnpm tauri dev` plays a local mp4 from the library with audio and video; the seek bar works (proves Range requests); the player registers and listens to a no-op `room://event` channel for the next phase.
  - Complexity: M. Agent: frontend.

---

## Phase 2 - Identity, signaling, room lifecycle

Goal: a server that can auth a client over WSS, a client that holds an Ed25519 keypair in the OS keyring, and a working room create / join / leave / list flow.

- **P2-T01 - Identity keypair (Ed25519, OS keyring).**
  - Goal: `identity_get()` returns the local pubkey and display name; on first launch a keypair is generated and stored in the OS keystore.
  - Files / modules: `apps/client/src-tauri/src/identity/mod.rs`, `apps/client/src-tauri/src/identity/keystore.rs`, `apps/client/src-tauri/src/commands/identity.rs`.
  - Prerequisites: P0-T02.
  - Acceptance: a fresh test app launches and `identity_get()` returns a non-null pubkey; a second launch returns the same pubkey (keypair is persistent); a "Rotate" action generates a new keypair.
  - Complexity: M. Agent: backend.

- **P2-T02 - Server auth: HELLO/WELCOME/CHALLENGE/AUTH_OK.**
  - Goal: the WebSocket endpoint completes the handshake from section 18.4.1 with Ed25519 signature verification and a bearer token.
  - Files / modules: `apps/server/src/ws/mod.rs`, `apps/server/src/auth/`, `apps/server/src/db.rs`, `apps/server/migrations/0001_init.sql`.
  - Prerequisites: P0-T03.
  - Acceptance: integration test (server in a thread + a tungstenite client) completes the handshake, server issues a bearer, the bearer is required for subsequent messages; a forged signature is rejected with `AUTH_FAIL("bad_sig")`.
  - Complexity: M. Agent: backend.

- **P2-T03 - Client WS client + reconnect backoff.**
  - Goal: the client has a `SignalingClient` that connects, completes the handshake, holds the bearer, and reconnects on disconnect with the 1s->30s exponential backoff (section 22.3.1).
  - Files / modules: `apps/client/src-tauri/src/net/signaling.rs`, `apps/client/src-tauri/src/net/reconnect.rs`.
  - Prerequisites: P2-T01, P2-T02.
  - Acceptance: unit test drives a fake WS server through 5 connect/disconnect cycles; the client reconnects with the expected backoff schedule (within +/-20% jitter tolerance) and never gives up.
  - Complexity: M. Agent: backend.

- **P2-T04 - Server: room registry + create/join/leave.**
  - Goal: `ROOM_CREATE`, `ROOM_JOIN_REQUEST`, `ROOM_JOINED`, `ROOM_LEAVE` are wired in the server with code generation, password (Argon2id), and participant list.
  - Files / modules: `apps/server/src/rooms/mod.rs`, `apps/server/src/rooms/codes.rs`, `apps/server/src/rooms/registry.rs`.
  - Prerequisites: P2-T02.
  - Acceptance: integration test creates a room, two clients join it, the server broadcasts `PEER_ANNOUNCE`; the room code is 6 chars in the alphabet from section 10.2 and never contains `0`, `O`, `1`, `I`, `L`.
  - Complexity: M. Agent: backend.

- **P2-T05 - Client: room create/join UI.**
  - Goal: `/rooms/new` and `/rooms/join` screens; a click on "Create" calls `ROOM_CREATE` and routes to `/rooms/:id` (which currently shows a placeholder, see P2-T06).
  - Files / modules: `apps/client/src/pages/rooms.new/`, `apps/client/src/pages/rooms.join/`, `apps/client/src/services/roomClient.ts`.
  - Prerequisites: P2-T04, P2-T03.
  - Acceptance: component test renders both pages, the form validation rejects empty display names and codes with invalid characters, the "Create" button calls the typed IPC and routes correctly.
  - Complexity: M. Agent: frontend.

- **P2-T06 - Room view placeholder + presence event hookup.**
  - Goal: `/rooms/:id` renders an empty player area, a participant strip, and a footer status; presence messages from the server populate the strip.
  - Files / modules: `apps/client/src/pages/rooms.$id/`, `apps/client/src/stores/useRoomStore.ts`, `apps/client/src/services/roomClient.ts`.
  - Prerequisites: P2-T05, P1-T10.
  - Acceptance: two Tauri dev windows connected to the same dev server each see the other in the participant strip within 5 seconds of joining; one leaves and the other sees them drop.
  - Complexity: M. Agent: frontend.

- **P2-T07 - Server: rate limiter + capability gate.**
  - Goal: per-connection token bucket (100 msg/s, 200 burst, 1 MB/s, 2 MB burst) and `check_capability` for the initial command set.
  - Files / modules: `apps/server/src/ratelimit/mod.rs`, `apps/server/src/rooms/caps.rs`.
  - Prerequisites: P2-T04.
  - Acceptance: a flood test sends 500 msg/s; the server responds with `RATE_LIMIT` on the offender's connection only, and other participants continue unaffected.
  - Complexity: M. Agent: backend.

- **P2-T08 - /rooms list + persistent recent rooms.**
  - Goal: the `/rooms` screen lists active and recent rooms; the client persists room history to SQLite (so restart shows the same list).
  - Files / modules: `apps/client/src/pages/rooms/index.tsx`, `apps/client/src-tauri/src/storage/rooms.rs`.
  - Prerequisites: P2-T06, P0-T05.
  - Acceptance: a client that creates a room, restarts, and visits `/rooms` still sees the room; an ended room shows up under "Recent" with the host's display name.
  - Complexity: S. Agent: frontend.

---

## Phase 3 - Manifest publishing + viewer download flow (P2P DataChannel transfers, chunking, verification)

Goal: a host publishes a signed manifest; a viewer downloads the file over a P2P DataChannel, verifies chunk hashes, atomic-renames it into the library, and the local player can play it.

- **P3-T01 - Manifest canonical form (Rust).**
  - Goal: `manifest::canonical::serialize` produces the canonical bytes from section 8 with a golden vector test.
  - Files / modules: `shared/manifest/src/canonical.rs`, `shared/manifest/src/lib.rs`, `shared/manifest/tests/golden_canonical.json`.
  - Prerequisites: P0-T07.
  - Acceptance: a unit test serializes a known manifest, hashes the bytes (the canonical-form commit), and asserts equality with a checked-in golden vector; non-NFC, NaN, or whitespace variants produce different bytes.
  - Complexity: M. Agent: backend.

- **P3-T02 - Manifest signing + verification (Ed25519).**
  - Goal: `sign_manifest(sk, &manifest) -> SignedManifest` and `verify_manifest(&SignedManifest) -> Result<(), Error>`.
  - Files / modules: `shared/manifest/src/signing.rs`, `shared/crypto/src/ed25519.rs`.
  - Prerequisites: P3-T01, P2-T01.
  - Acceptance: a roundtrip test signs and verifies successfully; a test that flips a single byte in the payload fails verification; a test that flips the signature bytes fails verification.
  - Complexity: S. Agent: backend.

- **P3-T03 - Host: `room_create` with manifest; `MANIFEST_PUBLISH` over WS.**
  - Goal: the host's `room_create` flow picks a media item, builds and signs the manifest, and the server stores it and broadcasts `MANIFEST_PUBLISH`.
  - Files / modules: `apps/client/src-tauri/src/room/host.rs`, `apps/client/src-tauri/src/commands/room.rs`, `apps/client/src-tauri/src/storage/manifests.rs`, `apps/server/src/rooms/manifest.rs`.
  - Prerequisites: P3-T02, P2-T04.
  - Acceptance: two clients (host + viewer) on the dev server; host picks a file, the viewer receives `MANIFEST_PUBLISH` over WS, the viewer verifies the signature against the host's pubkey from the room invite.
  - Complexity: M. Agent: backend.

- **P3-T04 - Chunk planner + bitmap persistence.**
  - Goal: `transfer::plan(sha256, size, chunk_size) -> ChunkPlan` and the `downloads` + `download_chunks` schema are populated.
  - Files / modules: `apps/client/src-tauri/src/transfer/plan.rs`, `apps/client/src-tauri/src/transfer/mod.rs`, `apps/client/src-tauri/src/storage/downloads.rs`.
  - Prerequisites: P0-T05.
  - Acceptance: a unit test for a 9 GiB file with `chunk_size=262144` produces 34356 chunks; each chunk's `offset` and `length` line up; the resulting `downloads` row and `download_chunks` rows are inserted in a single transaction.
  - Complexity: S. Agent: backend.

- **P3-T05 - WebRTC PeerConnection lifecycle (Rust + webrtc-rs).**
  - Goal: open an `RTCPeerConnection` to one peer, create the `files` DataChannel, exchange SDP/ICE via the server's `SIGNAL` messages.
  - Files / modules: `apps/client/src-tauri/src/net/webrtc.rs`, `apps/client/src-tauri/src/net/signaling.rs`.
  - Prerequisites: P2-T03.
  - Acceptance: two clients in the dev server; `pc.connectionState` reaches `connected` within 10 s on a typical LAN; the `files` DataChannel emits `open`; the connection order is deterministic by `user_id` (section 19.2.3).
  - Complexity: L. Agent: backend.

- **P3-T06 - DOWNLOAD_OFFER / DOWNLOAD_CHUNK / DOWNLOAD_ACK wire protocol.**
  - Goal: framing for the `media` DataChannel per section 9; viewer requests chunks, source sends them with per-chunk hash, viewer verifies and ACKs.
  - Files / modules: `apps/client/src-tauri/src/transfer/session.rs`, `apps/client/src-tauri/src/transfer/wire.rs`.
  - Prerequisites: P3-T05, P3-T04.
  - Acceptance: an integration test (loopback transport, 5% loss, 50 ms jitter) transfers a 50 MB fixture from source to viewer; every chunk's SHA-256 verifies; the final BLAKE3 matches the manifest; the file is atomic-renamed into the library.
  - Complexity: L. Agent: backend.

- **P3-T07 - Sliding window + backpressure.**
  - Goal: `W=16` outstanding requests, soft backpressure on `bufferedAmount > 2 MiB`, host's per-peer token bucket `B=4`.
  - Files / modules: `apps/client/src-tauri/src/transfer/scheduler.rs`.
  - Prerequisites: P3-T06.
  - Acceptance: an integration test simulates a slow viewer; the source pauses sends when `bufferedAmount` exceeds the threshold and resumes on `onbufferedamountlow`; windowed requests never exceed 16 in flight.
  - Complexity: M. Agent: backend.

- **P3-T08 - Download progress + state events to the webview.**
  - Goal: `download://progress` at <=5 Hz, `download://state` immediately, payload shape from section 4.
  - Files / modules: `apps/client/src-tauri/src/transfer/events.rs`, `apps/client/src/events.rs`, `apps/client/src/stores/useDownloadStore.ts`.
  - Prerequisites: P3-T07, P0-T06.
  - Acceptance: during a real download the React side logs no more than 5 progress events per second per download; state transitions arrive within 50 ms of the underlying change.
  - Complexity: S. Agent: backend.

- **P3-T09 - Multi-source selection + bitmap merge.**
  - Goal: a viewer pulls chunks from the lowest-priority reachable peer first; on 3 NAKs or RTT > 2 s for 10 s, rotate to the next source.
  - Files / modules: `apps/client/src-tauri/src/transfer/multi_source.rs`.
  - Prerequisites: P3-T08.
  - Acceptance: a 2-source integration test where the first source is silently dropping 10% of chunks still completes within the retry budget, and the failed chunks are pulled from the second source.
  - Complexity: M. Agent: backend.

- **P3-T10 - Download progress modal in the React UI.**
  - Goal: the `DownloadProgressModal` is non-dismissable while a download is active and blocks the route to `/rooms/:id`.
  - Files / modules: `apps/client/src/components/DownloadProgressModal.tsx`, `apps/client/src/pages/rooms.$id/route.tsx`.
  - Prerequisites: P3-T08.
  - Acceptance: a Playwright Vite-harness test imports a file via manifest, asserts the modal appears, asserts it cannot be closed via Escape or backdrop click, asserts it closes only when state is `complete`.
  - Complexity: S. Agent: frontend.

- **P3-T11 - Library dedup on download (hardlink / copy fallback).**
  - Goal: section 23.3 - if a downloaded `sha256` already exists locally, do not re-fetch; on Windows use a copy (v1), on POSIX prefer hardlink.
  - Files / modules: `apps/client/src-tauri/src/library/dedup.rs`.
  - Prerequisites: P3-T06.
  - Acceptance: a host and a viewer that already had the file (from a prior room) re-join a new room with the same file; the viewer marks the item "local" in the UI and never opens a transfer session.
  - Complexity: S. Agent: backend.

---

## Phase 4 - Playback engine + room control (play/pause/seek, position reports, drift UI, manual sync)

Goal: the host presses Play; viewers see the same media at the same position; drift is visible; manual sync works.

- **P4-T01 - PLAYBACK_CMD server validation + ordering.**
  - Goal: section 13 message types PL]AY, PAUSE, SEEK accepted only from the host (or a delegated co-host), `server_seq` assigned, `server_ts` stamped.
  - Files / modules: `apps/server/src/rooms/playback.rs`, `apps/server/src/rooms/registry.rs`.
  - Prerequisites: P2-T07, P3-T03.
  - Acceptance: an integration test sends PLAY/PAUSE/SEEK from a non-host; the server replies `ERROR("forbidden")` to that client and does not broadcast; the same commands from the host are accepted and broadcast in order.
  - Complexity: M. Agent: backend.

- **P4-T02 - Client playback command application.**
  - Goal: incoming PLAY/PAUSE/SEEK commands drive the `<video>` element via the command-application layer (no auto-seek beyond what the command says).
  - Files / modules: `apps/client/src/components/Player.tsx`, `apps/client/src/stores/useRoomStore.ts`, `apps/client/src/services/playback.ts`.
  - Prerequisites: P3-T10, P1-T10.
  - Acceptance: a two-client E2E test on the Vite harness: host clicks Play, the viewer's `<video>.paused` is `false` within 200 ms; host clicks Pause, viewer's `<video>.paused` is `true`; host seeks to 60s, viewer's `currentTime` is in [59.9, 60.1].
  - Complexity: M. Agent: frontend.

- **P4-T03 - POSITION_REPORT at 1 Hz.**
  - Goal: each client sends a `POSITION_REPORT{ media_position_ms, playing }` every second; the server forwards without modification.
  - Files / modules: `apps/client/src/services/playback.ts`, `apps/client/src-tauri/src/room/report.rs`, `apps/server/src/rooms/presence.rs`.
  - Prerequisites: P4-T01.
  - Acceptance: a two-client dev session; both clients log 1 POSITION_REPORT/sec; the host's UI shows the viewer's position updating each second.
  - Complexity: S. Agent: backend.

- **P4-T04 - Drift detection + drift UI.**
  - Goal: 1 Hz drift sampler; thresholds at 200 ms / 2 s / 5 s; non-blocking toasts; drift indicator visible only when smoothed offset > 2 s.
  - Files / modules: `apps/client/src/hooks/useDriftSmoother.ts`, `apps/client/src/components/DriftIndicator.tsx`.
  - Prerequisites: P4-T03.
  - Acceptance: a unit test feeds a noisy signal into the smoother and asserts the threshold behavior; a Playwright test asserts the indicator is hidden when drift is < 2 s, visible otherwise.
  - Complexity: M. Agent: frontend.

- **P4-T05 - Manual sync ("Sync to Host").**
  - Goal: a button that does an instant local seek to the host's last reported position; issues a `SEEK{ reason: "manual_sync" }` if the user has `playback.issue_commands`, otherwise stays local.
  - Files / modules: `apps/client/src/components/SyncButton.tsx`, `apps/client/src/services/playback.ts`.
  - Prerequisites: P4-T02, P4-T03.
  - Acceptance: a Playwright test with the user lacking playback capability clicks "Sync to Host"; the user's `currentTime` jumps but no SEEK command is emitted on the WS; a test with the user having the capability emits the SEEK and the room rebroadcasts a presence event.
  - Complexity: S. Agent: frontend.

- **P4-T06 - NTP-style clock skew measurement.**
  - Goal: every 60 s, take 4 RTT samples, take the median, store `skew_ms` and `jitter_ms`; apply to drift math.
  - Files / modules: `apps/client/src-tauri/src/room/skew.rs`, `apps/client/src/hooks/useDriftSmoother.ts`.
  - Prerequisites: P4-T04.
  - Acceptance: a unit test injects a fake RTT stream and asserts the median is taken; samples with RTT > 500 ms are rejected; jitter_ms > 200 ms widens the drift threshold.
  - Complexity: S. Agent: backend.

- **P4-T07 - Deduplication (per-sender monotonic_seq).**
  - Goal: client tracks `last_applied_seq[user_id]`; duplicates are dropped, gaps buffered up to 5 s, then applied.
  - Files / modules: `apps/client/src/services/playback.ts`, `apps/client/src-tauri/src/room/dedup.rs`.
  - Prerequisites: P4-T01.
  - Acceptance: a unit test replays a stream with a duplicate seq, a gap, and an out-of-order seq; duplicates are dropped, the gap is filled within 5 s, the out-of-order SEEK is dropped.
  - Complexity: S. Agent: backend.

- **P4-T08 - Server: presence + 15 s heartbeat.**
  - Goal: PRESENCE messages every 5 s; 3 missed = DISCONNECTED; PEER_LEAVE broadcast.
  - Files / modules: `apps/server/src/rooms/presence.rs`, `apps/client/src-tauri/src/room/heartbeat.rs`.
  - Prerequisites: P2-T04, P4-T03.
  - Acceptance: an integration test with one client that stops sending PRESENCE; after 15 s the server marks it DISCONNECTED and broadcasts PEER_LEAVE; a 5-min wait removes it from the in-memory state.
  - Complexity: M. Agent: backend.

---

## Phase 5 - Drawing + laser pointer

Goal: vector drawing on a transparent canvas above the video; a transient laser pointer for presence; undo.

- **P5-T01 - Drawing canvas + pointer pipeline.**
  - Goal: a `<canvas>` overlay above the `<video>`, sized to the video's intrinsic dimensions, redrawn on resize.
  - Files / modules: `apps/client/src/components/DrawingLayer.tsx`, `apps/client/src/hooks/useDrawingCanvas.ts`.
  - Prerequisites: P1-T10.
  - Acceptance: a Playwright test moves a pointer over the canvas with the pen tool; the resulting SVG path is correct (verified by re-rendering from the stroke history); a window resize redraws the canvas without flicker.
  - Complexity: M. Agent: frontend.

- **P5-T02 - DRAW_BEGIN / DRAW_POINT / DRAW_END over WS.**
  - Goal: pointer down emits a signed DRAW_BEGIN; pointer move emits DRAW_POINT at <=120 Hz with last-point-wins coalescing; pointer up emits DRAW_END.
  - Files / modules: `apps/client/src/services/drawing.ts`, `apps/server/src/rooms/drawing.rs`.
  - Prerequisites: P5-T01, P2-T07.
  - Acceptance: a Playwright test draws 200 points in 1 s; the WS trace shows <=120 DRAW_POINT messages; the server rebroadcasts to all other participants within 50 ms.
  - Complexity: M. Agent: backend.

- **P5-T03 - Undo + clear_all.**
  - Goal: `stroke_undo` (own stroke requires `drawing.undo_own`, any stroke requires `drawing.undo_any`); `clear_all` requires `drawing.clear_all` and clears all strokes.
  - Files / modules: `apps/client/src/services/drawing.ts`, `apps/server/src/rooms/drawing.rs`.
  - Prerequisites: P5-T02, P4-T01.
  - Acceptance: a Playwright test where two users draw, then one undoes the other's stroke (capability granted); the stroke disappears for both clients. Clear_all wipes the canvas; receiving clients reflect it within 100 ms.
  - Complexity: M. Agent: backend.

- **P5-T04 - Laser pointer rendering + animation.**
  - Goal: a separate canvas layer that renders a fading polyline trail of the last 20 positions; red dot at the head; `requestAnimationFrame` recompute.
  - Files / modules: `apps/client/src/components/LaserPointer.tsx`, `apps/client/src/hooks/useLaserTrail.ts`.
  - Prerequisites: P5-T02.
  - Acceptance: a Playwright test holds the laser key and moves the mouse; 16 simultaneous lasers (one per fake participant) all render within the 16 ms frame budget; releasing fades the trail over 200 ms.
  - Complexity: M. Agent: frontend.

- **P5-T05 - Laser color assignment by user_id hash.**
  - Goal: the local user is red; others are assigned from a 12-color palette by deterministic hash of `user_id`.
  - Files / modules: `apps/client/src/utils/laserColor.ts`, `apps/client/src/components/LaserPointer.tsx`.
  - Prerequisites: P5-T04.
  - Acceptance: unit test on `laserColor(user_id)` for a known set of UUIDs asserts the same color every time; the active drawing user's laser transitions to red on the next `stroke_begin`.
  - Complexity: S. Agent: frontend.

- **P5-T06 - Drawing toolbar + keyboard shortcuts.**
  - Goal: `d` toggles the toolbar; `l` activates the laser; tools (pen, eraser, arrow, rect, circle, text) all work.
  - Files / modules: `apps/client/src/components/DrawingToolbar.tsx`, `apps/client/src/hooks/useKeyboardScope.ts`.
  - Prerequisites: P5-T01.
  - Acceptance: a Playwright test presses `d`, the toolbar is visible and focused; presses `l` while drawing, the laser is suppressed locally; pressing `l` over an empty canvas shows the local laser.
  - Complexity: S. Agent: frontend.

---

## Phase 6 - Permissions, chat, presence polish

Goal: capability enforcement end-to-end, server-relayed chat, and the participant strip with avatars and connection quality.

- **P6-T01 - `can()` chokepoint + capability table.**
  - Goal: a single function on both client and server that takes (participant, scope, action) and returns bool; the server is the source of truth, the client mirrors.
  - Files / modules: `apps/client/src-tauri/src/room/caps.rs`, `apps/server/src/rooms/caps.rs`.
  - Prerequisites: P4-T01.
  - Acceptance: a unit test covers all (scope, action) tuples; the host returns true for everything; a viewer with no caps returns false for everything; a viewer with a preset returns the expected set.
  - Complexity: S. Agent: backend.

- **P6-T02 - PERMISSION_SET / CAPABILITY_UPDATE wire protocol.**
  - Goal: host can grant/revoke individual capabilities; CAPABILITY_UPDATE is broadcast; the client mirror is updated.
  - Files / modules: `apps/client/src/services/permissions.ts`, `apps/server/src/rooms/permissions.rs`.
  - Prerequisites: P6-T01, P5-T03.
  - Acceptance: a Playwright test where the host grants `drawing.draw` to a viewer; the viewer's toolbar activates; revoking it hides the toolbar on the next render.
  - Complexity: M. Agent: backend.

- **P6-T03 - Chat (CHAT_SEND / CHAT_MSG).**
  - Goal: server-relayed chat with a 2 KiB text limit and optional reply-to.
  - Files / modules: `apps/client/src/components/ChatPanel.tsx`, `apps/client/src/services/chat.ts`, `apps/server/src/rooms/chat.rs`.
  - Prerequisites: P2-T07.
  - Acceptance: a Playwright test where host and viewer exchange 10 messages; both see the same chat history; messages > 2 KiB are rejected at the schema layer.
  - Complexity: M. Agent: backend.

- **P6-T04 - Participant strip UI (avatar, name, quality bar).**
  - Goal: each participant renders with display name, an avatar (initial), a connection quality bar (driven by RTT to the server), and a host badge.
  - Files / modules: `apps/client/src/components/ParticipantTile.tsx`, `apps/client/src/hooks/useConnectionQuality.ts`.
  - Prerequisites: P2-T06, P4-T08.
  - Acceptance: a Playwright test asserts the strip shows N tiles for N participants; the host has a "Host" badge; the quality bar updates within 5 s when the user's network is throttled.
  - Complexity: S. Agent: frontend.

- **P6-T05 - Capability presets (Viewer / Editor / Co-host).**
  - Goal: a host can apply a preset to a participant; per-user overrides take precedence.
  - Files / modules: `apps/client/src/components/PermissionsModal.tsx`, `apps/client/src-tauri/src/room/presets.rs`.
  - Prerequisites: P6-T02.
  - Acceptance: a Playwright test applies the "Co-host" preset; the new co-host can issue PLAYBACK_CMD and the server accepts it.
  - Complexity: S. Agent: frontend.

- **P6-T06 - "Keep or Delete" leave-room modal.**
  - Goal: on room leave, the user is shown a modal listing `temporary` files from the room with Keep / Delete / Cancel actions.
  - Files / modules: `apps/client/src/components/LeaveRoomModal.tsx`, `apps/client/src/services/roomClient.ts`.
  - Prerequisites: P3-T11, P1-T09.
  - Acceptance: a Playwright test where the user has 3 temporary files from the room; clicking Delete moves all to OS trash and flips status; clicking Keep flips status to permanent.
  - Complexity: S. Agent: frontend.

---

## Phase 7 - Reconnection, host transfer, edge cases

Goal: the system stays up across the realistic failure modes.

- **P7-T01 - WS reconnect on the client.**
  - Goal: 1s->30s exponential backoff with +/-20% jitter; resume token; do not regress downloads to `pending`.
  - Files / modules: `apps/client/src-tauri/src/net/reconnect.rs`, `apps/client/src-tauri/src/net/signaling.rs`.
  - Prerequisites: P2-T03, P3-T08.
  - Acceptance: a chaos test kills the server for 10 s; the client reconnects within 30 s; the downloads resume from the existing `download_chunks` bitmap.
  - Complexity: M. Agent: backend.

- **P7-T02 - Stale participants (5 min removal).**
  - Goal: 5 min after DISCONNECTED, the participant is removed from the room in-memory and on disk.
  - Files / modules: `apps/server/src/rooms/registry.rs`, `apps/server/src/rooms/cleanup.rs`.
  - Prerequisites: P4-T08.
  - Acceptance: an integration test with a participant that has been DISCONNECTED for 5 min; the server's `participants` table no longer has the row, and `peers_alive` is empty.
  - Complexity: S. Agent: backend.

- **P7-T03 - Host disconnect grace period + transfer (v1.1 stub).**
  - Goal: 30 s grace; during grace, the room shows "host reconnecting"; after 30 s, the room ends (v1 behavior, per section 22.6).
  - Files / modules: `apps/server/src/rooms/host.rs`, `apps/client/src/components/RoomTopBar.tsx`.
  - Prerequisites: P4-T08, P2-T04.
  - Acceptance: an integration test with a host that disconnects; viewers see "host reconnecting" within 5 s; after 30 s, the room transitions to ENDED; viewers see a toast.
  - Complexity: M. Agent: backend.

- **P7-T04 - Pause / resume a download across restart.**
  - Goal: close the app mid-download; reopen; the in-flight download resumes from the persisted `download_chunks` bitmap.
  - Files / modules: `apps/client/src-tauri/src/transfer/resume.rs`, `apps/client/src-tauri/src/commands/transfer.rs`.
  - Prerequisites: P3-T07.
  - Acceptance: an integration test starts a download, pauses it at 30% by killing the process, restarts, and asserts the `hello` frame's bitmap matches and the download completes.
  - Complexity: M. Agent: backend.

- **P7-T05 - Process crash recovery.**
  - Goal: a panic in the transfer engine is caught at the `tokio::spawn` boundary and the download transitions to `failed` with `last_error = "internal: panic"`.
  - Files / modules: `apps/client/src-tauri/src/transfer/panic_boundary.rs`.
  - Prerequisites: P3-T07.
  - Acceptance: a unit test spawns a tokio task that panics; the transfer session transitions to `failed` and the rest of the app is unaffected.
  - Complexity: S. Agent: backend.

- **P7-T06 - Reconnect during playback: "Sync to host" already works (P4-05) but add a UI affordance to show last-known position.**
  - Goal: when the WS is down, the footer shows the last known host position and a "host offline" badge.
  - Files / modules: `apps/client/src/components/RoomFooter.tsx`, `apps/client/src/stores/useRoomStore.ts`.
  - Prerequisites: P7-T01, P4-T05.
  - Acceptance: a Playwright test that disconnects the server for 10 s; the footer shows "host offline" and the last known position; reconnecting within 5 s restores the live state.
  - Complexity: S. Agent: frontend.

- **P7-T07 - Manifest re-publish on host reconnect with new version.**
  - Goal: if the host's local file changes mid-room, the host can re-publish; viewers re-download the affected chunks.
  - Files / modules: `apps/client/src-tauri/src/room/host.rs`, `apps/server/src/rooms/manifest.rs`.
  - Prerequisites: P3-T03, P4-T01.
  - Acceptance: a Playwright test where the host replaces one of the media files in the library and re-publishes; the viewer receives the new manifest version and re-downloads the changed chunks.
  - Complexity: M. Agent: backend.

---

## Phase 8 - Security hardening, fuzzing, perf

Goal: the system is safe to ship, fast on commodity hardware, and the supply chain is auditable.

- **P8-T01 - Path traversal validator + fuzz target.**
  - Goal: the path validation function from section 21.7 is a single function in `core/` and is fuzzed.
  - Files / modules: `apps/client/src-tauri/src/core/paths.rs`, `apps/client/src-tauri/fuzz/fuzz_targets/path_validator.rs`.
  - Prerequisites: P1-T08.
  - Acceptance: `cargo fuzz run path_validator -- -max_total_time=60` runs without finding a crash; an explicit battery of crafted paths (`..`, absolute, symlinks, junctions, UNC, NUL) all return the expected reject.
  - Complexity: M. Agent: security.

- **P8-T02 - Protocol message fuzzing.**
  - Goal: a `cargo-fuzz` target for the MessagePack protocol decoder.
  - Files / modules: `apps/client/src-tauri/src/net/wire.rs`, `apps/client/src-tauri/fuzz/fuzz_targets/wire_decode.rs`.
  - Prerequisites: P2-T03.
  - Acceptance: `cargo fuzz run wire_decode -- -max_total_time=60` runs without crash; a known malformed message is in the corpus and is rejected with `ERROR("bad_msg")`.
  - Complexity: M. Agent: security.

- **P8-T03 - Subtitle parser fuzzing.**
  - Goal: fuzz the SRT, WebVTT, and ASS parsers.
  - Files / modules: `apps/client/src-tauri/src/media/subtitles/`, `apps/client/src-tauri/fuzz/fuzz_targets/subtitle_srt.rs` (and vtt, ass).
  - Prerequisites: P0-T01.
  - Acceptance: 60 s of fuzzing on each parser with no crashes; a known malicious subtitle (BOM, negative timestamp, 4 GB file size string) is rejected without panicking.
  - Complexity: M. Agent: security.

- **P8-T04 - `cargo-deny` + `pnpm audit` in CI.**
  - Goal: CI fails on new advisories or banned licenses.
  - Files / modules: `deny.toml`, `.github/workflows/ci.yml`.
  - Prerequisites: P0-T04.
  - Acceptance: `cargo deny check` passes locally; CI runs `cargo deny check` and `pnpm audit --prod`; a new high-severity advisory on a tracked dep causes CI to fail.
  - Complexity: S. Agent: security.

- **P8-T05 - Secret handling and log redaction.**
  - Goal: env vars marked `sensitive` (TURN_SECRET, DB_KEY) are read once and zeroized; the audit log redacts bearer tokens, passwords, private key material, and TURN credentials.
  - Files / modules: `apps/server/src/config.rs`, `apps/server/src/audit/`.
  - Prerequisites: P2-T02.
  - Acceptance: a test loads a config with sensitive values; after the config struct is dropped, the memory is checked (best-effort); the audit log writer strips fields named `bearer`, `password`, `private_key`, `credential`, `signature`.
  - Complexity: M. Agent: security.

- **P8-T06 - Perf bench harness + regression gates.**
  - Goal: criterion benches for the hot paths in section 28.10; CI fails on a >10% regression.
  - Files / modules: `apps/client/src-tauri/benches/`, `scripts/check-perf.sh`, `.github/workflows/ci.yml`.
  - Prerequisites: P3-T07, P1-T07, P0-T04.
  - Acceptance: `cargo bench` runs all hot-path benches; the persisted baseline is checked in; the CI step compares new results and fails on regression.
  - Complexity: M. Agent: perf.

- **P8-T07 - Webview Performance trace capture.**
  - Goal: a Playwright trace on a canonical scenario is uploaded as a CI artifact; long tasks > 50 ms are flagged.
  - Files / modules: `apps/client/tests/perf/`, `.github/workflows/ci.yml`.
  - Prerequisites: P5-T01, P1-T10.
  - Acceptance: a Playwright test traces a 60 s playback session with 5-minute drawing activity; the trace is uploaded; a script flags any long task > 50 ms.
  - Complexity: M. Agent: perf.

- **P8-T08 - Hardening checklist verification.**
  - Goal: the items in section 21.17 are present and verified in CI.
  - Files / modules: `.github/workflows/ci.yml`, `apps/server/Dockerfile`, `apps/client/src-tauri/Cargo.toml`.
  - Prerequisites: P8-T04, P8-T05.
  - Acceptance: the CI job asserts: binary is compiled with the hardening RUSTFLAGS; the Dockerfile runs as a non-root user; cgroup limits are in compose; the cargo-deny check is clean; the no-`unsafe` clippy lint is on.
  - Complexity: S. Agent: security.

---

## Phase 9 - Beta packaging, docs, release

Goal: signed installers, release notes, and the final docs that ship with v1.

- **P9-T01 - `tauri build` produces signed installers per OS.**
  - Goal: `tauri build` produces an MSI on Windows, DMG on macOS, and .deb + .AppImage on Linux.
  - Files / modules: `apps/client/src-tauri/tauri.conf.json`, `apps/client/package.json`, `.github/workflows/release.yml`.
  - Prerequisites: P0-T02, P8-T08.
  - Acceptance: a tagged CI run produces three artifacts and uploads them to a draft GitHub release; the macOS DMG is code-signed and notarized; the Windows MSI is signed.
  - Complexity: M. Agent: general.

- **P9-T02 - Server Docker image + Caddy reverse-proxy config.**
  - Goal: a single `docker-compose.prod.yml` brings up coturn + caddy + the server; certs are issued by Caddy.
  - Files / modules: `apps/server/Dockerfile`, `deploy/caddy/Caddyfile`, `deploy/coturn/turnserver.conf`, `deploy/docker-compose.prod.yml`.
  - Prerequisites: P2-T04, P2-T07.
  - Acceptance: a `docker compose up` on a fresh host exposes `https://locast.example/ws` and `https://locast.example/turn` and a client connects end-to-end.
  - Complexity: M. Agent: backend.

- **P9-T03 - SBOM + provenance attestation on release.**
  - Goal: every release artifact ships with a CycloneDX SBOM and a SLSA-style provenance attestation.
  - Files / modules: `apps/client/scripts/sbom.sh`, `apps/server/scripts/sbom.sh`, `.github/workflows/release.yml`.
  - Prerequisites: P9-T01.
  - Acceptance: a release run produces `locast-client.sbom.json` and `locast-client.intoto.jsonl` next to the binary; both files validate against their respective schemas.
  - Complexity: S. Agent: security.

- **P9-T04 - User-facing docs (README + user guide).**
  - Goal: a `docs/USER_GUIDE.md` with first-launch, room create, room join, settings.
  - Files / modules: `docs/USER_GUIDE.md`, `docs/README.md` (update).
  - Prerequisites: P1-T09, P3-T10, P6-T03.
  - Acceptance: a non-developer can follow the guide to install, import a file, host a room, and have a friend join - tested by a "friend test" on a non-engineer.
  - Complexity: M. Agent: general.

- **P9-T05 - CHANGELOG + release notes for v1.0.**
  - Goal: a `CHANGELOG.md` with all v1.0 changes; release notes published with the v1.0 tag.
  - Files / modules: `CHANGELOG.md`, `docs/RELEASE_NOTES_v1.md`.
  - Prerequisites: P9-T01.
  - Acceptance: the v1.0 GitHub release has notes that match the items in the changelog and reference the corresponding section of `docs/ARCHITECTURE.md`.
  - Complexity: S. Agent: general.

- **P9-T06 - Beta-tagged v1.0-rc1 build + internal dogfood.**
  - Goal: a v1.0-rc1 build is used by at least 5 internal people for a week; the build is at a tagged commit.
  - Files / modules: release workflow output.
  - Prerequisites: P9-T01, P9-T04.
  - Acceptance: 5 internal users have run the build for >=7 days; a "Blockers for v1.0" list contains zero P0 items.
  - Complexity: M. Agent: general.

- **P9-T07 - v1.0 release.**
  - Goal: the v1.0 tag is cut, the install artifacts are published, and `docs/DEFINITION_OF_DONE.md` is checked off.
  - Files / modules: `.github/workflows/release.yml`, `docs/DEFINITION_OF_DONE.md`.
  - Prerequisites: P9-T06.
  - Acceptance: the Definition of Done checklist is fully green; the v1.0 tag triggers a release that publishes signed installers; the GitHub release is public.
  - Complexity: S. Agent: general.

---

## Cross-cutting concerns

These are not one-time tasks; they are continuous, and any PR that touches the relevant code is expected to update them.

- **Documentation**: `docs/ARCHITECTURE.md` is the source of truth. Any behavior change updates it in the same PR. `docs/ROADMAP.md` (this file) is updated whenever a task is completed or added.
- **CHANGELOG**: every user-visible change gets an entry under `## Unreleased` on the day the PR merges.
- **CI**: every PR runs the lint + typecheck + test + e2e pipeline on Linux, macOS, Windows. The `cargo deny` and `pnpm audit` checks run on every PR; a new advisory must be waived with a comment.
- **Telemetry**: `apps/client/src-tauri/src/telemetry/` and `apps/server/src/metrics.rs` are the only places that emit telemetry. New event types require an addition to the Prometheus metric list in section 20.12 and to the structured-log event list in section 20.11.
- **Accessibility**: every new UI element has a render test in the Vite harness and an a11y assertion (ARIA role, focus order, prefers-reduced-motion behavior).
- **Security review**: every PR that touches `capabilities/`, the IPC surface, the auth path, the FS write path, or the WebSocket message types gets a review from the security agent type.
- **Performance**: every PR that touches a hot path (section 28.10) runs the criterion bench locally and confirms the new result is within 10% of the baseline.
- **Dependency updates**: Dependabot runs weekly. A new major version bump requires an architecture review.
- **License audit**: every new dependency must appear in `deny.toml` and be on the allowlist.

---

## Definition of Done for v1.0

Locast is labeled v1.0 only when **every** item below is true.

- [ ] All 70 roadmap tasks (P0..P9) are complete; the work is committed; CI is green on all three OSes.
- [ ] `docs/ARCHITECTURE.md` is up to date and matches the implementation; the open decisions in Appendix A are either resolved or explicitly moved to "deferred to v1.x" in the changelog.
- [ ] All 15 risk-mitigation pairs in section 29 have at least one passing test.
- [ ] `cargo deny check` is clean, `pnpm audit --prod` is clean, `cargo audit` is clean.
- [ ] No `unsafe` in the project's own code (vendored deps are noted in the license audit).
- [ ] Fuzzing has run for at least 60 s on each fuzz target with no crashes; the corpus is committed.
- [ ] Performance: every hot-path criterion bench is within 10% of its baseline; the in-CI two-client WebRTC transfer runs in under 60 s on the CI matrix.
- [ ] Accessibility: NVDA and VoiceOver smoke tests on the room view and the library pass; the manual testing checklist in section 27.11 is signed off.
- [ ] Poor-network test: a `clumsy`/`Network Link Conditioner` run with 100 ms latency, 5% loss, and 1 Mbps survives a forced 30 s server disconnect without corrupting state.
- [ ] Cross-platform: signed installers (Windows MSI, macOS DMG, Linux .deb + .AppImage) install and run on a clean host; both signed.
- [ ] SBOM and provenance attestation are published for the v1.0 release.
- [ ] `CHANGELOG.md` has a complete v1.0 entry; release notes are public.
- [ ] The user guide (`docs/USER_GUIDE.md`) has been dogfooded by at least 5 non-engineer internal users for 7 days with no P0 issues.
- [ ] The `tar pit` is empty: no `TODO`, `FIXME`, or `unimplemented!()` markers in the source tree.
