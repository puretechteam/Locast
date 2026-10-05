// When the host presses Play, Pause or Seek the host's own video moves first and
// the command is then sent so viewers follow. If the send fails, nothing told the
// host: their video played, viewers did not follow, and the only trace was a
// console warning. The controls now say so.
//
// `playback_send` is stubbed by wrapping the shim's invoke (registered after
// `injectLocastShim`, which installs the invoke it wraps); the page's
// `__playback_fail` flag makes it reject with Rust's `{ kind, message }` shape.

import { test, expect, injectLocastShim } from "./fixtures/vite-app";
import type { Page } from "@playwright/test";

const ROOM_ID = "r-host-playback-failure";
const HOST_ID = "aaaa0000-0000-0000-0000-000000000010";
const ALERT = '[data-testid="playback-send-failed"]';

async function openAsHost(page: Page): Promise<void> {
    await injectLocastShim(page);
    await page.addInitScript(() => {
        const w = window as unknown as {
            __TAURI_INTERNALS__: {
                invoke: (name: string, args?: unknown, options?: unknown) => Promise<unknown>;
            };
            __playback_fail: boolean;
            __playback_sends: number;
        };
        w.__playback_fail = false;
        w.__playback_sends = 0;
        const original = w.__TAURI_INTERNALS__.invoke;
        w.__TAURI_INTERNALS__.invoke = (name, args, options) => {
            if (name !== "playback_send") return original(name, args, options);
            w.__playback_sends += 1;
            return w.__playback_fail
                ? Promise.reject({ kind: "Other", message: "not connected" })
                : Promise.resolve({ envelope_id: "e", monotonic_seq: 1 });
        };
    });
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
        code: "PBFAIL",
        title: "playback",
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
        you_user_id: HOST_ID,
    });
    await page.waitForSelector('[data-testid="locast-playback-controls"]', { timeout: 5_000 });
}

async function setFail(page: Page, fail: boolean): Promise<void> {
    await page.evaluate((f) => {
        (window as unknown as { __playback_fail: boolean }).__playback_fail = f;
    }, fail);
}

async function sends(page: Page): Promise<number> {
    return await page.evaluate(
        () => (window as unknown as { __playback_sends: number }).__playback_sends,
    );
}

test("a failed Play tells the host that viewers did not get it", async ({ page }) => {
    await openAsHost(page);
    await setFail(page, true);
    await page.locator('[data-testid="locast-playback-play"]').click();

    await expect(page.locator(ALERT)).toBeVisible();
    await expect(page.locator(ALERT)).toContainText("did not get");
    expect(await sends(page)).toBe(1);
});

test("the message goes away once a command goes through", async ({ page }) => {
    await openAsHost(page);
    await setFail(page, true);
    await page.locator('[data-testid="locast-playback-play"]').click();
    await expect(page.locator(ALERT)).toBeVisible();

    await setFail(page, false);
    await page.locator('[data-testid="locast-playback-play"]').click();

    await expect(page.locator(ALERT)).toHaveCount(0);
    expect(await sends(page)).toBe(2);
});

test("a successful Play shows no message", async ({ page }) => {
    await openAsHost(page);
    await page.locator('[data-testid="locast-playback-play"]').click();
    await expect.poll(() => sends(page)).toBe(1);
    await expect(page.locator(ALERT)).toHaveCount(0);
});
