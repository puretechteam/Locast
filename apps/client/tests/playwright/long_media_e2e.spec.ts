// A three hour film in a real browser.
//
// Opt-in: set LOCAST_LONG_WEBM to a ~3 hour WebM (VP8/Opus). It is not tracked
// (it is over 100 MB); generate one with
//
//   ffmpeg -f lavfi -i testsrc2=size=160x90:rate=10 -f lavfi -i sine=frequency=440:sample_rate=16000 \
//          -t 10800 -c:v libvpx -deadline realtime -cpu-used 8 -b:v 60k -g 30 -c:a libopus -b:a 16k -ac 1 long.webm
//
// Without the variable these tests are skipped (and say so).
//
// What this proves, with a REAL Chromium <video> reading the file over HTTP
// Range the way the desktop app's `locast://` handler serves it: the
// browser reports the true three hour duration; seeking to the middle and to
// the last seconds works, as byte-range reads (the file's seek index is at its
// end, so even the first seek has to ask for the tail); playing out to the
// end ends the media cleanly; and the app's sync code handles 3 hour
// positions. What it does NOT prove: that three hours of real-time playback
// stay in sync (see the accelerated clock checks in drift.smoke.ts).

import { test, expect, injectLocastShim } from "./fixtures/vite-app";
import {
    HOST_ID,
    HOST_ROOM,
    OTHER_HOST_ID,
    VIEWER_ID,
    VIEWER_ROOM,
    mountRoomWithRealVideo,
    sentCommands,
    userGesture,
    videoState,
} from "./fixtures/real-video-room";
import type { Page } from "@playwright/test";

const LONG = process.env["LOCAST_LONG_WEBM"];
const H3_SEC = 10_800;

test.skip(LONG === undefined || LONG === "", "set LOCAST_LONG_WEBM to a ~3 hour WebM to run");

test.beforeEach(async ({ page, locast }) => {
    await injectLocastShim(page);
    await page.goto("/");
    await page.waitForLoadState("domcontentloaded");
    await locast.waitForBridge();
    await locast.resetInvokeLog();
});

const media = { path: LONG ?? "", contentType: "video/webm", urlPath: "/test/long.webm" };

async function setSkew0(page: Page): Promise<void> {
    await page.evaluate(() => {
        const w = window as unknown as {
            __locastClockSkew: { setSkewJitter: (s: number | null, j: number | null) => void };
        };
        w.__locastClockSkew.setSkewJitter(0, 0);
    });
}

test("the browser reads the true three hour duration", async ({ page }) => {
    await mountRoomWithRealVideo(page, VIEWER_ROOM, VIEWER_ID, media);
    const { duration } = await videoState(page);
    expect(duration).toBeGreaterThan(H3_SEC - 1);
    expect(duration).toBeLessThan(H3_SEC + 1);
});

test("seeking to the middle and near the end reads those byte ranges", async ({ page }) => {
    const stats = await mountRoomWithRealVideo(page, VIEWER_ROOM, VIEWER_ID, media);
    const before = stats.requests.length;

    await userGesture(page, "v.currentTime = 5400;");
    await expect.poll(async () => (await videoState(page)).currentTime, { timeout: 20_000 }).toBeGreaterThan(5399);
    expect((await videoState(page)).currentTime).toBeLessThan(5402);

    await userGesture(page, "v.currentTime = v.duration - 5;");
    await expect
        .poll(async () => (await videoState(page)).currentTime, { timeout: 20_000 })
        .toBeGreaterThan(H3_SEC - 6);

    // The browser asked for byte ranges (never the whole file) and got them.
    const after = stats.requests.slice(before);
    expect(after.length).toBeGreaterThan(0);
    for (const r of after) {
        expect(r.status === 206 || r.status === 200).toBe(true);
        expect(r.bytes).toBeLessThanOrEqual(4 * 1024 * 1024);
    }
    expect(after.some((r) => r.range !== null && r.range.startsWith("bytes="))).toBe(true);
});

test("playing out the last seconds ends the film cleanly", async ({ page }) => {
    await mountRoomWithRealVideo(page, VIEWER_ROOM, VIEWER_ID, media);
    await userGesture(page, "v.currentTime = v.duration - 4; await v.play();");
    await expect.poll(async () => (await videoState(page)).ended, { timeout: 25_000 }).toBe(true);
    const { currentTime, paused } = await videoState(page);
    expect(currentTime).toBeGreaterThan(H3_SEC - 1);
    expect(paused).toBe(true);
    // No error banner: reaching the end is not a failure.
    await expect(page.getByTestId("locast-player-error")).toHaveCount(0);
});

test("a viewer joining with the host nearly at the end lands there and finishes", async ({ page }) => {
    // The host started the film 3 hours ago: the snapshot says "playing, at
    // 2h59m50s, 3 s ago".
    const room = {
        ...VIEWER_ROOM,
        playback: {
            paused: false,
            media_position_ms: (H3_SEC - 10) * 1000,
            server_ts_ms: Date.now() - 3_000,
            server_seq: 400,
            sender_id: OTHER_HOST_ID,
            monotonic_seq: 400,
        },
    };
    await mountRoomWithRealVideo(page, room, VIEWER_ID, media);
    await setSkew0(page);

    await expect
        .poll(async () => (await videoState(page)).currentTime, { timeout: 20_000 })
        .toBeGreaterThan(H3_SEC - 8);
    await expect.poll(async () => (await videoState(page)).ended, { timeout: 25_000 }).toBe(true);
});

test("the host's scrub to the middle and to the end are sent with three hour positions", async ({
    page,
}) => {
    await mountRoomWithRealVideo(page, HOST_ROOM, HOST_ID, media);
    await userGesture(page, "await v.play();");
    await expect.poll(async () => (await sentCommands(page)).length).toBe(1);

    await userGesture(page, "v.currentTime = 5400;");
    await expect.poll(async () => (await sentCommands(page)).length, { timeout: 20_000 }).toBe(2);
    await userGesture(page, "v.currentTime = v.duration - 2;");
    await expect.poll(async () => (await sentCommands(page)).length, { timeout: 20_000 }).toBeGreaterThanOrEqual(3);

    const sent = await sentCommands(page);
    const seeks = sent.filter((s) => s.action === "seek");
    expect(seeks[0]!.media_position_ms).toBeGreaterThan(5_399_000);
    expect(seeks[0]!.media_position_ms).toBeLessThan(5_402_000);
    expect(seeks[1]!.media_position_ms).toBeGreaterThan((H3_SEC - 3) * 1000);
    // Strictly consecutive sequence numbers the whole way.
    const seqs = sent.map((s) => s.monotonic_seq);
    seqs.forEach((q, i) => i > 0 && expect(q).toBe(seqs[i - 1]! + 1));
});
