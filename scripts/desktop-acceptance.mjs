#!/usr/bin/env node
// Two (or three) REAL desktop clients on one machine, each in its own
// disposable test profile (see docs/LONG_FILM_TESTING.md), against a real
// in-process-free signaling server, driven through the real Tauri IPC of each
// window over the WebView2 remote-debugging port.
//
//   node scripts/desktop-acceptance.mjs [--media <file>] [--keep]
//
// With no --media a short H.264 clip is generated with ffmpeg. Nothing here
// touches the real app profile: every client runs with LOCAST_TEST_PROFILE_DIR
// under a fresh directory, and the script fails if the real profile changed.
//
// What it checks (exit status 1 on the first failure):
//  - separate identities and databases per client (separate key files);
//  - host import -> room -> signed manifest -> viewer joins through the real
//    invite link, trust anchor and all -> WebRTC download -> atomic install;
//  - the installed file plays through locast:// in a real <video>, duration,
//    seeks near the start / middle / end;
//  - host Play / Pause / Seek reach the viewer in order;
//  - a late joiner receives the current playback snapshot;
//  - a viewer disconnect/reconnect while paused and while playing, host
//    control continuing after;
//  - leaving the room and a fresh room carrying no old state.

import { spawn, spawnSync } from "node:child_process";
import { createRequire } from "node:module";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import http from "node:http";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(here, "..");
const require = createRequire(path.join(root, "apps", "client", "package.json"));
const { chromium } = require("@playwright/test");

const args = process.argv.slice(2);
const argVal = (n) => (args.includes(n) ? args[args.indexOf(n) + 1] : undefined);
const keep = args.includes("--keep");
const exe = path.join(root, "target", "debug", process.platform === "win32" ? "locast-client.exe" : "locast-client");
const serverExe = path.join(root, "target", "debug", process.platform === "win32" ? "locast-server.exe" : "locast-server");

const SERVER_PORT = 18787;
const VITE_PORT = 1420;
const results = [];
const children = [];
let failed = false;

const log = (m) => console.log(`[${new Date().toISOString().slice(11, 19)}] ${m}`);
function check(name, cond, detail = "") {
  results.push({ name, ok: !!cond, detail });
  log(`${cond ? "PASS" : "FAIL"} ${name}${detail ? " :: " + detail : ""}`);
  if (!cond) {
    failed = true;
    throw new Error(`check failed: ${name} ${detail}`);
  }
}
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
async function until(what, fn, ms = 60000, step = 200) {
  const end = Date.now() + ms;
  let last;
  while (Date.now() < end) {
    try {
      last = await fn();
      if (last) return last;
    } catch (e) {
      last = e;
    }
    await sleep(step);
  }
  throw new Error(`timed out waiting for ${what} (last: ${last && last.message ? last.message : JSON.stringify(last)})`);
}
function httpGet(url) {
  return new Promise((resolve, reject) => {
    http.get(url, (res) => {
      let b = "";
      res.on("data", (d) => (b += d));
      res.on("end", () => resolve({ status: res.statusCode, body: b }));
    }).on("error", reject);
  });
}
function start(cmd, argv, opts) {
  const c = spawn(cmd, argv, { stdio: ["ignore", "pipe", "pipe"], ...opts });
  const tag = path.basename(cmd) + ":" + c.pid;
  c.stdout.on("data", (d) => fs.appendFileSync(opts.logFile, d));
  c.stderr.on("data", (d) => fs.appendFileSync(opts.logFile, d));
  children.push({ c, tag });
  return c;
}
function killAll() {
  for (const { c } of children) {
    try {
      if (process.platform === "win32") spawnSync("taskkill", ["/PID", String(c.pid), "/T", "/F"]);
      else c.kill("SIGKILL");
    } catch {}
  }
}

// ---------------------------------------------------------------- real profile guard
const realDir = process.platform === "win32"
  ? path.join(process.env.APPDATA ?? "", "com.puretechteam.locast")
  : path.join(os.homedir(), ".local", "share", "com.puretechteam.locast");
function snapshotReal() {
  if (!fs.existsSync(realDir)) return "absent";
  const out = [];
  const walk = (d) => {
    for (const e of fs.readdirSync(d, { withFileTypes: true })) {
      const p = path.join(d, e.name);
      if (e.isDirectory()) walk(p);
      else {
        const s = fs.statSync(p);
        out.push(`${path.relative(realDir, p)}:${s.size}:${s.mtimeMs}`);
      }
    }
  };
  walk(realDir);
  return out.sort().join("\n");
}

// ---------------------------------------------------------------- driving a client window
async function attach(port) {
  const browser = await until(`CDP on ${port}`, async () => {
    const v = await httpGet(`http://127.0.0.1:${port}/json/version`);
    return v.status === 200 ? chromium.connectOverCDP(`http://127.0.0.1:${port}`) : null;
  }, 90000, 500);
  const page = await until("a page", async () => {
    for (const ctx of browser.contexts()) {
      const p = ctx.pages().find((x) => x.url().startsWith("http://127.0.0.1:" + VITE_PORT) || x.url().startsWith("http://localhost:" + VITE_PORT));
      if (p) return p;
    }
    return null;
  }, 60000);
  await until("tauri IPC", () => page.evaluate(() => !!window.__TAURI_INTERNALS__), 60000);
  await page.evaluate(() => {
    window.__ev = {};
    window.__listen = async (event) => {
      window.__ev[event] = window.__ev[event] || [];
      const handler = window.__TAURI_INTERNALS__.transformCallback((e) => window.__ev[event].push(e.payload));
      await window.__TAURI_INTERNALS__.invoke("plugin:event|listen", { event, target: { kind: "Any" }, handler });
    };
  });
  for (const ev of ["playback://state", "download://state", "download://progress", "room://state"]) {
    await page.evaluate((e) => window.__listen(e), ev);
  }
  const inv = (cmd, a = {}) =>
    page.evaluate(async ([c, x]) => {
      try {
        return { ok: true, v: await window.__TAURI_INTERNALS__.invoke(c, x) };
      } catch (e) {
        return { ok: false, e: typeof e === "string" ? e : JSON.stringify(e) };
      }
    }, [cmd, a]);
  const call = async (cmd, a) => {
    const r = await inv(cmd, a);
    if (!r.ok) throw new Error(`${cmd} failed: ${r.e}`);
    return r.v;
  };
  return { page, browser, inv, call, events: (e) => page.evaluate((x) => window.__ev[x] || [], e) };
}

// ---------------------------------------------------------------- main
async function main() {
  for (const f of [exe, serverExe]) {
    if (!fs.existsSync(f)) throw new Error(`build first: cargo build -p locast-client -p locast-server (missing ${f})`);
  }
  const stamp = new Date().toISOString().replace(/[:.]/g, "-");
  const base = path.join(process.platform === "win32" ? "C:\\locast-test" : path.join(os.tmpdir(), "locast-test"), `desktop-${stamp}`);
  fs.mkdirSync(base, { recursive: true });
  const logs = path.join(base, "logs");
  fs.mkdirSync(logs);
  log(`workspace ${base}`);
  const realBefore = snapshotReal();

  // Media.
  let media = argVal("--media");
  if (!media) {
    media = path.join(base, "clip.mp4");
    const r = spawnSync("ffmpeg", ["-y", "-f", "lavfi", "-i", "testsrc2=size=320x180:rate=15", "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=16000",
      "-t", "180", "-c:v", "libx264", "-preset", "ultrafast", "-crf", "35", "-g", "30", "-pix_fmt", "yuv420p", "-c:a", "aac", "-b:a", "32k", "-ac", "1", "-movflags", "+faststart", media]);
    if (r.status !== 0) throw new Error("ffmpeg failed: " + r.stderr);
  }
  const probe = spawnSync("ffprobe", ["-v", "error", "-show_entries", "format=duration", "-of", "default=nw=1:nk=1", media]);
  const durationS = parseFloat(String(probe.stdout));
  const sizeBytes = fs.statSync(media).size;
  log(`media ${media} ${sizeBytes} bytes ${durationS}s`);

  // Server + frontend dev server.
  start(serverExe, [], { env: { ...process.env, LOCAST_BIND_ADDR: `127.0.0.1:${SERVER_PORT}`, LOCAST_LOG: "info" }, logFile: path.join(logs, "server.log") });
  await until("server", async () => (await httpGet(`http://127.0.0.1:${SERVER_PORT}/health`).catch(() => ({ status: 0 }))).status > 0, 30000).catch(() => {});
  const viteUp = await httpGet(`http://127.0.0.1:${VITE_PORT}/`).then((r) => r.status === 200).catch(() => false);
  if (!viteUp) {
    start(process.platform === "win32" ? "pnpm.cmd" : "pnpm", ["exec", "vite"], { cwd: path.join(root, "apps", "client"), shell: process.platform === "win32", env: process.env, logFile: path.join(logs, "vite.log") });
    await until("vite", async () => (await httpGet(`http://127.0.0.1:${VITE_PORT}/`)).status === 200, 60000);
  } else {
    log("frontend dev server already running on :" + VITE_PORT);
  }

  // Clients.
  const names = ["alice", "bob", "carol"];
  const ports = { alice: 9231, bob: 9232, carol: 9233 };
  const dirs = Object.fromEntries(names.map((n) => [n, path.join(base, "profiles", n)]));
  const launch = (n) =>
    start(exe, [], {
      env: {
        ...process.env,
        LOCAST_TEST_PROFILE_DIR: dirs[n],
        LOCAST_SIGNALING_URL: `ws://127.0.0.1:${SERVER_PORT}/ws`,
        WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: `--remote-debugging-port=${ports[n]} --autoplay-policy=no-user-gesture-required`,
      },
      logFile: path.join(logs, `${n}.log`),
    });
  const C = {};
  for (const n of ["alice", "bob"]) {
    launch(n);
    C[n] = await attach(ports[n]);
  }

  // ---- separate identities and databases
  const idA = await C.alice.call("identity_get", { displayName: "Alice" });
  const idB = await C.bob.call("identity_get", { displayName: "Bob" });
  check("two clients have different identities", idA.public_key !== idB.public_key, `${idA.user_id.slice(0, 8)} vs ${idB.user_id.slice(0, 8)}`);
  for (const n of ["alice", "bob"]) {
    check(`${n} keeps its identity key and database in its own profile`,
      fs.existsSync(path.join(dirs[n], "identity.key")) && fs.existsSync(path.join(dirs[n], "index.sqlite")) && fs.existsSync(path.join(dirs[n], ".locast-test-profile")));
  }
  check("the identity key files differ", fs.readFileSync(path.join(dirs.alice, "identity.key"), "utf8") !== fs.readFileSync(path.join(dirs.bob, "identity.key"), "utf8"));
  check("the real app profile is untouched so far", snapshotReal() === realBefore);

  // ---- host imports, creates a room, shares
  const imported = await C.alice.call("media_import", { paths: [media] });
  check("host imported the file", imported.length === 1 && imported[0].size_bytes === sizeBytes, imported[0]?.filename);
  const mediaId = imported[0].id;
  await C.alice.call("room_connect_signaling");
  await until("alice authenticated", async () => (await C.alice.call("signaling_get_state")).phase === "Authenticated", 30000);
  const room = await C.alice.call("room_create", { title: "desktop acceptance", migrationEnabled: true });
  const invite = await C.alice.call("room_invite_url");
  check("host created a room and an invite link", !!room.code && invite.startsWith("locast://join/"), invite);
  await C.alice.call("manifest_publish", { mediaIds: [mediaId] });

  // ---- viewer joins through the invite
  await C.bob.call("room_connect_signaling");
  const joined = await until("bob joins", async () => {
    const r = await C.bob.inv("room_join", { code: room.code, displayName: "Bob", inviteUrl: invite });
    return r.ok ? r.v : null;
  }, 30000, 500);
  check("viewer joined through the invite", joined.id === room.id);
  const manifest = await until("viewer verified manifest", async () => {
    const m = await C.bob.call("manifest_current");
    return m && m.media && m.media.length ? m : null;
  }, 60000);
  check("viewer holds the verified manifest", manifest.media[0].size_bytes === sizeBytes && manifest.media[0].filename === path.basename(media));

  // ---- transfer, verification, atomic install
  const t0 = Date.now();
  const dl = await C.bob.call("download_open", { mediaId: manifest.media[0].id });
  log("download_open -> " + JSON.stringify(dl));
  await until("download complete", async () => {
    const evs = await C.bob.events("download://state");
    const last = evs[evs.length - 1];
    if (last && last.state === "failed") throw new Error("download failed: " + last.error_message);
    return last && last.state === "complete";
  }, 30 * 60 * 1000, 500);
  log(`transfer + verify + install took ${((Date.now() - t0) / 1000).toFixed(1)}s`);
  const lib = await C.bob.call("library_list", { query: null, limit: 50, offset: 0 });
  const mine = lib.find((x) => x.filename === path.basename(media));
  check("viewer's library has the installed copy", !!mine && mine.size_bytes === sizeBytes && mine.sha256 === imported[0].sha256, mine && mine.status);
  const url = await C.bob.call("media_resolve_url", { mediaId: mine.id });
  check("locast:// url resolved", /^(locast:|https?:\/\/locast\.localhost)/.test(url), url);

  // ---- local playback of the installed copy through locast://
  const pb = await C.bob.page.evaluate(async (u) => {
    const v = document.createElement("video");
    v.muted = true;
    v.preload = "auto";
    // This platform's form of the scheme (http://locast.localhost/ on Windows).
    const base = window.__TAURI_INTERNALS__.convertFileSrc("", "locast");
    v.src = base.replace(/\/?$/, "/") + u.replace("locast://", "").replace(/^\/+/, "");
    document.body.appendChild(v);
    const wait = (ev, ms = 20000) => new Promise((res, rej) => { const t = setTimeout(() => rej(new Error("timeout " + ev)), ms); v.addEventListener(ev, () => { clearTimeout(t); res(); }, { once: true }); });
    await new Promise((res, rej) => { v.addEventListener("loadedmetadata", res, { once: true }); v.addEventListener("error", () => rej(new Error("video error " + (v.error && v.error.code))), { once: true }); });
    const out = { duration: v.duration, seeks: [] };
    for (const f of [0.02, 0.5, 0.98]) {
      v.currentTime = v.duration * f;
      await wait("seeked");
      out.seeks.push({ wanted: v.duration * f, got: v.currentTime });
    }
    await v.play();
    await new Promise((r) => setTimeout(r, 1500));
    out.advanced = v.currentTime;
    // End of media: play the last two seconds out to the `ended` event.
    v.currentTime = Math.max(0, v.duration - 2);
    await wait("seeked");
    const ended = new Promise((res) => v.addEventListener("ended", () => res(true), { once: true }));
    await v.play();
    out.ended = await Promise.race([ended, new Promise((r) => setTimeout(() => r(false), 15000))]);
    out.endedAt = v.currentTime;
    v.remove();
    // A file that is not there fails with an error, it does not hang.
    const bad = document.createElement("video");
    bad.src = base.replace(/\/?$/, "/") + "media/0000000000000000/missing.mp4";
    document.body.appendChild(bad);
    out.missing = await Promise.race([
      new Promise((res) => bad.addEventListener("error", () => res("error:" + (bad.error && bad.error.code)), { once: true })),
      new Promise((r) => setTimeout(() => r("hang"), 15000)),
    ]);
    bad.remove();
    return out;
  }, url);
  check("installed copy has the real duration through locast://", Math.abs(pb.duration - durationS) < 1, `${pb.duration} vs ${durationS}`);
  check("seeks near start, middle and end land where asked", pb.seeks.every((s) => Math.abs(s.got - s.wanted) < 2), JSON.stringify(pb.seeks));
  check("playback advances", pb.advanced > pb.seeks[2].got - 0.1);
  check("playing out the end reaches `ended` at the end of the media", pb.ended === true && Math.abs(pb.endedAt - pb.duration) < 0.5, `ended=${pb.ended} at ${pb.endedAt}`);
  check("a missing media file errors instead of hanging", String(pb.missing).startsWith("error"), String(pb.missing));

  // ---- host controls reach the viewer, in order
  const play = (c, kind, pos) => c.call("playback_send", { cmd: { action: kind, monotonic_seq: 0, media_position_ms: pos } });
  await play(C.alice, "play", 1000);
  await play(C.alice, "seek", 60000);
  await play(C.alice, "pause", 60000);
  await until("viewer saw three commands", async () => (await C.bob.events("playback://state")).length >= 3, 15000);
  const seen = (await C.bob.events("playback://state")).map((e) => [e.kind, e.media_position_ms, e.server_seq]);
  check("viewer received play, seek, pause in order", JSON.stringify(seen) === JSON.stringify([["play", 1000, 1], ["seek", 60000, 2], ["pause", 60000, 3]]), JSON.stringify(seen));

  // ---- late joiner gets the snapshot
  launch("carol");
  C.carol = await attach(ports.carol);
  const idC = await C.carol.call("identity_get", { displayName: "Carol" });
  check("a third client is a third identity", idC.public_key !== idA.public_key && idC.public_key !== idB.public_key);
  await C.carol.call("room_connect_signaling");
  const cj = await until("carol joins", async () => {
    const r = await C.carol.inv("room_join", { code: room.code, displayName: "Carol", inviteUrl: invite });
    return r.ok ? r.v : null;
  }, 30000, 500);
  const snap = cj.playback;
  check("late joiner receives the current playback snapshot", snap && snap.paused === true && snap.media_position_ms === 60000 && snap.server_seq === 3, JSON.stringify(snap));
  await until("carol has the manifest", async () => ((await C.carol.call("manifest_current"))?.media?.length ?? 0) > 0 || (await C.carol.inv("manifest_fetch", { mediaId: manifest.media[0].id })).ok, 30000);

  // ---- viewer reconnect while paused, then while playing; host control continues
  const stateOf = async (c) => c.call("room_get_state");
  await C.bob.call("signaling_disconnect");
  await sleep(800);
  await play(C.alice, "seek", 90000); // while bob is gone, still paused
  await C.bob.call("signaling_connect");
  await until("bob back in the room with the current state", async () => {
    const s = await stateOf(C.bob);
    return s && s.playback && s.playback.media_position_ms === 90000 && s.playback.paused === true ? s : null;
  }, 45000);
  check("viewer reconnected while paused and caught up", true);
  await play(C.alice, "play", 90000);
  await until("bob hears play", async () => (await C.bob.events("playback://state")).some((e) => e.kind === "play" && e.media_position_ms === 90000), 15000);
  await C.bob.call("signaling_disconnect");
  await sleep(800);
  await play(C.alice, "seek", 120000); // while bob is gone, playing
  await C.bob.call("signaling_connect");
  await until("bob back while playing", async () => {
    const s = await stateOf(C.bob);
    return s && s.playback && s.playback.media_position_ms === 120000 && s.playback.paused === false ? s : null;
  }, 45000);
  check("viewer reconnected while playing and caught up", true);
  await play(C.alice, "pause", 125000);
  await until("bob hears the host after reconnect", async () => (await C.bob.events("playback://state")).some((e) => e.kind === "pause" && e.media_position_ms === 125000), 15000);
  check("host control continues after the viewer's reconnect", true);

  // ---- leaving and a fresh room carry nothing over
  await C.bob.call("room_leave");
  check("viewer left the room", (await stateOf(C.bob)) === null);
  const room2 = await C.alice.call("room_create", { title: "second", migrationEnabled: true }).catch((e) => ({ err: e.message }));
  if (room2.err) {
    await C.alice.call("room_leave");
  }
  const fresh = room2.err ? await C.alice.call("room_create", { title: "second", migrationEnabled: true }) : room2;
  check("a fresh room has no old playback state", fresh.id !== room.id && !fresh.playback);
  const invite2 = await C.alice.call("room_invite_url");
  const b2 = await until("bob joins the fresh room", async () => {
    const r = await C.bob.inv("room_join", { code: fresh.code, displayName: "Bob", inviteUrl: invite2 });
    return r.ok ? r.v : null;
  }, 30000, 500);
  check("the viewer sees no playback state or manifest from the old room", !b2.playback && !((await C.bob.call("manifest_current"))?.media?.length));

  // ---- the real profile was never touched
  check("the real app profile was not touched", snapshotReal() === realBefore);
}

let exitCode = 0;
try {
  await main();
} catch (e) {
  exitCode = 1;
  console.error("FAILED:", e.message);
} finally {
  killAll();
  const ok = results.filter((r) => r.ok).length;
  console.log(`\n${ok}/${results.length} checks passed${failed || exitCode ? " - FAILED" : ""}`);
  if (!keep) {
    await sleep(1500);
  }
  process.exit(exitCode);
}
