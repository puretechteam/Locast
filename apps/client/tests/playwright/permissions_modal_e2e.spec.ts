// The host's Permissions dialog. Apply sends one `room_permission_set` per
// participant whose preset changed. It used to close the dialog in a `finally`,
// so a failure part-way looked like success while only some changes had landed.
//
// `room_permission_set` is stubbed by wrapping the shim's invoke (registered
// after `injectLocastShim`, which installs the invoke it wraps): it records
// every call and rejects with Rust's `{ kind, message }` shape while the page's
// `__permissions_fail_from` counter says to.

import { test, expect, injectLocastShim } from "./fixtures/vite-app";
import type { Page } from "@playwright/test";

const ROOM_ID = "r-permissions-modal";
const HOST_ID = "aaaa0000-0000-0000-0000-00000000000a";
const V1_ID = "bbbb0000-0000-0000-0000-000000000001";
const V2_ID = "bbbb0000-0000-0000-0000-000000000002";

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

function hostSummary() {
    return {
        id: ROOM_ID,
        code: "PERMS1",
        title: "perms",
        host_user_id: HOST_ID,
        host_migration_enabled: true,
        created_ms: 1_700_000_000_000,
        participants: [
            participant(HOST_ID, "host", true),
            participant(V1_ID, "viewer-one", false),
            participant(V2_ID, "viewer-two", false),
        ],
        host_disconnected: false,
        host_disconnect_deadline_ms: null,
        you_user_id: HOST_ID,
    };
}

async function openAsHost(page: Page): Promise<void> {
    await injectLocastShim(page);
    await page.addInitScript(() => {
        const w = window as unknown as {
            __TAURI_INTERNALS__: {
                invoke: (name: string, args?: unknown, options?: unknown) => Promise<unknown>;
            };
            __permission_calls: Array<{ targetUserId: string; addCapSet: number; removeCapSet: number }>;
            // 1-based index of the first call that fails; 0 means none do.
            __permissions_fail_from: number;
        };
        w.__permission_calls = [];
        w.__permissions_fail_from = 0;
        const original = w.__TAURI_INTERNALS__.invoke;
        w.__TAURI_INTERNALS__.invoke = (name, args, options) => {
            if (name !== "room_permission_set") return original(name, args, options);
            w.__permission_calls.push(
                args as { targetUserId: string; addCapSet: number; removeCapSet: number },
            );
            const failFrom = w.__permissions_fail_from;
            if (failFrom > 0 && w.__permission_calls.length >= failFrom) {
                return Promise.reject({ kind: "Other", message: "not allowed right now" });
            }
            return Promise.resolve(null);
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
    }, hostSummary());
    await page.waitForSelector(".room-page__permissions-btn", { timeout: 5_000 });
    await page.locator(".room-page__permissions-btn").click();
    await expect(page.locator(".permissions-modal__panel")).toBeVisible();
}

async function calls(page: Page) {
    return await page.evaluate(
        () =>
            (
                window as unknown as {
                    __permission_calls: Array<{ targetUserId: string; addCapSet: number; removeCapSet: number }>;
                }
            ).__permission_calls,
    );
}

function pickPreset(page: Page, userId: string, preset: "editor" | "co-host") {
    return page.locator(`input[name="preset-${userId}"][value="${preset}"]`).check();
}

test("Apply sends only the rows that changed and closes the dialog", async ({ page }) => {
    await openAsHost(page);
    await pickPreset(page, V1_ID, "editor");
    await page.locator(".permissions-modal__apply").click();

    await expect(page.locator(".permissions-modal__panel")).toHaveCount(0);
    const sent = await calls(page);
    expect(sent).toHaveLength(1);
    expect(sent[0]?.targetUserId).toBe(V1_ID);
});

test("a failed Apply stays open with the error and a retry can finish the job", async ({ page }) => {
    await openAsHost(page);
    // The second of the two calls fails.
    await page.evaluate(() => {
        (window as unknown as { __permissions_fail_from: number }).__permissions_fail_from = 2;
    });
    await pickPreset(page, V1_ID, "editor");
    await pickPreset(page, V2_ID, "co-host");
    await page.locator(".permissions-modal__apply").click();

    await expect(page.locator('[data-testid="permissions-error"]')).toContainText("not allowed right now");
    await expect(page.locator(".permissions-modal__panel")).toBeVisible();
    await expect(page.locator(".permissions-modal__apply")).toBeEnabled();
    expect(await calls(page)).toHaveLength(2);

    // The failure clears, the host retries: both rows are sent again (each
    // call replaces the participant's caps, so this is safe) and it closes.
    await page.evaluate(() => {
        (window as unknown as { __permissions_fail_from: number }).__permissions_fail_from = 0;
    });
    await page.locator(".permissions-modal__apply").click();
    await expect(page.locator(".permissions-modal__panel")).toHaveCount(0);
    const sent = await calls(page);
    expect(sent.slice(2).map((c) => c.targetUserId)).toEqual([V1_ID, V2_ID]);
});
