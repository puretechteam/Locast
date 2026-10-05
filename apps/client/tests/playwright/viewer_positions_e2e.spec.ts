// The host's "Viewer positions" list. A viewer who left stayed in it with an age
// counter that grew forever (`removeViewer` had no callers), and still counted
// toward the room median.

import { test, expect, injectLocastShim } from "./fixtures/vite-app";
import type { Page } from "@playwright/test";

const ROOM_ID = "r-viewer-positions";
const HOST_ID = "aaaa0000-0000-0000-0000-00000000000d";
const V1_ID = "bbbb0000-0000-0000-0000-0000000000a1";
const V2_ID = "bbbb0000-0000-0000-0000-0000000000a2";

function participant(id: string, name: string, isHost: boolean) {
    return {
        user_id: id,
        display_name: name,
        joined_ms: 1_700_000_000_000,
        status: "Connected" as const,
        last_seen_ms: 1_700_000_000_000,
        is_host: isHost,
    };
}

function hostSummary(viewers: string[]) {
    return {
        id: ROOM_ID,
        code: "POSIT1",
        title: "positions",
        host_user_id: HOST_ID,
        host_migration_enabled: true,
        created_ms: 1_700_000_000_000,
        participants: [
            participant(HOST_ID, "host", true),
            ...viewers.map((id, i) => participant(id, `viewer-${i}`, false)),
        ],
        host_disconnected: false,
        host_disconnect_deadline_ms: null,
        you_user_id: HOST_ID,
    };
}

async function setSummary(page: Page, viewers: string[]): Promise<void> {
    await page.evaluate((s) => {
        (window as unknown as { __locastRoomStore: { setSummary: (s: unknown) => void } })
            .__locastRoomStore.setSummary(s);
    }, hostSummary(viewers));
}

test.beforeEach(async ({ page, locast }) => {
    await injectLocastShim(page);
    await page.goto("/");
    await page.waitForLoadState("domcontentloaded");
    await locast.waitForBridge();
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
    await setSummary(page, [V1_ID, V2_ID]);
    await page.waitForSelector('[data-testid="viewer-positions"]', { timeout: 5_000 });
});

function report(senderId: string, positionMs: number) {
    return {
        room_id: ROOM_ID,
        sender_id: senderId,
        media_position_ms: positionMs,
        playing: true,
        client_ts_ms: Date.now(),
    };
}

const rows = '[data-testid="viewer-position-row"]';

test("a viewer who leaves disappears from the host's positions list", async ({ page, locast }) => {
    await locast.emitPositionReport(report(V1_ID, 10_000));
    await locast.emitPositionReport(report(V2_ID, 20_000));
    await expect(page.locator(rows)).toHaveCount(2);

    // V2 leaves: the next summary no longer lists them.
    await setSummary(page, [V1_ID]);

    await expect(page.locator(rows)).toHaveCount(1);
    await expect(page.locator(rows)).toHaveAttribute("data-sender-id", V1_ID);
});

test("the remaining viewers are untouched when someone leaves", async ({ page, locast }) => {
    await locast.emitPositionReport(report(V1_ID, 10_000));
    await locast.emitPositionReport(report(V2_ID, 20_000));
    await expect(page.locator(rows)).toHaveCount(2);

    await setSummary(page, [V2_ID]);

    await expect(page.locator(rows)).toHaveCount(1);
    await expect(page.locator(rows)).toHaveAttribute("data-sender-id", V2_ID);
});

test("a summary update that keeps everyone keeps every row", async ({ page, locast }) => {
    await locast.emitPositionReport(report(V1_ID, 10_000));
    await locast.emitPositionReport(report(V2_ID, 20_000));
    await expect(page.locator(rows)).toHaveCount(2);

    await setSummary(page, [V1_ID, V2_ID]);

    await expect(page.locator(rows)).toHaveCount(2);
});
