// Chat history used to live in the room page's own state, fed by a listener the
// page registered. The page is unmounted while a download runs (the blocking
// guard hides it), so a viewer who joined and started downloading lost every
// message sent meanwhile, and the history already received. The history now
// lives in a store fed by an app-level listener.

import { test, expect, injectLocastShim } from "./fixtures/vite-app";
import type { Page } from "@playwright/test";

const ROOM_ID = "r-chat-history";
const HOST_ID = "aaaa0000-0000-0000-0000-00000000000f";
const MEDIA_ID = "aabbccdd-1111-2222-3333-444455556666";
const CAP_CHAT = 0x80;

function message(text: string, n: number) {
    return {
        room_id: ROOM_ID,
        sender_id: HOST_ID,
        sender_name: "host",
        text,
        reply_to: null,
        ts_ms: 1_700_000_000_000 + n,
    };
}

function summary() {
    return {
        id: ROOM_ID,
        code: "CHAT01",
        title: "chat",
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
        you_cap_set: CAP_CHAT,
    };
}

async function showRoom(page: Page): Promise<void> {
    await page.evaluate((s) => {
        (window as unknown as { __locastRoomStore: { setSummary: (s: unknown) => void } })
            .__locastRoomStore.setSummary(s);
    }, summary());
    await page.waitForSelector('[data-testid="locast-player"]', { timeout: 5_000 });
}

async function enterRoom(page: Page): Promise<void> {
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
    await showRoom(page);
}

test.beforeEach(async ({ page, locast }) => {
    await injectLocastShim(page);
    await page.goto("/");
    await page.waitForLoadState("domcontentloaded");
    await locast.waitForBridge();
});

test("messages sent while a download hides the room page are shown afterwards", async ({ page, locast }) => {
    await enterRoom(page);
    await locast.emitChatMessage(message("before the download", 1));
    await expect(page.getByText("before the download")).toBeVisible();

    // The download starts: the room page, and the history it held, is gone.
    await locast.emitDownloadState({ id: "d1", media_id: MEDIA_ID, state: "transferring" });
    await expect(page.locator('[data-testid="locast-player"]')).toHaveCount(0);
    await locast.emitChatMessage(message("during the download", 2));
    await locast.emitChatMessage(message("still downloading", 3));

    // It finishes and the room page comes back with the whole history, in order.
    await locast.emitDownloadState({ id: "d1", media_id: MEDIA_ID, state: "complete" });
    await page.waitForSelector('[data-testid="locast-player"]', { timeout: 5_000 });
    const texts = await page.locator(".chat-panel__messages").innerText();
    expect(texts).toContain("before the download");
    expect(texts).toContain("during the download");
    expect(texts).toContain("still downloading");
    expect(texts.indexOf("before the download")).toBeLessThan(texts.indexOf("during the download"));
    expect(texts.indexOf("during the download")).toBeLessThan(texts.indexOf("still downloading"));
});

test("a room that ends takes its chat history with it", async ({ page, locast }) => {
    await enterRoom(page);
    await locast.emitChatMessage(message("the old room's message", 1));
    await expect(page.getByText("the old room's message")).toBeVisible();

    await locast.emitRoomState(null);
    await expect(page.locator('[data-testid="room-empty"]')).toBeVisible();

    // Entering another room starts with an empty history.
    await showRoom(page);
    await expect(page.getByText("No messages yet")).toBeVisible();
    await expect(page.getByText("the old room's message")).toHaveCount(0);
});
