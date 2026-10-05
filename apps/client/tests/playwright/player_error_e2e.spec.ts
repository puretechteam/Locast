// The Player's error banner. It was set by a failed media load or a rejected
// play() and never cleared, so it stayed up for the life of the room page, even
// after the user picked a different, working file.
//
// A media URL is made to fail or to hang with `page.route`, so no real video
// file is needed.

import { test, expect, injectLocastShim } from "./fixtures/vite-app";
import type { Page } from "@playwright/test";

const ROOM_ID = "r-player-error";
const HOST_ID = "aaaa0000-0000-0000-0000-00000000000b";
const BANNER = '[data-testid="locast-player-error"]';

async function openRoom(page: Page): Promise<void> {
    await injectLocastShim(page);
    await page.goto("/");
    await page.evaluate((to) => {
        window.history.pushState({}, "", to);
        window.dispatchEvent(new PopStateEvent("popstate"));
    }, `/rooms/${ROOM_ID}`);
    await page.waitForSelector('[data-testid="room-empty"]', { timeout: 5_000 });
    await page.waitForFunction(
        () => (window as { __locastRoomStore?: unknown }).__locastRoomStore !== undefined,
        undefined,
        { timeout: 5_000 },
    );
    await page.evaluate((s) => {
        (window as unknown as { __locastRoomStore: { setSummary: (s: unknown) => void } })
            .__locastRoomStore.setSummary(s);
    }, {
        id: ROOM_ID,
        code: "PLAYER",
        title: "player",
        host_user_id: HOST_ID,
        host_migration_enabled: true,
        created_ms: 1_700_000_000_000,
        participants: [
            {
                user_id: HOST_ID,
                display_name: "host",
                joined_ms: 1_700_000_000_000,
                status: "Connected" as const,
                last_seen_ms: 1_700_000_000_000,
                is_host: true,
            },
        ],
        host_disconnected: false,
        host_disconnect_deadline_ms: null,
    });
    await page.waitForSelector('[data-testid="locast-player"]', { timeout: 5_000 });
    await page.waitForFunction(
        () => (window as { __locastStore?: unknown }).__locastStore !== undefined,
        undefined,
        { timeout: 5_000 },
    );
}

async function setMediaSrc(page: Page, src: string): Promise<void> {
    await page.evaluate((s) => {
        (window as unknown as { __locastStore: { setMediaSrc: (s: string) => void } })
            .__locastStore.setMediaSrc(s);
    }, src);
}

test("a failed load shows the banner, and choosing another source clears it", async ({ page }) => {
    // The first file fails outright; the second never finishes loading, so it
    // neither plays nor fails.
    await page.route("**/test/broken.mp4", (route) =>
        route.fulfill({ status: 404, body: "not found" }),
    );
    await page.route("**/test/hangs.mp4", () => {
        /* never answered */
    });
    await openRoom(page);

    await setMediaSrc(page, "/test/broken.mp4");
    await expect(page.locator(BANNER)).toContainText("media load failed");

    await setMediaSrc(page, "/test/hangs.mp4");
    await expect(page.locator(BANNER)).toHaveCount(0);
});

test("the banner does not appear when nothing has failed", async ({ page }) => {
    await page.route("**/test/hangs.mp4", () => {
        /* never answered */
    });
    await openRoom(page);
    await setMediaSrc(page, "/test/hangs.mp4");
    await expect(page.locator('[data-testid="locast-player-video"]')).toBeVisible();
    await expect(page.locator(BANNER)).toHaveCount(0);
});
