// A room that ends (or that the user leaves from the download dialog) must not
// be shown again. The room store was only cleared by the room page's own
// listener, so a room that ended while the page was unmounted (hidden by the
// download guard, or while the user was on another page) came back from the
// store on the next visit, complete with participants and a Leave button for a
// room that no longer existed. Other room-scoped stores had no owner at all.

import { test, expect, injectLocastShim } from "./fixtures/vite-app";
import type { Page } from "@playwright/test";

const ROOM_ID = "r-room-end";
const HOST_ID = "aaaa0000-0000-0000-0000-00000000000c";
const MEDIA_ID = "aabbccdd-1111-2222-3333-444455556666";

function summary() {
    return {
        id: ROOM_ID,
        code: "ENDED1",
        title: "ending soon",
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
    };
}

async function spaNavigate(page: Page, path: string): Promise<void> {
    await page.evaluate((to) => {
        window.history.pushState({}, "", to);
        window.dispatchEvent(new PopStateEvent("popstate"));
    }, path);
}

async function enterRoom(page: Page): Promise<void> {
    await spaNavigate(page, `/rooms/${ROOM_ID}`);
    await page.waitForSelector('[data-testid="room-empty"]', { timeout: 5_000 });
    await page.waitForFunction(
        () => (window as { __locastRoomStore?: unknown }).__locastRoomStore !== undefined,
        undefined,
        { timeout: 5_000 },
    );
    await page.evaluate((s) => {
        (window as unknown as { __locastRoomStore: { setSummary: (s: unknown) => void } })
            .__locastRoomStore.setSummary(s);
    }, summary());
    await page.waitForSelector('[data-testid="locast-player"]', { timeout: 5_000 });
}

test.beforeEach(async ({ page, locast }) => {
    await injectLocastShim(page);
    await page.goto("/");
    await page.waitForLoadState("domcontentloaded");
    await locast.waitForBridge();
});

test("a room that ends while a download hides the room page is not shown again", async ({ page, locast }) => {
    await enterRoom(page);

    // A download starts: the guard unmounts the room page and the modal covers it.
    await locast.emitDownloadState({ id: "d1", media_id: MEDIA_ID, state: "transferring" });
    await expect(page.locator('[data-testid="dlm-dialog"]')).toBeVisible();
    await expect(page.locator('[data-testid="locast-player"]')).toHaveCount(0);

    // The room ends while the page is not mounted, so its own listener misses it.
    await locast.emitRoomState(null);

    // Nothing about that room is left: the dialog for its download is gone,
    // and the page shows the empty state instead of the dead room.
    await expect(page.locator('[data-testid="dlm-dialog"]')).toHaveCount(0);
    await expect(page.locator('[data-testid="room-empty"]')).toBeVisible();
    await expect(page.locator('[data-testid="locast-player"]')).toHaveCount(0);
});

test("leaving from the download dialog does not bring the room back", async ({ page, locast }) => {
    await enterRoom(page);
    // No source connected yet: the dialog offers a way out by leaving.
    await locast.emitDownloadState({ id: "d1", media_id: MEDIA_ID, state: "pending" });
    await page.locator('[data-testid="dlm-leave"]').click();

    await expect(page.locator('[data-testid="dlm-dialog"]')).toHaveCount(0);
    await expect(page.locator('[data-testid="room-empty"]')).toBeVisible();
    await expect(page.locator('[data-testid="locast-player"]')).toHaveCount(0);
});

test("a room that ends while the user is on another page is not shown on return", async ({ page, locast }) => {
    await enterRoom(page);
    await spaNavigate(page, "/library");
    await expect(page.locator('[data-testid="locast-player"]')).toHaveCount(0);

    await locast.emitRoomState(null);

    await spaNavigate(page, `/rooms/${ROOM_ID}`);
    await expect(page.locator('[data-testid="room-empty"]')).toBeVisible();
    await expect(page.locator('[data-testid="locast-player"]')).toHaveCount(0);
});
