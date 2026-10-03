# Manual test: local playback through `locast://` (P1-T10)

This is the desktop check the roadmap asks for in P1-T10: a local MP4 from the
library plays with audio and video, the seek bar works (which proves `Range`
requests), and the player keeps exactly one `room://event` listener. The
automated tests cover the URL mapping, the handler's range behaviour and the
listener lifecycle; they cannot start a real WebView, so this part is manual.

## Before you start

- **Your data location.** `pnpm tauri dev` uses your real per-user app-data
  folder (on Windows `%APPDATA%\com.puretechteam.locast`). The repository has no
  setting that redirects it, and overriding `APPDATA` does not work on Windows
  (Tauri asks the OS for the folder). Run this on a machine, VM or Windows user
  account where creating that folder is fine. A fresh profile starts with an
  empty library.
- **Use a test MP4 with an ASCII file name** (for example `sample.mp4`), H.264
  video and AAC audio, a minute or longer so seeking is visible. Non-ASCII
  names import but are currently refused when played, because the library path
  validator only allows ASCII (a separate, known gap).
- From `apps/client`: `pnpm install`, then `pnpm tauri dev`. The first build
  takes several minutes.

## Steps

1. **Launch.** `pnpm tauri dev` opens the Locast window on the Library page.
2. **Import.** Click **Import files**, pick your MP4. A notice says
   `Imported 1 file.` and a tile with the file name and size appears.
3. **Play.** Click **Play** on the tile. The page changes to
   `/rooms/local` and shows `Playing locally: <file name>` above the video.
4. **Video and audio.** Press play on the video controls. Picture moves and you
   hear audio.
5. **Seek.** Drag the seek bar forward, then backward, and click a point in the
   middle. Playback continues from where you released it; it never jumps back
   to the start.
6. **Confirm range requests.** Right-click the window and choose **Inspect**,
   open the **Network** tab, filter on `locast`, and seek again. Requests go to
   `http://locast.localhost/media/<sha-prefix>/<file name>` (Windows) or
   `locast://localhost/media/...` (macOS, Linux). Each response is `206` with a
   `Content-Range: bytes <start>-<end>/<total>` header. A seek to the middle
   shows a request whose range starts near that offset, not at `0`. No single
   response is larger than 8 MiB.
7. **Listener lifecycle.** Click **Back to library**, then **Play** again, three
   or four times. Playback still starts cleanly each time. (The listener count
   itself is asserted by the automated test; there is nothing to see here beyond
   nothing breaking.)
8. **Inside a room (optional, needs the dev signaling server).** Start the
   server (`cargo run -p locast-server -j 1` from the repo root), create a room,
   and on the room page click **Choose a file from your library**. Click **Play**
   on a tile and confirm you land back on the room with the video loaded.

## Record

For each step note pass or fail. If seeking restarts at 0, or the video does not
load, copy the failing request (URL, status, headers) from the Network tab.
