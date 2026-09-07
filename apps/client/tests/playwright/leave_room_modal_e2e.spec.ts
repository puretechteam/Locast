import { test, expect, injectLocastShim } from "./fixtures/vite-app";
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
    await page.waitFunction(() => (window as { __locastRoomStore?: unknown }).__locastRoomStore !== undefined, undefined, { timeout: 5_000 });
    await page.evaluate((s) => { (window as { __locastRoomStore?: { setSummary: (s: unknown) => void } }).__locastRoomStore!.setSummary(s); }, {
        id: ROOM_ID, code: "P6T06", title: "P6-T06", host_user_id: HOST_ID, host_migration_enabled: true, created_ms: 1_700_000_000_000,
        participants: [{ user_id: HOST_ID, display_name: "host", joined_ms: 1_700_000_000_000, status: "Connected" as const, last_seen_ms: 1_700_000_000_000, is_host: true }],
        host_disconnected: false, host_disconnect_deadline_ms: null,
    });
    await page.waitForSelector('[data-testid="locast-player"]', { timeout: 5_000 });
}

async function addTempFiles(page: Page, ids: string[]) {
    for (const id of ids) {
        await page.evaluate(({ dlId }) => import("/tests/playwright/shim/tauriShim.ts").then((mod) => {
            mod.__emit("download://state", { v: 1, id: dlId, media_id: dlId, state: "transferring", error_message: null });
            mod.__emit("download://progress", { v: 1, id: dlId, state: "transferring", transferred_bytes: 1024, total_bytes: 2048, bytes_per_sec_ema: 1024, eta_seconds: 1 });
        }), { dlId: id });
    }
}

test.beforeEach(async ({ page, locast }) => { await locast.waitForBridge(); });

test("Delete: 3 temp files listed, delete_files_to_trash IPC logged", async ({ page, locast }) => {
    await navigate(page);
    await addTempFiles(page, ["dl-1", "dl-2", "dl-3"]);
    await page.locator(".room-footer__leave").click();
    await expect(page.locator(".lrm-panel")).toBeVisible();
    await expect(page.locator(".lrm-item")).toHaveCount(3);
    await locast.resetInvokeLog();
    await page.locator(".lrm-btn--delete").click();
    await page.waitForTimeout(300);
    const log = await locast.readInvokeLog();
    const deletes = log.filter((e) => e.name === "delete_files_to_trash");
    expect(deletes).toHaveLength(1);
    expect((deletes[0].args as { fileIds: string[] }).fileIds).toEqual(["dl-1", "dl-2", "dl-3"]);
});

test("Keep: 3 temp files listed, mark_files_permanent IPC logged", async ({ page, locast }) => {
    await navigate(page);
    await addTempFiles(page, ["dl-k1", "dl-k2", "dl-k3"]);
    await page.locator(".room-footer__leave").click();
    await expect(page.locator(".lrm-panel")).toBeVisible();
    await expect(page.locator(".lrm-item")).toHaveCount(3);
    await locast.resetInvokeLog();
    await page.locator(".lrm-btn--keep").click();
    await page.waitForTimeout(300);
    const log = await locast.readInvokeLog();
    const keeps = log.filter((e) => e.name === "mark_files_permanent");
    expect(keeps).toHaveLength(1);
    expect((keeps[0].args as { fileIds: string[] }).fileIds).toEqual(["dl-k1", "dl-k2", "dl-k3"]);
});
