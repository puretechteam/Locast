// P7-T03 acceptance: the client RoomTopBar must show a
// "host reconnecting" banner when the room summary
// transitions to `host_disconnected: true`, and the page
// must return to the empty-room state when `room://state`
// emits `null` (the server-side RoomClosed signal).
//
// This spec drives the UI through the Vite test shim
// rather than a real Tauri runtime, so no real 30 s grace
// window is waited on.

import { test, expect, injectLocastShim } from "./fixtures/vite-app";
import type { Page } from "@playwright/test";

const ROOM_ID = "r-p7t03";
const HOST_ID = "aaaa0000-0000-0000-0000-000000000010";

function makeSummary(overrides: Record<string, unknown> = {}) {
    return {
        id: ROOM_ID,
        code: "P7T03",
        title: "P7-T03 host disconnect",
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
        ...overrides,
    };
}

async function mountRoom(page: Page): Promise<void> {
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
    }, makeSummary());
    await page.waitForSelector('[data-testid="locast-player"]', { timeout: 5_000 });
}

test.beforeEach(async ({ page, locast }) => {
    await injectLocastShim(page);
    await page.goto("/");
    await page.waitForLoadState("domcontentloaded");
    await locast.waitForBridge();
});

test("host_disconnected summary shows grace banner; RoomClosed returns to empty state", async ({ page, locast }) => {
    await mountRoom(page);

    const deadlineMs = Date.now() + 60_000;
    await locast.emitCapabilityUpdate(
        makeSummary({
            participants: [
                {
                    user_id: HOST_ID,
                    display_name: "host",
                    joined_ms: 1_700_000_000_000,
                    status: "Reconnecting" as const,
                    last_seen_ms: 1_700_000_000_000,
                    is_host: true,
                },
            ],
            host_disconnected: true,
            host_disconnect_deadline_ms: deadlineMs,
            you_cap_set: 0,
        }),
    );

    await page.waitForSelector(".room-top-bar__grace-banner", { timeout: 5_000 });
    const banner = page.locator(".room-top-bar__grace-banner");
    await expect(banner).toBeVisible();
    await expect(banner).toContainText("Host reconnecting");

    await locast.emitRoomState(null);

    await page.waitForSelector('[data-testid="room-empty"]', { timeout: 5_000 });
    await expect(page.locator('[data-testid="room-empty"]')).toBeVisible();
    await expect(page.locator(".room-top-bar__toast")).toContainText("Room ended");
});
