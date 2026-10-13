# Testing a three hour film

How to run the long-film acceptance checks, what each one proves, and what it
does not. Everything here uses temporary directories and throwaway
identities. None of it touches `%APPDATA%\com.puretechteam.locast`, your
library, your index or your staging data, and none of it starts the desktop
app.

## 1. Make a test film (not tracked: over 100 MB)

Needs `ffmpeg`. Low resolution, low bitrate, with audio; the duration is real.

```bash
# H.264/AAC MP4 for the Rust end-to-end run (110 MB, 10800.000 s)
ffmpeg -f lavfi -i testsrc2=size=160x90:rate=10 -f lavfi -i sine=frequency=440:sample_rate=16000 \
  -t 10800 -c:v libx264 -preset ultrafast -crf 38 -g 30 -pix_fmt yuv420p \
  -c:a aac -b:a 24k -ac 1 -movflags +faststart movie_3h.mp4

# VP8/Opus WebM for the Chromium run (Playwright's Chromium has no H.264)
ffmpeg -f lavfi -i testsrc2=size=160x90:rate=10 -f lavfi -i sine=frequency=440:sample_rate=16000 \
  -t 10800 -c:v libvpx -deadline realtime -cpu-used 8 -b:v 60k -g 30 \
  -c:a libopus -b:a 16k -ac 1 movie_3h.webm
```

## 2. Host to viewer, with the real parts

Real signaling server (in process), two isolated clients, real WebRTC data
channels, the production importer, signed manifest, hash verification, install
and the `locast://` handler.

```bash
LOCAST_SMOKE_MEDIA=/path/movie_3h.mp4 LOCAST_SMOKE_EXPECT_DURATION_MS=10800000 \
  cargo test -j 1 -p locast-client --test smoke_host_viewer -- --ignored --nocapture
```

It checks: the manifest carries the file's size and a duration of about
10 800 000 ms; the viewer's installed copy hashes identically and `ffprobe`s as
the same duration; `locast://` serves byte-exact ranges at the start, the
middle, the last byte, a suffix, past the end (416), a multi-range request
(416) and with no `Range` header. `RUST_LOG=locast_client_lib=debug` shows
what the transfer is doing; `LOCAST_SMOKE_TRANSFER_SECS` bounds the wait.
Transfer speed from a debug build is not representative of a release build.

## 3. Playback of a long film in a real browser

```bash
cd apps/client
LOCAST_LONG_WEBM=/path/movie_3h.webm \
  pnpm exec playwright test --config=tests/playwright/playwright.config.ts long_media_e2e.spec.ts
```

Real Chromium `<video>` reading the file over HTTP Range: the true 3 h
duration, seeks to the middle and the last seconds, playing out to the end,
a viewer joining with the host a few seconds from the end, and the host's
scrubs sent as three hour positions. Without the variable the spec is skipped
and reported as skipped.

## 4. Reconnects and long sessions

```bash
# many random drops of the viewer, the host or both, commands flowing throughout
LOCAST_SOAK_CYCLES=400 LOCAST_SOAK_SEED=424242 \
  cargo test -j 1 -p locast-client --test viewer_reconnect -- --ignored soak --nocapture
```

Both clients sit behind TCP proxies in front of the real in-process server;
after every outage the viewer must still be in the room and a fresh host
command must reach it. About two seconds per cycle. The ordinary
`viewer_reconnect` tests (no `--ignored`) run in CI.

## Running the real desktop app twice on one machine (test profiles)

The app normally keeps its database, library and staging files in
`%APPDATA%\com.puretechteam.locast` and its identity in one fixed OS
credential entry, so a second instance would be the same user, and launching
the app for an experiment runs startup cleanup against real data. A disposable
profile avoids both. It is opt-in and per process:

```powershell
# one instance = one empty directory you own
$env:LOCAST_TEST_PROFILE_DIR = "C:\locast-test\alice"
$env:LOCAST_SIGNALING_URL    = "ws://127.0.0.1:8787/ws"     # your test server
.\target\debug\locast-client.exe
```

With the variable set, that process keeps its database, library, staging
files, logs, webview data (cookies, local storage) and a key file
(`identity.key`) inside the directory, and nothing in the OS credential store.
The window title says `[TEST PROFILE ...]`. A second instance with a different
directory is a different user.

It fails closed. If the value is not a safe disposable location the app prints
why and exits with status 2; it never falls back to the real profile. A
directory is accepted only if it is absolute, has no `..`, is not a filesystem
root, the home directory or an ancestor of it, does not overlap the real app
directories (in either direction, symlinks resolved, case-insensitively), and
is empty/new or already a test profile (it holds `.locast-test-profile`, which
the app writes on first use). With the variable unset nothing changes.

Cleanup removes exactly one profile and nothing else:

```powershell
.\target\debug\locast-client.exe --remove-test-profile C:\locast-test\alice
```

It applies the same checks and also requires the marker file, so a real data
directory (marker or not) or any other folder is refused. Close the instance
first (an open database cannot be removed on Windows). Use a debug build to see
the messages: a release build has no console.

## Two real desktop clients, end to end

`scripts/desktop-acceptance.mjs` launches two real `locast-client` windows (and
a third, late one) in test profiles, a real `locast-server` with a file
database, and the frontend dev server, then drives the real Tauri IPC of each
window over the WebView2 remote-debugging port. Build first, then run it:

```bash
cargo build -p locast-client -p locast-server
node scripts/desktop-acceptance.mjs                                   # 5 MB clip made with ffmpeg
node scripts/desktop-acceptance.mjs --media /path/movie_3h.mp4        # your own file
node scripts/desktop-acceptance.mjs --media /path/movie_3h.mp4 --soak-minutes 35
```

It needs Windows with WebView2, ffmpeg/ffprobe on `PATH`, and port 1420 (the
frontend; an already running dev server is reused), 18787 (server) and
9231-9233 (debug ports) free. Each client runs with `LOCAST_WEBRTC_LOCAL_ONLY=1`,
which (only inside a test profile) keeps WebRTC on loopback with no STUN: on a
machine with several adapters (Wi-Fi plus a VPN) the candidate pairs two local
processes offer include ones that cannot reach each other, and the connection
then fails or stalls. That mode narrows connectivity; it is not how the app
runs for two people on different machines.

What it checks, and fails on the first miss: separate identities, key files and
databases; import, room, invite link, signed manifest, the viewer joining
through the invite with the host key as trust anchor; the WebRTC transfer,
whole-file verification and atomic install; the installed file playing through
`locast://` in a real `<video>` (duration, seeks near the start, middle and end,
playing out to `ended`, a missing file erroring instead of hanging); host Play,
Seek and Pause reaching the viewer in order; a late joiner receiving the
current playback snapshot; the viewer disconnecting and reconnecting while
paused and while playing, host control continuing; leaving and a fresh room
carrying nothing over; and that the real app profile was not touched.
`--soak-minutes N` then keeps three clients in the room for N real minutes with
the host commanding every 20 s and a viewer reconnecting now and then.

Reading a result honestly:

| What | Kind |
| --- | --- |
| `smoke_host_viewer` with a 6.2 GB / 10 800 s file | real file through import, transfer, install and Range reads, in one process (debug build, ~27 min for the transfer) |
| this script with a 6.2 GB / 10 800 s file | the same, through two real windows and the real IPC |
| `long_session`, `viewer_reconnect` soak | accelerated (short bearer lifetime, many cycles) |
| `--soak-minutes` | real time, but minutes, not three hours |
| a genuine three hours of playback | not run; the sync maths is covered by unit tests and accelerated positions |

## What this does not prove

- Three hours of real-time playback staying in sync. The sync maths is covered
  by unit checks and accelerated positions, not by a three hour wall clock; the
  longest real-time run is the `--soak-minutes` one above.
- Two peers on different networks. There is no TURN relay (STUN only), so two
  people behind symmetric NATs may not connect; nothing here exercises a NAT.
- The packaged installer. Test profiles (above) run the built executable, not
  an installed package.
- Behaviour across NATs. Everything here is on localhost.
- Release-build transfer throughput.

## Sharing large films (multi-frame manifests)

The signed manifest lists a SHA-256 per 256 KiB chunk (about 67 bytes each):
~2.7 MB for a 10.5 GB film, ~13.7 MB for the default 50 GiB media quota. The
signaling socket caps a frame at 1 MiB, so a manifest above ~640 KiB (about
9,500 chunks, 2.4 GB) travels as `MANIFEST_PART` frames instead of one
`MANIFEST_PUBLISH`; smaller manifests are unchanged, so older peers still work
for them. This works with the default settings on both ends; nothing needs
raising.

- The host serializes the ONE signed manifest, splits it into pieces of at
  most 512 KiB (about 683 KiB as base64), and paces them at 60% of the
  server's advertised byte rate (WELCOME), so a 50 GiB manifest takes about
  30 seconds to send. It then waits for the server's broadcast of that
  manifest to come back, and re-sends it (a fresh assembly, up to three
  attempts) if it does not, so a lost piece is an error or a retry, never a
  silently missing manifest.
- The server reassembles per connection, only for the room's host, then runs
  the same checks as for a single frame (signature, signed room id, signer ==
  authenticated host, version, storage) before relaying the pieces to the room.
  A late joiner's `MANIFEST_REQUEST` is answered in pieces the same way.
- A viewer reassembles and then runs exactly the single-frame acceptance
  (signature, trust anchor from the invite, stale-version guard), plus a check
  that the manifest names the room the pieces came in. A manifest is never
  accepted partially.
- Limits: 32 MiB per manifest (about 120 GiB of media), 1,024 pieces, one
  assembly per server connection, two per client, all charged to a shared byte
  budget; an assembly with no progress for 30 s (server) or 60 s (client)
  expires and a dropped connection frees it. An out-of-order or conflicting
  piece, a changed header or a digest mismatch drops the assembly.
- A selection whose manifest exceeds 32 MiB is refused up front with
  `ManifestTooLarge`.

Tests: `tests/large_manifest.rs` (client crate) runs a 10.5 GB and a 50 GiB
manifest through the production signing, pacing, server relay, viewer
assembly and a late joiner's fetch with default settings;
`apps/server/tests/rooms.rs` covers missing, duplicate, conflicting, altered
and oversized pieces, a non-host, and a host that disconnects mid-publication;
`shared/protocol/src/manifest_parts.rs` and `net/room.rs` have the unit tests;
`fuzz_targets/manifest_parts.rs` fuzzes the assembler.
