import { test, expect, injectLocastShim, nth } from "./fixtures/vite-app";
import type { Page } from "@playwright/test";

const ROOM_ID = "r-p6t06";
const HOST_ID = "aaaa0000-0000-0000-0000-000000000001";

async function navigate(page: Page) {
    await injectLocastShim(page);
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
