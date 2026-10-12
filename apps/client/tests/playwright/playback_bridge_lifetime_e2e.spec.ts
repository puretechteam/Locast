// Playback events must not be lost while the room page is not on screen.
//
// A viewer who does not have the shared file yet downloads it behind the
// blocking guard, which unmounts the room page for the whole transfer (and any
// viewer who wanders to the Library does the same). The host may press Play in
// the meantime. The `playback://state` listener used to live inside the room
// page, so those events were dropped and the viewer came back to a player that
// knew nothing about the room.

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

async function spaNavigate(page: Page, path: string): Promise<void> {
    await page.evaluate((to) => {
        window.history.pushState({}, "", to);
        window.dispatchEvent(new PopStateEvent("popstate"));
    }, path);
}

test("a PLAY sent while the viewer is away from the room page is applied on return", async ({
    page,
    locast,
}) => {
    await mountRoomWithRealVideo(page, VIEWER_ROOM, VIEWER_ID);
    await page.evaluate(() => {
        const w = window as unknown as {
            __locastClockSkew: { setSkewJitter: (s: number | null, j: number | null) => void };
        };
        w.__locastClockSkew.setSkewJitter(0, 0);
    });

    // Leave the room page (the download guard does the same for minutes).
    await spaNavigate(page, "/library");
    await page.waitForSelector('[data-testid="locast-player"]', { state: "detached" });

    await locast.emitPlaybackState({
        room_id: VIEWER_ROOM.id,
        server_seq: 1,
        server_ts_ms: Date.now() - 3_000,
        sender_id: OTHER_HOST_ID,
        monotonic_seq: 1,
        kind: "play",
        media_position_ms: 1_000,
    });

    // Back in the room, the player must be playing at the host's position.
    await spaNavigate(page, `/rooms/${VIEWER_ROOM.id}`);
    await page.waitForSelector('[data-testid="locast-player-video"]');
    await page.waitForFunction(
        () => {
            const v = document.querySelector<HTMLVideoElement>('[data-testid="locast-player-video"]');
            return v !== null && v.readyState >= 1;
        },
        undefined,
        { timeout: 10_000 },
    );
    await expect.poll(async () => (await videoState(page)).paused, { timeout: 8_000 }).toBe(false);
    // 1 s + 3 s of age = about 4 s (plus the time spent navigating back).
    const { currentTime } = await videoState(page);
    expect(currentTime).toBeGreaterThan(3.8);
    expect(currentTime).toBeLessThan(7);
});
