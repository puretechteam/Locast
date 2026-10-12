// The drift sampler must keep ticking while the room page re-renders often.
//
// The 1 Hz drift sampler is a timer created in an effect. The room page gave it
// fresh inline functions for reading the local video on every render, and the
// effect depended on them, so EVERY render of the page cleared the timer and
// started a new one. Anything that re-renders the page more than once a second
// (peers' position reports, chat) therefore starved the sampler: it never
// ticked, the drift EMA never moved and the "you are out of sync" indicator
// never appeared, however far behind the viewer was.
//
// A real Chromium <video> plays behind the host here while position reports
// stream in faster than the sampler's period.

import { test, expect, injectLocastShim } from "./fixtures/vite-app";
import {
    OTHER_HOST_ID,
    VIEWER_ID,
    VIEWER_ROOM,
    mountRoomWithRealVideo,
    userGesture,
} from "./fixtures/real-video-room";

test.beforeEach(async ({ page, locast }) => {
    await injectLocastShim(page);
    await page.goto("/");
    await page.waitForLoadState("domcontentloaded");
    await locast.waitForBridge();
    await locast.resetInvokeLog();
});

test("a viewer far behind the host is told so even while position reports pour in", async ({
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

    // The host is playing at 8 s; the viewer is made to play from the start.
    await locast.emitPlaybackState({
        room_id: VIEWER_ROOM.id,
        server_seq: 1,
        server_ts_ms: Date.now(),
        sender_id: OTHER_HOST_ID,
        monotonic_seq: 1,
        kind: "play",
        media_position_ms: 8_000,
    });
    await userGesture(page, "v.currentTime = 0; await v.play();");

    // Position reports from two peers, every 250 ms: each one re-renders the
    // room page, four times per sampler period.
    await page.evaluate(
        ({ room, a, b }) => {
            const w = window as unknown as {
                __locast: { emitPositionReport: (p: unknown) => Promise<void> };
            };
            let n = 0;
            window.setInterval(() => {
                n += 1;
                for (const sender_id of [a, b]) {
                    void w.__locast.emitPositionReport({
                        room_id: room,
                        sender_id,
                        media_position_ms: n * 250,
                        playing: true,
                        client_ts_ms: Date.now(),
                    });
                }
            }, 250);
        },
        { room: VIEWER_ROOM.id, a: OTHER_HOST_ID, b: "44444444-4444-4444-4444-444444444444" },
    );

    // ~8 s behind: the sampler must tick (it needs a few of them) and the
    // indicator must show, well within this window.
    await expect(page.locator('[data-testid="drift-indicator"]')).toHaveCount(1, { timeout: 8_000 });
    await expect(page.locator('[data-testid="drift-indicator"]')).toHaveAttribute(
        "data-direction",
        "behind",
    );
});
