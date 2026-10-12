// A viewer that applies a host command LATE must land where the host is now.
//
// A command carries the position at the moment the server accepted it. The
// viewer often applies it later: the shared file was still downloading when the
// host pressed Play, the player was remounted, the viewer joined mid-film. The
// player used to seek to the command's raw position, so the viewer started
// behind by the command's age and stayed there until someone pressed Sync.
//
// A real Chromium <video> (a tiny clip over HTTP Range) is the system under
// test, so the assertions are on the element's own currentTime / paused.

import { test, expect, injectLocastShim } from "./fixtures/vite-app";
import {
    OTHER_HOST_ID,
    VIEWER_ID,
    VIEWER_ROOM,
    mountRoomWithRealVideo,
    videoState,
} from "./fixtures/real-video-room";
import type { Page } from "@playwright/test";

test.beforeEach(async ({ page, locast }) => {
    await injectLocastShim(page);
    await page.goto("/");
    await page.waitForLoadState("domcontentloaded");
    await locast.waitForBridge();
    await locast.resetInvokeLog();
});

async function setSkew(page: Page, skewMs: number | null): Promise<void> {
    await page.evaluate((s) => {
        const w = window as unknown as {
            __locastClockSkew: { setSkewJitter: (s: number | null, j: number | null) => void };
        };
        w.__locastClockSkew.setSkewJitter(s, 0);
    }, skewMs);
}

function event(seq: number, kind: "play" | "pause" | "seek", positionMs: number, ageMs: number) {
    return {
        room_id: VIEWER_ROOM.id,
        server_seq: seq,
        server_ts_ms: Date.now() - ageMs,
        sender_id: OTHER_HOST_ID,
        monotonic_seq: seq,
        kind,
        media_position_ms: positionMs,
    };
}

test("a PLAY applied 4 s after the host pressed it starts 4 s further in", async ({ page, locast }) => {
    await mountRoomWithRealVideo(page, VIEWER_ROOM, VIEWER_ID);
    await setSkew(page, 0);

    await locast.emitPlaybackState(event(1, "play", 2_000, 4_000));

    await expect.poll(async () => (await videoState(page)).paused).toBe(false);
    const { currentTime } = await videoState(page);
    // 2 s + 4 s of age, plus the few hundred ms that passed since.
    expect(currentTime).toBeGreaterThan(5.9);
    expect(currentTime).toBeLessThan(7.5);
});

test("a SEEK while the room is playing is also projected forward", async ({ page, locast }) => {
    await mountRoomWithRealVideo(page, VIEWER_ROOM, VIEWER_ID);
    await setSkew(page, 0);
    await locast.emitPlaybackState(event(1, "play", 0, 0));
    await expect.poll(async () => (await videoState(page)).paused).toBe(false);

    // 1 s + 7 s of age = about 8 s. Natural playback from the start cannot
    // reach that inside the poll window, so only the projection can.
    await locast.emitPlaybackState(event(2, "seek", 1_000, 7_000));
    await expect.poll(async () => (await videoState(page)).currentTime).toBeGreaterThan(7.9);
    expect((await videoState(page)).currentTime).toBeLessThan(9.8);
});

test("a PAUSE is frozen at its position however late it lands", async ({ page, locast }) => {
    await mountRoomWithRealVideo(page, VIEWER_ROOM, VIEWER_ID);
    await setSkew(page, 0);
    await locast.emitPlaybackState(event(1, "play", 0, 0));
    await expect.poll(async () => (await videoState(page)).paused).toBe(false);

    await locast.emitPlaybackState(event(2, "pause", 3_000, 60_000));
    await expect.poll(async () => (await videoState(page)).paused).toBe(true);
    const { currentTime } = await videoState(page);
    expect(currentTime).toBeGreaterThan(2.9);
    expect(currentTime).toBeLessThan(3.2);
});

test("a SEEK in a paused room stays at its position", async ({ page, locast }) => {
    await mountRoomWithRealVideo(page, VIEWER_ROOM, VIEWER_ID);
    await setSkew(page, 0);
    await locast.emitPlaybackState(event(1, "play", 0, 0));
    await expect.poll(async () => (await videoState(page)).paused).toBe(false);
    await locast.emitPlaybackState(event(2, "pause", 1_000, 0));
    await expect.poll(async () => (await videoState(page)).paused).toBe(true);

    await locast.emitPlaybackState(event(3, "seek", 8_000, 30_000));
    await expect.poll(async () => (await videoState(page)).currentTime).toBeGreaterThan(7.9);
    const { currentTime, paused } = await videoState(page);
    expect(currentTime).toBeLessThan(8.2);
    expect(paused).toBe(true);
});

test("a measured clock skew is honoured when projecting", async ({ page, locast }) => {
    await mountRoomWithRealVideo(page, VIEWER_ROOM, VIEWER_ID);
    // The server's clock is 3 s ahead of this machine's (skew = server - local):
    // an event stamped "now" on the server is 3 s in the local future, so it
    // has aged by nothing and must not move the viewer ahead.
    await setSkew(page, 3_000);
    await locast.emitPlaybackState({ ...event(1, "play", 2_000, 0), server_ts_ms: Date.now() + 3_000 });
    await expect.poll(async () => (await videoState(page)).paused).toBe(false);
    const { currentTime } = await videoState(page);
    expect(currentTime).toBeGreaterThan(1.9);
    expect(currentTime).toBeLessThan(3.5);
});
