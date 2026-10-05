// A component that throws while rendering used to blank the whole window: React
// unmounts the tree, nothing is left to click, and the only recovery was
// restarting the app. The error boundary shows a fallback with a way out.
//
// The room page is made to throw by giving the room store a summary whose
// participant list is `null`, which the page maps over while rendering.

import { test, expect, injectLocastShim } from "./fixtures/vite-app";

const ROOM_ID = "r-error-boundary";

test.beforeEach(async ({ page, locast }) => {
    await injectLocastShim(page);
    await page.goto("/");
    await page.waitForLoadState("domcontentloaded");
    await locast.waitForBridge();
});

async function breakTheRoomPage(page: import("@playwright/test").Page): Promise<void> {
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
    await page.evaluate((id) => {
        (window as unknown as { __locastRoomStore: { setSummary: (s: unknown) => void } })
            .__locastRoomStore.setSummary({
                id,
                code: "BROKEN",
                title: "broken",
                host_user_id: "aaaa0000-0000-0000-0000-00000000000e",
                host_migration_enabled: true,
                created_ms: 1_700_000_000_000,
                participants: null,
                host_disconnected: false,
                host_disconnect_deadline_ms: null,
            });
    }, ROOM_ID);
}

test("a render failure shows a fallback instead of a blank window", async ({ page }) => {
    await breakTheRoomPage(page);

    const fallback = page.locator('[data-testid="error-boundary"]');
    await expect(fallback).toBeVisible();
    await expect(fallback).toContainText("Something went wrong");
    await expect(page.locator('[data-testid="error-boundary-detail"]')).not.toBeEmpty();
    await expect(page.getByRole("button", { name: "Reload" })).toBeVisible();
    await expect(page.getByRole("link", { name: "Back to library" })).toBeVisible();
});

test("the fallback's link recovers the app", async ({ page }) => {
    await breakTheRoomPage(page);
    await expect(page.locator('[data-testid="error-boundary"]')).toBeVisible();

    await page.getByRole("link", { name: "Back to library" }).click();

    await expect(page).toHaveURL(/\/library$/);
    await expect(page.locator('[data-testid="error-boundary"]')).toHaveCount(0);
    await expect(page.getByRole("link", { name: "Settings" })).toBeVisible();
});

test("a healthy page is not wrapped in a fallback", async ({ page }) => {
    await expect(page.locator('[data-testid="error-boundary"]')).toHaveCount(0);
    await expect(page).toHaveURL(/\/library$/);
});
