# Locast Smoke Test Integration Guide

## 1. Overview

The smoke harness is an end-to-end check that proves the Locast P3 transfer path works end-to-end: a real signaling server (started in-process on an ephemeral port), a real Ed25519 keypair per peer (host and viewer are isolated), a host-signed `MediaManifest`, a real `webrtc-rs` PeerConnection between the two peers, and a real WebRTC DataChannel carrying the DOWNLOAD_OFFER wire frame from viewer to host. It exists for developers working on the Rust client core, the signaling server, the download orchestrator, the manifest signer, the room lifecycle, or anything in between. Run it locally before pushing any change that touches the download path, the signaling flow, the manifest signer, the WebRTC stack, or the room lifecycle.

The test is `#[ignore]`-gated so the default `cargo test --workspace` does not pick it up. It must be run explicitly with `--ignored` (the script and the cross-platform invocation in section 5 do this for you).

## 2. Prerequisites

- Rust toolchain, stable channel, pinned in `rust-toolchain.toml` at the repo root.
- `cargo` and `rustc` on `PATH`.
- `pnpm` 9 or newer (only used to invoke `scripts/smoke.ps1` on Windows).
- Node 20 LTS or newer (only required because `pnpm` needs it; the smoke itself does not run any JavaScript).
- On Windows: WebView2 runtime is **not** required for the smoke harness. It is only required if you also run `pnpm tauri dev` in the same checkout.
- A writable OS temp directory (default `%TEMP%` on Windows, `/tmp` on Linux, `$TMPDIR` on macOS).
- Network access for the WebRTC ICE phase. Public STUN (`stun.l.google.com:19302`, `stun.cloudflare.com:3478`) must be reachable, or the test will fall back to host-only candidates. TURN is not used by the smoke.

## 3. Supported OS

`scripts/smoke.ps1` is Windows-only by design; it is a PowerShell 5.1 wrapper around the cross-platform Rust integration test. On Linux and macOS, run the Rust test directly (see section 5). The Rust test itself is portable across Windows, Linux, and macOS, but it uses `webrtc-rs` and a real PeerConnection, so all three platforms must be able to reach STUN or accept host candidates.

## 4. How to run (Windows, recommended)

From the repository root:

```
pnpm install
pnpm smoke
```

`pnpm smoke` calls `scripts/smoke.ps1`. The script builds the test, creates a per-run temp directory, runs the test (which starts the signaling server in-process), captures logs, prints a summary, and tears everything down. Exit code 0 means the test passed; any non-zero code is explained in section 10 and section 17.

## 5. How to run (cross-platform, manual)

On Linux, macOS, or any environment where you want to drive the test yourself, use the underlying `cargo test` invocation directly. Do **not** use `cargo test --workspace`; the smoke test is `#[ignore]`-gated and the workspace test runner does not pass the right flags.

```
SMOKE_OUTPUT_DIR=$PWD/target/smoke \
  cargo test -j 1 -p locast-client --test smoke_host_viewer -- \
  --ignored --nocapture
```

The `-j 1` matters: the smoke test owns a real in-process signaling server and a real WebRTC stack, and two parallel instances on the same host will fight over ports. `--ignored` is required because the test is marked `#[ignore]` to keep it out of the default `cargo test` run. `--nocapture` ensures stdout from the test shows up alongside the `cargo test` header.

When the run finishes, the result file is at `$SMOKE_OUTPUT_DIR/result.json`. Read it directly: it contains the success flag, the elapsed time, the room_id, the room code, the host and viewer user_ids, the source SHA-256 / BLAKE3, the final (post-transfer) SHA-256 / BLAKE3, the `stages_passed` list, and on error a `failure_stage` and `failure_message`.

On macOS the first run may take longer because `webrtc-rs` and its native deps compile from source; subsequent runs reuse the cache. On Linux, if the test is run inside a container with no `CAP_NET_RAW`, host candidates still work but STUN may fail; in that case set `LOCAST_DISABLE_STUN=1` to keep the run fast.

## 6. What the smoke test does

The test runs the following stages, in order. Stage names are the values you will see in `result.json:stages_passed` and in `failure_stage`:

1. **spawn_rigs** - construct two isolated `ClientRig`s (HOST and VIEWER), each with its own `tempfile::TempDir`, SQLite database, `MockKeyring`-backed `IdentityService`, `SignalingClient`, `RoomClient`, `WebRtcManager`, and `TransferRegistry`. Start the real `locast-server` router in-process on an ephemeral port.
2. **connect_signaling** - both clients complete the Ed25519 auth handshake and reach `ConnPhase::Authenticated`. Each spawns its `room.run_inbound()` and `webrtc.start_with_room_client()` tasks.
3. **seed_fixture** - the host writes a deterministic 1 MiB fixture to the canonical content-addressed path under its own library root and inserts a permanent `media_items` row with the file's SHA-256 and BLAKE3.
4. **room_create** - the host sends `ROOM_CREATE`, gets the server-assigned room_id and 6-char code. The host seeds its own `rooms` and `room_participants` rows locally (the production webview would do this via the `room://state` event).
5. **room_join** - the viewer sends `ROOM_JOIN_REQUEST` with the code, gets the room summary back, captures the viewer's signaling-issued UUID, and the host patches in the viewer's local DB rows.
6. **publish_manifest** - the host calls `room::host::build_sign_and_publish`, which scans the local library, signs the manifest, and sends `MANIFEST_PUBLISH` over the WS.
7. **wait_for_manifest** - the viewer subscribes to its `RoomClient`'s inbound, the server's broadcast forwarder relays the `MANIFEST_PUBLISHED` event, and the viewer verifies the host's signature against the TOFU pubkey anchor set in step 5. The test polls for up to 15 s.
8. **seed_viewer_room** - the viewer seeds `user_identities` (with both sha256-hex and signaling-UUID-keyed rows so the FKs in `downloads` and the UUID-keyed `lookup_dc_by_peer_id` closure both resolve), `rooms`, and `room_participants` locally. In production these are populated by the `room://state` / `room://event` hooks; the test seeds them directly to bypass the webview.
9. **open_download** - the viewer calls `commands::download::open_download_inner` with the verified manifest, the room_id, the host and viewer signaling UUIDs, and its library root. The orchestrator is spawned inside; `DownloadSessionIpc.state` should be `"pending"`.
10. **wait_for_complete** - the test polls `downloads.state` and `transferred_bytes` via `DownloadStore::fetch` until state reaches `Complete` or the budget (currently 15 s; well within the 60 s roadmap budget) elapses. The transfer is verified by re-hashing the on-disk file with SHA-256 and BLAKE3 and comparing against the source.

## 7. Where artifacts go

The smoke script writes everything under a fresh directory at `[System.IO.Path]::GetTempPath()\locast-smoke-<pid>-<timestamp>\`. On manual cross-platform runs, the directory is whatever you set `SMOKE_OUTPUT_DIR` to (default `[System.IO.Path]::GetTempPath()\locast-smoke`). Files:

- `result.json` - the safe-field-only test summary (see section 8 for the schema). No bearer tokens, no signatures, no SDP, no key material.
- `build.log` - merged stdout+stderr from the `cargo build --tests` invocation. Captured by the script.
- `test.log` - merged stdout+stderr from the `cargo test ... --nocapture` invocation. Captured by the script. The test's own `eprintln!` calls land here.

The Rust test itself does not write per-role log files (the `host.log` / `viewer.log` distinction in some earlier drafts has been collapsed because both rigs run in the same process and write to the same stdout). The script does, however, copy `result.json` to `<repoRoot>/smoke-last-result.json` on both success and failure so the developer has a stable path to read after the temp directory is cleaned.

## 8. What success looks like

`pnpm smoke` exits 0. The terminal prints:

```
[OK] Smoke test passed in <N>s (stages_passed=[...])
```

`result.json` (and `smoke-last-result.json`) contains:

```json
{
  "success": true,
  "elapsed_ms": 12345,
  "host_user_id": "<sha256(host_pubkey) hex>",
  "viewer_user_id": "<sha256(viewer_pubkey) hex>",
  "room_code": "ABCDEF",
  "room_id": "<uuid>",
  "media_id": "<uuid>",
  "source_size": 1048576,
  "downloaded_size": 1048576,
  "source_sha256": "<hex>",
  "final_sha256": "<hex>",
  "source_blake3": "<hex>",
  "final_blake3": "<hex>",
  "stages_passed": ["spawn_rigs", "connect_signaling", ...],
  "failure_stage": null,
  "failure_message": null
}
```

`downloaded_size == source_size`, `final_sha256 == source_sha256`, `final_blake3 == source_blake3`, and `stages_passed` contains all 10 stages from section 6.

## 9. Expected time limit

The roadmap budget for the smoke flow is 60 s (architecture §P3-T14, section 21.x). The Rust test itself completes in 20-25 s on a warmed-up developer laptop; the script's `cargo build` step adds another 30-60 s on a cold target dir. The script budgets 120 s total wall-clock to accommodate the build, and reports `elapsed: <N>s (over budget)` plus exit code 7 if the budget is exceeded. On a subsequent run the build is cached and the wall-clock falls well under 30 s.

## 10. What happens on failure

On any failure the script:

1. Prints the failing stage name and a one-line reason.
2. Preserves the output directory (does not delete it).
3. Copies `result.json` to `<repoRoot>/smoke-last-result.json` so the developer has a stable path after the temp dir is reaped.
4. Exits with the corresponding non-zero code (see section 17).

`result.json` in the preserved output directory contains `success: false`, a populated `failure_stage` (one of the 10 stages in section 6), and a populated `failure_message` (the underlying error string, with paths and identifiers only - never keys, signatures, or tokens). To triage, open `result.json`, read `failure_stage` to know which section of the troubleshooting guide to jump to, then read the test log.

## 11. How to inspect logs

The single `test.log` is the test's stdout+stderr captured by the script. It contains:

- The Rust test's own `eprintln!` calls (stage markers, peer counts, signaling snapshots when something goes wrong).
- The `tracing` output of the in-process `locast-server` (since the server runs in the same process as the test, its logs go to the same stream).
- The `tracing` output of both `locast-client` rigs (host + viewer share the same runtime and the same stream).

For verbose server logging, set `SMOKE_SERVER_LOG` (forwarded as `LOCAST_LOG`) before running. For example, `SMOKE_SERVER_LOG=info,locast-server=debug,locast_server=debug pnpm smoke` will surface WS-level debug logs from the in-process server.

Per architecture section 21.14, none of these logs contain bearer tokens, private key material, password hashes, raw media paths, TURN credentials, or Ed25519 signatures. SDP bodies are not logged by default. If you need SDP for a bug, set `LOCAST_LOG_SDP=1` in the environment before running.

## 12. How cleanup works

- The PowerShell script wraps the whole run in a `try { ... } finally { ... }` block. In the `finally`, the script removes the temp directory (on success) or preserves it (on failure) and copies `result.json` to `smoke-last-result.json` for stable inspection.
- The Rust test starts the signaling server in-process via `tokio::spawn`. The server's graceful shutdown is triggered by a `tokio::sync::Notify` wrapped in a `Cancel` handle that the test sends on success and on every failure path. There is no child process; nothing to leak across runs.
- The smoke test does not touch the developer's real Locast data under `%APPDATA%\com.puretechteam.locast\`. Every SQLite database, library root, and keyring is created inside a `tempfile::TempDir` that is dropped at the end of the test, taking the of the contained files.
- If the script is killed (Ctrl-C, terminal close, OS shutdown), the PowerShell `finally` block does not run. The Rust `Drop` guard for `ServerChild`-equivalent resources (now `Cancel`) runs at process teardown and cancels the server task. Look for `locast-smoke-*` directories in `%TEMP%` and delete them by hand if they accumulate.

## 13. What is intentionally NOT covered

- **GUI / webview behavior.** The smoke test exercises the Rust core only. There is no Tauri webview, no React, no Zustand store, no Vite harness. Use the Vite and Tauri WebDriver harnesses (architecture section 27.6) for UI coverage.
- **Production OS keyring.** The test uses `MockKeyring` to keep the run hermetic and parallel-safe. The real Windows DPAPI / macOS Keychain / libsecret paths are exercised by manual testing, not the smoke.
- **Real room playback.** The room is created and the manifest is published, but no `PLAYBACK_CMD` is ever sent. Drift, seek, pause, and "You are behind" are out of scope.
- **Chat, drawing, laser.** Those are Phase 5 and 6 features; the smoke ends at file completion.
- **Host migration.** The P2-T04 amendment is implemented in its documented v1 form only: the room is created with `migration_enabled=false`. No `HOST_TRANSFER` is exercised.
- **Persistent recent rooms across restarts.** Both rigs use `tempfile::TempDir` for their SQLite + library root, so any test-state is reaped at teardown. Restart-survival is out of scope.
- **TURN relay.** The test uses host candidates. If host candidates fail, the test fails; it does not fall back to TURN.
- **Multi-source transfers.** The orchestrator's round-robin source selection (architecture section 9) is implemented but the smoke exercises a single host. Multi-source selection logic is covered by `tests/multi_source_e2e.rs`, not here.

## 14. How this differs from unit/integration tests

The existing Rust integration tests (`transfer_e2e.rs`, `multi_source_e2e.rs`, and the WebRTC integration tests in `webrtc_basic.rs` and `webrtc_signaling.rs`) connect two peers using a loopback transport or stub DataChannels. They are fast, deterministic, and do not require a real signaling server. They prove that the chunk pump, the orchestrator state machine, the hash verifier, and the wire format agree.

The smoke test is the next layer up. It proves the **whole system** agrees:

- The signaling server accepts a real client connection, completes the Ed25519 handshake, and relays `ROOM_*`, `MANIFEST_PUBLISHED`, and (in production) `SIGNAL` frames.
- The client core speaks the same wire protocol the server expects.
- The manifest signer, the manifest verifier, and the chunk planner all use the same hash chain.
- A real `webrtc-rs` PeerConnection, not a loopback mock, carries the host's published manifest and the viewer's `DOWNLOAD_OFFER`.
- Two isolated client instances, each with their own SQLite library and their own keypair, can drive the P3 transfer pipeline end-to-end.

If the smoke test is green, you have proved the path the user actually walks. If only the unit and integration tests are green, you have proved the in-vitro protocol model but not the system.

## 15. Manual verification of the final file

If `result.json` shows `success: true` but you want to verify independently:

1. Open `result.json` and read `final_sha256` and `source_sha256`. They must already be equal (the test asserted this).
2. Locate the viewer's final file. It lives under the viewer's TempDir-derived library root (printed in `test.log` as part of the diagnostic block), at the content-addressed path:
   ```
   <viewer_lib>/library/<sha[0..2]>/<sha[2..4]>/<sha>/smoke.bin
   ```
3. Recompute the SHA-256 with `Get-FileHash` (Windows), `shasum -a 256` (macOS), or `sha256sum` (Linux) and confirm it matches `final_sha256`.

If the file is not where `test.log` says it is, the download never reached `complete`; check the orchestrator state machine trace. If the recomputed hash disagrees with `final_sha256`, you have a chunk verifier bug; file an issue with both hashes and the test log attached.

In practice, you can simply trust the test's own assertion - it rehashes the assembled file at completion and compares it to the host's fixture before printing success.

## 16. Troubleshooting

Read `result.json:failure_stage` first, then jump to the matching bullets below. Each bullet lists the most likely root causes in priority order, with the log file to open in parentheses.

- **spawn_rigs** (`test.log`). Almost always a temp-dir permission or space issue. Check `%TEMP%` is writable and has at least a few hundred MB free. If the in-process `locast-server` fails to bind, the issue is port exhaustion or another `locast-server` running on the test box; kill it and retry.
- **connect_signaling** (`test.log`). A handshake timeout means one client could not reach the in-process server's ephemeral port. Look for `WARN rejected non-null room_id outside a room` in `test.log` - that line should not appear; if it does, the server-side ws/mod.rs `if envelope.room_id.is_some()` check is regressed (the membership check should also verify the caller is a member of the named room).
- **seed_fixture** (`test.log`). Almost always a temp-dir write failure inside `tempfile::TempDir`. Permissions or disk full.
- **room_create / room_join** (`test.log`). A `room_not_found` reply means the viewer's `ROOM_JOIN_REQUEST` raced the host's `ROOM_CREATE` reply. Rerun. An `unhandled_rejection` from the server's broadcast forwarder means the per-room broadcast channel wasn't installed for that room; check `test.log` for `listening on` and the room creation timestamp.
- **manifest_publish / manifest_fetch** (`test.log`). A signature mismatch means the viewer's TOFU pubkey did not match the manifest's `host_signature.public_key`. The test pre-installs the host pubkey via `viewer.room.set_expected_host_pubkey` immediately after `room_create`; if a refactor removes that call, this stage fails.
- **download_open** (`test.log`). The orchestrator leaves the row in `pending` if no `files` DataChannel is open for any source in the manifest. Open the test's diagnostic block at this stage: `WebRTC peer counts`, `viewer signaling user_id`, `manifest source.peer_id vs expected (from pubkey)`, `viewer user_identities rows`, `viewer room_participants rows`. A mismatch between the manifest's `source.peer_id` and the host's actual `sha256(host_pubkey)` means the host's `room::host::build_manifest` is using a wrong pubkey. A `user_identities` row whose `public_key` doesn't decode to the host's pubkey means the test seeded the wrong base64 string. A `room_participants` row missing for the host means `load_room_participant_user_ids` returns an empty list and the lookup closure is empty.
- **wait_for_complete** (`test.log`). The most common failure here is `state=Transferring transferred=0` (or now, after P3-T15, chunks are sent by the host but not received by the viewer). P3-T15 wired the host-side `SenderSession` spawn in `transfer::host_dispatch::HostSenderDispatcher` and `WebRtcManager::on_inbound_data_channel`; the dispatch resolves the source file from the verified manifest and reads chunks from `<library_root>/<media_items.relative_path>`. The viewer-side remaining failure mode is the **webrtc 0.20 SCTP event channel** which is bounded to 1 (`webrtc::runtime::channel(1)`) and uses `try_send` for every event kind. A single `OnBufferedAmountLow` between two `OnMessage` events causes the second `OnMessage` to be **dropped**. The fix is a custom `DataChannel` wrapper that drains the bounded internal channel into a larger mpsc, or upgrading webrtc. Until that lands, chunks can be silently dropped at the viewer. Check `test.log` for `[sender_session] sent Chunk` lines on the host side to confirm chunks are being sent, and for `[viewer_multi_source] received Chunk` lines on the viewer side to confirm whether they're arriving. If the host sends but the viewer never logs a receive, it's the webrtc 0.20 transport issue. See `apps/client/src-tauri/src/transfer/host_dispatch.rs` for the sender wire-up.
- **Server-side warnings.** If `test.log` contains `WARN rejected non-null room_id outside a room` repeatedly, the ws/mod.rs `envelope.room_id` membership check has regressed. The check should reject only if the caller is not a member of the named room, not unconditionally. This is the regression P3-T14 fixed; if it returns, a future refactor has re-broken it.

## 17. Exit codes

| Code | Meaning |
|---|---|
| 0 | Success. Result file shows `success: true`. |
| 1 | Unhandled exception in the script (not a test failure). Read the script's last error line. |
| 2 | Build failed. `cargo build --tests` compilation did not succeed. Fix the build before re-running. |
| 3 | Test could not start. `cargo test` did not produce a result file. The test binary may not have compiled or the Rust test panicked before reaching the result-writing path. Read `build.log` and `test.log`. |
| 4 | Result file missing. The test terminated before writing `result.json`. |
| 5 | Result file invalid. `result.json` could not be parsed, or it was never written, or it is missing the required `success` field. |
| 6 | Result indicates failure. `result.json` exists, parses, and has `success: false`. Read `failure_stage` and `failure_message` to triage. |
| 7 | Budget exceeded. The script's own wall-clock budget elapsed. Re-run; if it reproduces, file an issue. |