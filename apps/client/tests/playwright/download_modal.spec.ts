// NOTE: The roadmap P3-T10 acceptance bullet "imports a file via
// manifest" requires the full Tauri IPC pipeline (manifest fetch +
// download session start). The Vite-only harness cannot exercise
// that pipeline without a Tauri runtime. This spec covers the
// equivalent UI behaviour by injecting synthetic download events
// at the same Tauri-event boundary the real Rust backend would
// use. A future Tauri-driver / WebDriver spec will cover the full
// manifest-import pathway.

import { test, expect, injectLocastShim } from "./fixtures/vite-app";
import type { Page } from "@playwright/test";

const DLG = '[data-testid="dlm-dialog"]';
const ROOM_EMPTY = '[data-testid="room-empty"]';

async function spaNavigate(page: Page, path: string): Promise<void> {
    await page.evaluate((to) => {
        window.history.pushState({}, "", to);
        window.dispatchEvent(new PopStateEvent("popstate"));
    }, path);
}

test.beforeEach(async ({ page, locast }) => {
    await injectLocastShim(page);
    await page.goto("/");
    await page.waitForLoadState("domcontentloaded");
    await locast.waitForBridge();
});

test("modal renders when a download is active", async ({ page, locast }) => {
    await locast.emitDownloadState({
        id: "d1",
        media_id: "aabbccdd-1111-2222-3333-444455556666",
        state: "transferring",
    });
    await expect(page.locator(DLG)).toBeVisible();
});

test("Escape does not dismiss the modal", async ({ page, locast }) => {
    await locast.emitDownloadState({
        id: "d1",
        media_id: "aabbccdd-1111-2222-3333-444455556666",
        state: "transferring",
    });
    await expect(page.locator(DLG)).toBeVisible();
    await page.locator('[data-testid="dlm-dialog"]').focus();
    await page.keyboard.press("Escape");
    await expect(page.locator(DLG)).toBeVisible();
});

test("backdrop click does not dismiss the modal", async ({ page, locast }) => {
    await locast.emitDownloadState({
        id: "d1",
        media_id: "aabbccdd-1111-2222-3333-444455556666",
        state: "transferring",
    });
    await expect(page.locator(DLG)).toBeVisible();
    await page.locator('[data-testid="dlm-backdrop"]').click({ position: { x: 5, y: 5 } });
    await expect(page.locator(DLG)).toBeVisible();
});

test("/rooms/:id is blocked while a download is active", async ({ page, locast }) => {
    await locast.emitDownloadState({
        id: "d1",
        media_id: "aabbccdd-1111-2222-3333-444455556666",
        state: "transferring",
    });
    await expect(page.locator(DLG)).toBeVisible();
    await spaNavigate(page, "/rooms/abc");
    await expect(page.locator(ROOM_EMPTY)).toHaveCount(0);
    await expect(page.locator(DLG)).toBeVisible();
});

test("complete closes the modal and unblocks /rooms/:id", async ({ page, locast }) => {
    await locast.emitDownloadState({
        id: "d1",
        media_id: "aabbccdd-1111-2222-3333-444455556666",
        state: "transferring",
    });
    await locast.emitDownloadProgress({
        id: "d1",
        state: "transferring",
        transferred_bytes: 1024,
        total_bytes: 2048,
        bytes_per_sec_ema: 1024,
        eta_seconds: 1,
    });
    await expect(page.locator(DLG)).toBeVisible();
    await locast.emitDownloadState({
        id: "d1",
        media_id: "aabbccdd-1111-2222-3333-444455556666",
        state: "complete",
    });
    await expect(page.locator(DLG)).toHaveCount(0);
    await spaNavigate(page, "/rooms/abc");
    await expect(page.locator(ROOM_EMPTY)).toBeVisible();
});

test("failed does NOT auto-close", async ({ page, locast }) => {
    await locast.emitDownloadState({
        id: "d1",
        media_id: "aabbccdd-1111-2222-3333-444455556666",
        state: "transferring",
    });
    await locast.emitDownloadState({
        id: "d1",
        media_id: "aabbccdd-1111-2222-3333-444455556666",
        state: "failed",
        error_message: "disk full",
    });
    await expect(page.locator(DLG)).toBeVisible();
    await expect(page.locator('[data-testid="dlm-error"]')).toContainText("disk full");
});

// A failed download used to leave the full-screen modal up with no control, and
// the blocking guard hid every route, so the only way out was restarting the app.
test("a failed download can be dismissed, which unblocks the app", async ({ page, locast }) => {
    await locast.emitDownloadState({
        id: "d1",
        media_id: "aabbccdd-1111-2222-3333-444455556666",
        state: "failed",
        error_message: "host left the room",
    });
    await expect(page.locator(DLG)).toBeVisible();
    await spaNavigate(page, "/rooms/abc");
    await expect(page.locator(ROOM_EMPTY)).toHaveCount(0);

    await page.locator('[data-testid="dlm-dismiss"]').click();

    await expect(page.locator(DLG)).toHaveCount(0);
    await expect(page.locator(ROOM_EMPTY)).toBeVisible();
});

// With no source connected yet, `download_open` leaves the row pending and the
// shared-media bridge retries forever. If the host never connects (a
// restrictive NAT, a host that went away) the full-screen modal had no control
// at all, so the only way out was restarting the app.

// Wraps the shim's invoke (the stub is added after `injectLocastShim` in
// `beforeEach`, so it runs after the shim on every load): counts `room_leave`
// calls and can make them fail with the `{ kind, message }` object Rust's
// `AppError` arrives as.
async function stubRoomLeave(page: Page, failWith: string | null): Promise<void> {
    await page.addInitScript((failure: string | null) => {
        const w = window as unknown as {
            __TAURI_INTERNALS__: {
                invoke: (name: string, args?: unknown, options?: unknown) => Promise<unknown>;
            };
            __room_leave_calls: number;
        };
        w.__room_leave_calls = 0;
        const original = w.__TAURI_INTERNALS__.invoke;
        w.__TAURI_INTERNALS__.invoke = (name, args, options) => {
            if (name !== "room_leave") return original(name, args, options);
            w.__room_leave_calls += 1;
            return failure === null
                ? Promise.resolve(null)
                : Promise.reject({ kind: "Other", message: failure });
        };
    }, failWith);
    await page.reload();
}

async function roomLeaveCalls(page: Page): Promise<number> {
    return await page.evaluate(
        () => (window as unknown as { __room_leave_calls: number }).__room_leave_calls,
    );
}

test("a download that has not started offers a way out by leaving the room", async ({ page, locast }) => {
    await stubRoomLeave(page, null);
    await locast.waitForBridge();
    await locast.emitDownloadState({
        id: "d1",
        media_id: "aabbccdd-1111-2222-3333-444455556666",
        state: "pending",
    });
    await expect(page.locator(DLG)).toBeVisible();
    await expect(page.locator('[data-testid="dlm-waiting"]')).toBeVisible();

    await page.locator('[data-testid="dlm-leave"]').click();

    await expect(page.locator(DLG)).toHaveCount(0);
    expect(await roomLeaveCalls(page)).toBe(1);
});

test("a failed leave is reported and the modal stays", async ({ page, locast }) => {
    await stubRoomLeave(page, "signaling is down");
    await locast.waitForBridge();
    await locast.emitDownloadState({
        id: "d1",
        media_id: "aabbccdd-1111-2222-3333-444455556666",
        state: "pending",
    });
    await page.locator('[data-testid="dlm-leave"]').click();

    await expect(page.locator('[data-testid="dlm-leave-error"]')).toContainText("signaling is down");
    await expect(page.locator(DLG)).toBeVisible();
    await expect(page.locator('[data-testid="dlm-leave"]')).toBeEnabled();
    expect(await roomLeaveCalls(page)).toBe(1);
});

test("a transferring download has no leave button", async ({ page, locast }) => {
    await locast.emitDownloadState({
        id: "d1",
        media_id: "aabbccdd-1111-2222-3333-444455556666",
        state: "transferring",
    });
    await expect(page.locator(DLG)).toBeVisible();
    await expect(page.locator('[data-testid="dlm-leave"]')).toHaveCount(0);
});

test("an in-progress download cannot be dismissed", async ({ page, locast }) => {
    await locast.emitDownloadState({
        id: "d1",
        media_id: "aabbccdd-1111-2222-3333-444455556666",
        state: "transferring",
    });
    await expect(page.locator(DLG)).toBeVisible();
    await expect(page.locator('[data-testid="dlm-dismiss"]')).toHaveCount(0);
});
