import { test, expect, injectLocastShim } from "./fixtures/vite-app";
import type { Page } from "@playwright/test";

const CAP_CHAT = 0x80;

const HOST_ID = "11111111-1111-1111-1111-111111111111";
const VIEWER_ID = "22222222-2222-2222-2222-222222222222";

const ROOM_ID = "r-p6t03-chat";

function makeRoomSummary(youCapSet: number | undefined) {
    return {
        id: ROOM_ID,
        code: "P6T03",
        title: "P6-T03",
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
            {
                user_id: VIEWER_ID,
                display_name: "viewer",
                joined_ms: 1_700_000_000_500,
                status: "Connected" as const,
                last_seen_ms: 1_700_000_000_500,
                is_host: false,
            },
        ],
        host_disconnected: false,
        host_disconnect_deadline_ms: null,
        you_cap_set: youCapSet,
    };
}

function makeChatMessage(senderId: string, senderName: string, text: string, tsOffset: number, replyTo: string | null = null) {
    return {
        room_id: ROOM_ID,
        sender_id: senderId,
        sender_name: senderName,
        text,
        reply_to: replyTo,
        ts_ms: 1_700_000_000_000 + tsOffset,
    };
}

async function spaNavigate(page: Page, path: string): Promise<void> {
    await page.evaluate((to) => {
        window.history.pushState({}, "", to);
        window.dispatchEvent(new PopStateEvent("popstate"));
    }, path);
}

async function setupClient(p: Page): Promise<void> {
    await injectLocastShim(p);
    await p.goto("/");
    await p.waitForLoadState("domcontentloaded");
    await spaNavigate(p, `/rooms/${ROOM_ID}`);
    await p.waitForSelector('[data-testid="room-empty"]', { timeout: 5_000 });
    await p.waitFunction(
        () => (window as { __locastRoomStore?: unknown }).__locastRoomStore !==
            undefined,
        undefined,
        { timeout: 5_000 },
    );
}

async function hydrateRoomPage(page: Page, capSet: number): Promise<void> {
    const summary = makeRoomSummary(capSet);
    await page.evaluate((s) => {
        const w = window as unknown as {
            __locastRoomStore?: { setSummary: (s: unknown) => void };
        };
        w.__locastRoomStore!.setSummary(s);
    }, summary);

    await page.waitForSelector('[data-testid="room-empty"]', {
        state: "detached",
        timeout: 5_000,
    });
    await page.waitForSelector('[data-testid="locast-player"]', {
        timeout: 5_000,
    });
    await page.waitForFunction(
        () => (window as { __locastStore?: unknown }).__locastStore !==
            undefined,
        undefined,
        { timeout: 5_000 },
    );
    await page.evaluate(() => {
        const w = window as unknown as {
            __locastStore?: {
                setMediaSrc: (s: string) => void;
                setMediaReady: (r: boolean) => void;
            };
        };
        w.__locastStore!.setMediaSrc("/test/asset.mp4");
        w.__locastStore!.setMediaReady(true);
    });
}

test("chat panel hidden when no CHAT cap", async ({ page }) => {
    await setupClient(page);
    await hydrateRoomPage(page, 0);

    const chatPanel = page.locator('[data-testid="chat-panel"]');
    await expect(chatPanel).toHaveCount(0);
});

test("chat panel visible with CHAT cap", async ({ page }) => {
    await setupClient(page);
    await hydrateRoomPage(page, CAP_CHAT);

    const chatPanel = page.locator('[data-testid="chat-panel"]');
    await expect(chatPanel).toBeVisible();
});

test("messages > 2 KiB show inline error", async ({ page }) => {
    await setupClient(page);
    await hydrateRoomPage(page, CAP_CHAT);

    const input = page.locator('[data-testid="chat-input"]');
    const sendBtn = page.locator('[data-testid="chat-send-btn"]');
    const errorEl = page.locator('[data-testid="chat-error"]');

    const largeText = "a".repeat(2049);
    await input.fill(largeText);

    await expect(errorEl).toBeVisible();
    await expect(errorEl).toContainText("2048 character limit");

    const sendBtnDisabled = await sendBtn.isDisabled();
    expect(sendBtnDisabled).toBe(true);
});

test("host and viewer exchange 10 messages", async ({ page: hostPage, page: viewerPage, locast }) => {
    await setupClient(hostPage);
    await setupClient(viewerPage);

    await hydrateRoomPage(hostPage, CAP_CHAT);
    await hydrateRoomPage(viewerPage, CAP_CHAT);

    const hostInput = hostPage.locator('[data-testid="chat-input"]');
    const viewerInput = viewerPage.locator('[data-testid="chat-input"]');
    const hostSendBtn = hostPage.locator('[data-testid="chat-send-btn"]');
    const viewerSendBtn = viewerPage.locator('[data-testid="chat-send-btn"]');

    for (let i = 1; i <= 5; i++) {
        await hostInput.fill(`host message ${i}`);
        await hostSendBtn.click();
        await hostPage.waitForTimeout(100);
    }

    for (let i = 1; i <= 5; i++) {
        await viewerInput.fill(`viewer message ${i}`);
        await viewerSendBtn.click();
        await viewerPage.waitForTimeout(100);
    }

    for (let i = 1; i <= 5; i++) {
        const hostMsg = makeChatMessage(HOST_ID, "host", `host message ${i}`, i * 1000);
        await locast.emitChatMessage(hostMsg);
        await hostPage.waitForTimeout(50);
    }

    for (let i = 1; i <= 5; i++) {
        const viewerMsg = makeChatMessage(VIEWER_ID, "viewer", `viewer message ${i}`, (i + 5) * 1000);
        await locast.emitChatMessage(viewerMsg);
        await viewerPage.waitForTimeout(50);
    }

    await hostPage.waitForTimeout(300);
    await viewerPage.waitForTimeout(300);

    const hostMessages = hostPage.locator('[data-testid="chat-message"]');
    const viewerMessages = viewerPage.locator('[data-testid="chat-message"]');

    await expect(hostMessages).toHaveCount(10);
    await expect(viewerMessages).toHaveCount(10);

    const hostMessageTexts = await hostMessages.allTextContents();
    const viewerMessageTexts = await viewerMessages.allTextContents();
    expect(hostMessageTexts).toEqual(viewerMessageTexts);
});
