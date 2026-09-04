// P4-T05 acceptance: a Playwright test with the user
// lacking playback capability clicks "Sync to Host";
// the user's `currentTime` jumps but no SEEK command is
// emitted on the WS; a test with the user having the
// capability emits the SEEK and the room rebroadcasts a
// presence event. (roadmap P4-T05 line 354)
//
// This spec drives the two branches of the manual-sync
// hook through the existing Vite harness. The harness
// cannot load arbitrary media into `<video>` (Chromium
// requires a real source for `currentTime` to be
// settable before metadata loads). The tests therefore
// drive the hook through its public test seam
// (`window.__locastDrift`) and assert:
//  - the button's `data-can-sync` / `disabled` state
//    tracks `canSync`,
//  - clicking the button as a VIEWER bumps the
//    `localSeekTick` counter (the Vite harness cannot
//    observe the actual `<video>.currentTime` write)
//    AND does NOT record a `playback_send` invoke,
//  - clicking the button as the HOST also bumps the
//    counter AND records exactly one `playback_send`
//    invoke with `action: "seek"` and a `monotonic_seq`
//    that matches the shared counter.
//  - the DriftIndicator's Resync button uses the same
//    hook.
//
// The "presence event" acceptance line is not directly
// asserted: the existing playback event path already
// rebroadcasts via `playback://state` (P4-T02), and a
// dedicated presence-event shape is a follow-on task
// (see the final report's "deviations" section).

import { test, expect, injectLocastShim } from "./fixtures/vite-app";
import type { Page } from "@playwright/test";

const HOST_ROOM = {
    id: "r-p4t05-host",
    code: "HOST1",
    title: "P4-T05 host",
    host_user_id: "11111111-1111-1111-1111-111111111111",
    host_migration_enabled: true,
    created_ms: 1_700_000_000_000,
    participants: [
        {
            user_id: "11111111-1111-1111-1111-111111111111",
            display_name: "host",
            joined_ms: 1_700_000_000_000,
            status: "Connected" as const,
            last_seen_ms: 1_700_000_000_000,
            is_host: true,
        },
    ],
    host_disconnected: false,
    host_disconnect_deadline_ms: null,
};

const VIEWER_ROOM = {
    id: "r-p4t05-viewer",
    code: "VIEW22",
    title: "P4-T05 viewer",
    host_user_id: "33333333-3333-3333-3333-333333333333",
    host_migration_enabled: true,
    created_ms: 1_700_000_000_000,
    participants: [
        {
            user_id: "33333333-3333-3333-3333-333333333333",
            display_name: "remote-host",
            joined_ms: 1_700_000_000_000,
            status: "Connected" as const,
            last_seen_ms: 1_700_000_000_000,
            is_host: true,
        },
        {
            user_id: "22222222-2222-2222-2222-222222222222",
            display_name: "viewer",
            joined_ms: 1_700_000_000_500,
            status: "Connected" as const,
            last_seen_ms: 1_700_000_000_500,
            is_host: false,
        },
    ],
    host_disconnected: false,
    host_disconnect_deadline_ms: null,
};

const HOST_T0_MS = 1_700_000_000_000;
const HOST_TARGET_MS = 30_000;

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
    await locast.resetInvokeLog();
    // Reset every per-test counter / override so the
    // seam is in a deterministic state.
    await page.evaluate(() => {
        const w = window as unknown as {
            __locastDrift?: {
                resetLocalSeekTick?: () => void;
                resetForcedHostCommand?: () => void;
            };
        };
        w.__locastDrift?.resetLocalSeekTick?.();
        w.__locastDrift?.resetForcedHostCommand?.();
    });
});

async function mountRoom(
    page: Page,
    roomSummary: typeof HOST_ROOM,
    localUserId: string,
): Promise<void> {
    await spaNavigate(page, `/rooms/${roomSummary.id}`);
    await page.waitForSelector('[data-testid="room-empty"]', { timeout: 5_000 });
    await page.waitForFunction(
        () => (window as { __locastRoomStore?: unknown }).__locastRoomStore !== undefined,
        undefined,
        { timeout: 5_000 },
    );
    await page.evaluate(
        ({ s, lid }) => {
            const w = window as unknown as {
                __locastRoomStore?: {
                    setSummary: (s: unknown) => void;
                    setLocalUserId: (id: string | null) => void;
                };
            };
            if (!w.__locastRoomStore) {
                throw new Error("room store shim not present on window");
            }
            w.__locastRoomStore.setLocalUserId(lid);
            w.__locastRoomStore.setSummary(s);
        },
        { s: roomSummary, lid: localUserId },
    );
    await page.waitForSelector('[data-testid="room-empty"]', {
        state: "detached",
        timeout: 5_000,
    });
    await page.waitForSelector('[data-testid="locast-player"]', { timeout: 5_000 });
    await page.waitForFunction(
        () => (window as { __locastStore?: unknown }).__locastStore !== undefined,
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
        if (!w.__locastStore) {
            throw new Error("playback store shim not present on window");
        }
        w.__locastStore.setMediaSrc("/test/asset.mp4");
        w.__locastStore.setMediaReady(true);
    });
    await page.waitForSelector('[data-testid="sync-button"]', { timeout: 5_000 });
    await page.waitForFunction(
        () => (window as { __locastDrift?: unknown }).__locastDrift !== undefined,
        undefined,
        { timeout: 5_000 },
    );
}

async function forceHostCommand(
    page: Page,
    payload: {
        room_id: string;
        media_position_ms: number;
        server_ts_ms: number;
    },
): Promise<void> {
    await page.evaluate((p) => {
        const w = window as unknown as {
            __locastDrift?: {
                forceHostCommandForTest: (p: typeof p) => void;
            };
        };
        if (!w.__locastDrift) {
            throw new Error("drift seam not present on window");
        }
        w.__locastDrift.forceHostCommandForTest(p);
    }, payload);
}

async function readLocalSeekTick(page: Page): Promise<number> {
    return await page.evaluate(() => {
        const w = window as unknown as {
            __locastDrift?: { readLocalSeekTick: () => number };
        };
        return w.__locastDrift?.readLocalSeekTick() ?? 0;
    });
}

async function resetLocalSeekTick(page: Page): Promise<void> {
    await page.evaluate(() => {
        const w = window as unknown as {
            __locastDrift?: { resetLocalSeekTick: () => void };
        };
        w.__locastDrift?.resetLocalSeekTick();
    });
}

async function readInvokeLog(
    page: Page,
): Promise<Array<{ name: string; args: unknown }>> {
    return await page.evaluate(() => {
        const w = window as unknown as {
            __locast_invoke_log?: Array<{ name: string; args: unknown }>;
        };
        return w.__locast_invoke_log ?? [];
    });
}

test("Sync button is disabled when no host command has been received", async ({
    page,
}) => {
    await mountRoom(page, HOST_ROOM, "11111111-1111-1111-1111-111111111111");
    const btn = page.locator('[data-testid="sync-button"]');
    await expect(btn).toBeDisabled();
    await expect(btn).toHaveAttribute("data-can-sync", "false");
    const log = await readInvokeLog(page);
    expect(log.find((e) => e.name === "playback_send")).toBeUndefined();
});

test("VIEWER clicking Sync locally seeks the <video> and does NOT emit a PLAYBACK_CMD", async ({
    page,
    locast,
}) => {
    await mountRoom(page, VIEWER_ROOM, "22222222-2222-2222-2222-222222222222");
    await forceHostCommand(page, {
        room_id: VIEWER_ROOM.id,
        media_position_ms: HOST_TARGET_MS,
        server_ts_ms: HOST_T0_MS,
    });
    await page.waitForTimeout(50);
    const btn = page.locator('[data-testid="sync-button"]');
    await expect(btn).toBeEnabled();
    await expect(btn).toHaveAttribute("data-can-sync", "true");
    await resetLocalSeekTick(page);
    await locast.resetInvokeLog();
    await btn.click();
    await page.waitForTimeout(50);
    const seekTick = await readLocalSeekTick(page);
    expect(seekTick).toBe(1);
    const log = await readInvokeLog(page);
    expect(log.find((e) => e.name === "playback_send")).toBeUndefined();
});

test("HOST clicking Sync locally seeks AND emits exactly one PLAYBACK_CMD with action='seek'", async ({
    page,
    locast,
}) => {
    await mountRoom(page, HOST_ROOM, "11111111-1111-1111-1111-111111111111");
    await forceHostCommand(page, {
        room_id: HOST_ROOM.id,
        media_position_ms: HOST_TARGET_MS,
        server_ts_ms: HOST_T0_MS,
    });
    await page.waitForTimeout(50);
    const btn = page.locator('[data-testid="sync-button"]');
    await expect(btn).toBeEnabled();
    await resetLocalSeekTick(page);
    await locast.resetInvokeLog();
    await btn.click();
    await page.waitForTimeout(50);
    const seekTick = await readLocalSeekTick(page);
    expect(seekTick).toBe(1);
    const log = await readInvokeLog(page);
    const sends = log.filter((e) => e.name === "playback_send");
    expect(sends).toHaveLength(1);
    const args = sends[0].args as {
        cmd?: { action?: string; monotonic_seq?: number; media_position_ms?: number };
    } | null;
    expect(args).not.toBeNull();
    expect(args?.cmd?.action).toBe("seek");
    expect(args?.cmd?.monotonic_seq).toBe(1);
    // The target is the host's last position projected
    // forward by (now - server_ts). We assert it is
    // within a generous window of the expected target
    // (HOST_TARGET_MS + elapsed-since-HOST_T0_MS). The
    // exact value depends on `Date.now()` at the time
    // the test runs, so we do not assert byte equality.
    const expectedApprox =
        HOST_TARGET_MS + (Date.now() - HOST_T0_MS);
    const actual = args?.cmd?.media_position_ms ?? 0;
    expect(Math.abs(actual - expectedApprox)).toBeLessThan(2_000);
});

test("two HOST clicks consume monotonic_seq 1 and 2 (shared counter)", async ({
    page,
}) => {
    await mountRoom(page, HOST_ROOM, "11111111-1111-1111-1111-111111111111");
    await forceHostCommand(page, {
        room_id: HOST_ROOM.id,
        media_position_ms: HOST_TARGET_MS,
        server_ts_ms: HOST_T0_MS,
    });
    await page.waitForTimeout(50);
    const btn = page.locator('[data-testid="sync-button"]');
    await expect(btn).toBeEnabled();
    await btn.click();
    await page.waitForTimeout(50);
    await btn.click();
    await page.waitForTimeout(50);
    const log = await readInvokeLog(page);
    const sends = log.filter((e) => e.name === "playback_send");
    expect(sends).toHaveLength(2);
    const seqs = sends.map(
        (s) =>
            (s.args as { cmd: { monotonic_seq: number } }).cmd.monotonic_seq,
    );
    expect(seqs).toEqual([1, 2]);
});

test("DriftIndicator Resync button uses the same sync implementation as the Sync button", async ({
    page,
    locast,
}) => {
    await mountRoom(page, VIEWER_ROOM, "22222222-2222-2222-2222-222222222222");
    await forceHostCommand(page, {
        room_id: VIEWER_ROOM.id,
        media_position_ms: HOST_TARGET_MS,
        server_ts_ms: HOST_T0_MS,
    });
    await page.evaluate(() => {
        const w = window as unknown as {
            __locastDrift?: { setSmoothed: (v: number | null) => void };
        };
        if (!w.__locastDrift) {
            throw new Error("drift seam not present on window");
        }
        w.__locastDrift.setSmoothed(3000);
    });
    await page.waitForTimeout(50);
    const resync = page.locator('[data-testid="drift-indicator-resync"]');
    await expect(resync).toHaveCount(1);
    await resetLocalSeekTick(page);
    await locast.resetInvokeLog();
    await resync.click();
    await page.waitForTimeout(50);
    const seekTick = await readLocalSeekTick(page);
    expect(seekTick).toBe(1);
    const log = await readInvokeLog(page);
    expect(log.find((e) => e.name === "playback_send")).toBeUndefined();
});

test("Sync button is disabled when media is not ready", async ({ page }) => {
    // Mount the room WITHOUT setting mediaReady. The
    // button is rendered (the user is in a room) but
    // disabled because the canSync gate requires
    // mediaReady === true.
    await spaNavigate(page, `/rooms/${HOST_ROOM.id}`);
    await page.waitForSelector('[data-testid="room-empty"]', { timeout: 5_000 });
    await page.waitForFunction(
        () => (window as { __locastRoomStore?: unknown }).__locastRoomStore !== undefined,
        undefined,
        { timeout: 5_000 },
    );
    await page.evaluate((s) => {
        const w = window as unknown as {
            __locastRoomStore?: { setSummary: (s: unknown) => void };
        };
        if (!w.__locastRoomStore) {
            throw new Error("room store shim not present on window");
        }
        w.__locastRoomStore.setSummary(s);
    }, HOST_ROOM);
    await page.waitForSelector('[data-testid="locast-player"]', { timeout: 5_000 });
    await page.waitForSelector('[data-testid="sync-button"]', { timeout: 5_000 });
    const btn = page.locator('[data-testid="sync-button"]');
    await expect(btn).toBeDisabled();
    await expect(btn).toHaveAttribute("data-can-sync", "false");
});
