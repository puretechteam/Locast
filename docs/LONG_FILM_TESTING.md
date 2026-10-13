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

## What this does not prove

- Three hours of real-time playback staying in sync. The sync maths is covered
  by unit checks and accelerated positions, not by a three hour wall clock.
- The packaged desktop app. It has no app-data override and one fixed OS
  keyring identity, so two instances on one machine would share an identity.
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
