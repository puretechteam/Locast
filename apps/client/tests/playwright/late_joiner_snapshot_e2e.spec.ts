// A viewer who joins late starts in step with the host.
//
// The room summary the server sends on join now carries the playback state
// (`RoomSummaryIpc.playback`): whether the room is paused, the position at the
// last command and the server time of that command. Before, a late joiner
// knew nothing: `hostPaused` defaulted to false, no position was known, and
// the first command it did see was held for 5 s by the sequence-gap buffer
// (and a SEEK in a paused room would have unpaused it). A real Chromium
// <video> is the system under test.

import { test, expect, injectLocastShim } from "./fixtures/vite-app";
import {
    OTHER_HOST_ID,
    VIEWER_ID,
    VIEWER_ROOM,
    mountRoomWithRealVideo,
    type RoomFixture,
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

function snapshot(over: Partial<NonNullable<RoomFixture["playback"]>>): NonNullable<RoomFixture["playback"]> {
    return {
        paused: false,
        media_position_ms: 1_000,
        server_ts_ms: Date.now(),
        server_seq: 5,
        sender_id: OTHER_HOST_ID,
        monotonic_seq: 5,
        ...over,
    };
}

async function setSkew0(page: Page): Promise<void> {
    await page.evaluate(() => {
        const w = window as unknown as {
            __locastClockSkew: { setSkewJitter: (s: number | null, j: number | null) => void };
        };
        w.__locastClockSkew.setSkewJitter(0, 0);
    });
}

async function hostPaused(page: Page): Promise<boolean> {
    return await page.evaluate(() =>
        (window as unknown as { __locastStore: { getHostPaused: () => boolean } }).__locastStore.getHostPaused(),
    );
}

test("joining while the host is playing starts playing at the host's position", async ({ page }) => {
    const room = { ...VIEWER_ROOM, playback: snapshot({ media_position_ms: 500, server_ts_ms: Date.now() - 8_000 }) };
    // The snapshot is applied as soon as the media is ready, which can be
    // BEFORE the first clock probe; the position is corrected when the skew
    // arrives (here: after).
    await mountRoomWithRealVideo(page, room, VIEWER_ID);
    await setSkew0(page);

    // 0.5 s + 8 s of age = 8.5 s. Natural playback from the start would need
    // 8 s to get there, far beyond this window: only the projection can.
    await expect.poll(async () => (await videoState(page)).currentTime, { timeout: 3_000 }).toBeGreaterThan(8.4);
    expect((await videoState(page)).paused).toBe(false);
    expect(await hostPaused(page)).toBe(false);
});

test("joining while the host is paused stays paused at the host's position", async ({ page }) => {
    const room = {
        ...VIEWER_ROOM,
        playback: snapshot({ paused: true, media_position_ms: 6_000, server_ts_ms: Date.now() - 300_000 }),
    };
    await mountRoomWithRealVideo(page, room, VIEWER_ID);
    await setSkew0(page);

    await expect.poll(async () => (await videoState(page)).currentTime).toBeGreaterThan(5.9);
    const { currentTime, paused } = await videoState(page);
    expect(currentTime).toBeLessThan(6.2);
    expect(paused).toBe(true);
    // The drift clock and Sync must freeze: this is the `hostPaused` the
    // joiner used to get wrong (always false).
    expect(await hostPaused(page)).toBe(true);
});

test("the host's next command applies at once, not after the 5 s gap buffer", async ({ page, locast }) => {
    const room = {
        ...VIEWER_ROOM,
        playback: snapshot({ paused: true, media_position_ms: 6_000, server_seq: 5, monotonic_seq: 5 }),
    };
    await mountRoomWithRealVideo(page, room, VIEWER_ID);
    await setSkew0(page);
    await expect.poll(async () => (await videoState(page)).currentTime).toBeGreaterThan(5.9);

    // seq 6 is exactly what the joiner expects after the snapshot's seq 5.
    await locast.emitPlaybackState({
        room_id: VIEWER_ROOM.id,
        server_seq: 6,
        server_ts_ms: Date.now(),
        sender_id: OTHER_HOST_ID,
        monotonic_seq: 6,
        kind: "play",
        media_position_ms: 2_000,
    });
    // Well inside the 5 s the dedup buffer would have held it.
    await expect.poll(async () => (await videoState(page)).paused, { timeout: 2_500 }).toBe(false);
    expect((await videoState(page)).currentTime).toBeLessThan(4.5);
    expect(await hostPaused(page)).toBe(false);
});

test("a snapshot older than what the live stream delivered is ignored", async ({ page, locast }) => {
    await mountRoomWithRealVideo(page, VIEWER_ROOM, VIEWER_ID);
    await setSkew0(page);
    await locast.emitPlaybackState({
        room_id: VIEWER_ROOM.id,
        server_seq: 1,
        server_ts_ms: Date.now(),
        sender_id: OTHER_HOST_ID,
        monotonic_seq: 1,
        kind: "play",
        media_position_ms: 2_000,
    });
    await expect.poll(async () => (await videoState(page)).paused).toBe(false);
    await locast.emitPlaybackState({
        room_id: VIEWER_ROOM.id,
        server_seq: 2,
        server_ts_ms: Date.now(),
        sender_id: OTHER_HOST_ID,
        monotonic_seq: 2,
        kind: "pause",
        media_position_ms: 3_000,
    });
    await expect.poll(async () => (await videoState(page)).paused).toBe(true);

    // A stale ROOM_STATE (snapshot from before the pause) must not undo it.
    const stale = { ...VIEWER_ROOM, playback: snapshot({ paused: false, server_seq: 1, monotonic_seq: 1 }) };
    await page.evaluate((r) => {
        (window as unknown as { __locastRoomStore: { setSummary: (s: unknown) => void } }).__locastRoomStore.setSummary(r);
    }, stale);
    expect(await hostPaused(page)).toBe(true);
    expect((await videoState(page)).paused).toBe(true);
});
