import { test, expect, injectLocastShim, nth } from "./fixtures/vite-app";
import type { Page } from "@playwright/test";

const ROOM_ID = "r-p6t06";
const HOST_ID = "aaaa0000-0000-0000-0000-000000000001";

// `failures` makes the named commands reject with the `{ kind, message }` object
// Rust's `AppError` arrives as, and every call to a command is counted in
// `window.__calls`. The wrapper is registered after `injectLocastShim`, which
// installs the invoke it wraps.
async function navigate(page: Page, failures: Record<string, string> = {}) {
    await injectLocastShim(page);
    await page.addInitScript((fail: Record<string, string>) => {
        const w = window as unknown as {
            __TAURI_INTERNALS__: {
                invoke: (name: string, args?: unknown, options?: unknown) => Promise<unknown>;
            };
            __calls: Record<string, number>;
        };
        w.__calls = {};
        const original = w.__TAURI_INTERNALS__.invoke;
        w.__TAURI_INTERNALS__.invoke = (name, args, options) => {
            w.__calls[name] = (w.__calls[name] ?? 0) + 1;
            const message = fail[name];
            return message === undefined
                ? original(name, args, options)
                : Promise.reject({ kind: "Other", message });
        };
    }, failures);
    await page.goto("/");
    await page.waitForLoadState("domcontentloaded");
    await page.evaluate((to) => {
        window.history.pushState({}, "", to);
        window.dispatchEvent(new PopStateEvent("popstate"));
    }, `/rooms/${ROOM_ID}`);
    await page.waitForSelector('[data-testid="room-empty"]', { timeout: 5_000 });
    await page.waitForFunction(() => (window as { __locastRoomStore?: unknown }).__locastRoomStore !== undefined, undefined, { timeout: 5_000 });
    await page.evaluate((s) => { (window as { __locastRoomStore?: { setSummary: (s: unknown) => void } }).__locastRoomStore!.setSummary(s); }, {
        id: ROOM_ID, code: "P6T06", title: "P6-T06", host_user_id: HOST_ID, host_migration_enabled: true, created_ms: 1_700_000_000_000,
        participants: [{ user_id: HOST_ID, display_name: "host", joined_ms: 1_700_000_000_000, status: "Connected" as const, last_seen_ms: 1_700_000_000_000, is_host: true }],
        host_disconnected: false, host_disconnect_deadline_ms: null,
    });
    await page.waitForSelector('[data-testid="locast-player"]', { timeout: 5_000 });
}

async function callsTo(page: Page, command: string): Promise<number> {
    return await page.evaluate(
        (name) => (window as unknown as { __calls: Record<string, number> }).__calls[name] ?? 0,
        command,
    );
}

// get_temp_files in the shim returns whatever the test seeded for the room.
async function seedTempFiles(page: Page, ids: string[]) {
    await page.evaluate(({ roomId, hostId, fileIds }) => {
        const w = window as unknown as { __locast_tempFiles: Record<string, unknown[]> };
        w.__locast_tempFiles[roomId] = fileIds.map((id, i) => ({
            file_id: id,
            room_id: roomId,
            filename: `${id}.mp4`,
            size_bytes: 1024 * (i + 1),
            created_ms: 1_700_000_000_000 + i,
            owner_user_id: hostId,
        }));
    }, { roomId: ROOM_ID, hostId: HOST_ID, fileIds: ids });
}

test("Delete: 3 temp files listed, delete_files_to_trash IPC logged", async ({ page, locast }) => {
    await navigate(page);
    await seedTempFiles(page, ["dl-1", "dl-2", "dl-3"]);
    await page.locator(".room-footer__leave").click();
    await expect(page.locator(".lrm-panel")).toBeVisible();
    await expect(page.locator(".lrm-item")).toHaveCount(3);
    await locast.resetInvokeLog();
    await page.locator(".lrm-btn--delete").click();
    await expect
        .poll(async () => (await locast.readInvokeLog()).filter((e) => e.name === "delete_files_to_trash").length)
        .toBe(1);
    const log = await locast.readInvokeLog();
    const deletes = log.filter((e) => e.name === "delete_files_to_trash");
    expect(deletes).toHaveLength(1);
    expect((nth(deletes, 0).args as { fileIds: string[] }).fileIds).toEqual(["dl-1", "dl-2", "dl-3"]);
});

test("Keep: 3 temp files listed, mark_files_permanent IPC logged", async ({ page, locast }) => {
    await navigate(page);
    await seedTempFiles(page, ["dl-k1", "dl-k2", "dl-k3"]);
    await page.locator(".room-footer__leave").click();
    await expect(page.locator(".lrm-panel")).toBeVisible();
    await expect(page.locator(".lrm-item")).toHaveCount(3);
    await locast.resetInvokeLog();
    await page.locator(".lrm-btn--keep").click();
    await expect
        .poll(async () => (await locast.readInvokeLog()).filter((e) => e.name === "mark_files_permanent").length)
        .toBe(1);
    const log = await locast.readInvokeLog();
    const keeps = log.filter((e) => e.name === "mark_files_permanent");
    expect(keeps).toHaveLength(1);
    expect((nth(keeps, 0).args as { fileIds: string[] }).fileIds).toEqual(["dl-k1", "dl-k2", "dl-k3"]);
});

// Leaving used to fail silently from this dialog: `onConfirm` was not awaited,
// so a rejected leave set an error in the footer behind the full-screen
// backdrop, the dialog looked idle again, and the user could loop forever.
test("a failed leave is reported in the dialog, which stays open for a retry", async ({ page }) => {
    await navigate(page, { room_leave: "signaling is down" });
    await seedTempFiles(page, ["dl-1"]);
    await page.locator(".room-footer__leave").click();
    await page.locator(".lrm-btn--keep").click();

    await expect(page.locator('[data-testid="lrm-error"]')).toContainText("signaling is down");
    await expect(page.locator(".lrm-panel")).toBeVisible();
    await expect(page.locator(".lrm-btn--keep")).toBeEnabled();
    await expect(page.locator(".lrm-btn--delete")).toBeEnabled();
    await expect(page).toHaveURL(new RegExp(`/rooms/${ROOM_ID}$`));
    expect(await callsTo(page, "room_leave")).toBe(1);
});

test("a failed keep is reported and the room is not left", async ({ page }) => {
    await navigate(page, { mark_files_permanent: "disk error" });
    await seedTempFiles(page, ["dl-1"]);
    await page.locator(".room-footer__leave").click();
    await page.locator(".lrm-btn--keep").click();

    await expect(page.locator('[data-testid="lrm-error"]')).toContainText("disk error");
    await expect(page.locator(".lrm-panel")).toBeVisible();
    expect(await callsTo(page, "room_leave")).toBe(0);
});

test("a failed file listing is reported, not shown as 'no files', and leaving still works", async ({ page }) => {
    await navigate(page, { get_temp_files: "database is locked" });
    await page.locator(".room-footer__leave").click();

    await expect(page.locator('[data-testid="lrm-load-error"]')).toContainText("database is locked");
    await expect(page.locator(".lrm-panel")).not.toContainText("No temporary files in this room");
    // Deleting an unknown set of files is not offered; leaving is.
    await expect(page.locator(".lrm-btn--delete")).toBeDisabled();
    await page.locator(".lrm-btn--keep").click();
    await expect(page).toHaveURL(/\/rooms$/);
    expect(await callsTo(page, "room_leave")).toBe(1);
});

test("Escape closes the dialog without leaving", async ({ page }) => {
    await navigate(page);
    await page.locator(".room-footer__leave").click();
    await expect(page.locator(".lrm-panel")).toBeVisible();
    await page.keyboard.press("Escape");
    await expect(page.locator(".lrm-panel")).toHaveCount(0);
    expect(await callsTo(page, "room_leave")).toBe(0);
});
