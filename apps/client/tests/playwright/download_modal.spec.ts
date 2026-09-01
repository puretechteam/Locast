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
