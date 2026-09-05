// P4-T06 acceptance: when the clock-skew jitter is high
// (architecture section 13.3: > 200 ms), the drift indicator
// and severe-band thresholds widen (2 s -> 3 s, 5 s -> 7 s).
// The Vite harness cannot measure real jitter; the tests
// drive the `__locastClockSkew` seam (which is the same seam
// the `useClockSkew` hook writes to) to exercise the
// threshold-widening path deterministically. The pure-math
// smoke test (`drift.smoke.ts`) covers the full threshold
// table; this file asserts the UI responds to a live change.

import { test, expect, injectLocastShim } from "./fixtures/vite-app";
import type { Page } from "@playwright/test";

const HOST_ROOM = {
    id: "r-p4t06-drift",
    code: "DRIFT1",
    title: "P4-T06 drift threshold",
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

const HOST_T0_MS = 1_700_000_000_000;

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
});

async function mountRoom(page: Page): Promise<void> {
    await spaNavigate(page, `/rooms/${HOST_ROOM.id}`);
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
        { s: HOST_ROOM, lid: HOST_ROOM.host_user_id },
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
    await page.waitForFunction(
        () => (window as { __locastDrift?: unknown }).__locastDrift !== undefined,
        undefined,
        { timeout: 5_000 },
    );
    await page.waitForFunction(
        () =>
            typeof (window as { __locastDrift?: { forceHostCommandForTest?: unknown } })
                .__locastDrift?.forceHostCommandForTest === "function",
        undefined,
        { timeout: 5_000 },
    );
    await page.evaluate(
        ({ roomId, media_position_ms, server_ts_ms }) => {
            const w = window as unknown as {
                __locastDrift?: {
                    forceHostCommandForTest: (p: {
                        room_id: string;
                        media_position_ms: number;
                        server_ts_ms: number;
                    }) => void;
                };
            };
            if (!w.__locastDrift) {
                throw new Error("drift seam not present on window");
            }
            w.__locastDrift.forceHostCommandForTest({
                room_id: roomId,
                media_position_ms,
                server_ts_ms,
            });
        },
        {
            roomId: HOST_ROOM.id,
            media_position_ms: 0,
            server_ts_ms: HOST_T0_MS,
        },
    );
    await page.waitForTimeout(50);
}

async function setSmoothed(page: Page, v: number | null): Promise<void> {
    await page.evaluate((val) => {
        const w = window as unknown as {
            __locastDrift?: { setSmoothed: (v: number | null) => void };
        };
        if (!w.__locastDrift) {
            throw new Error("drift seam not present on window");
        }
        w.__locastDrift.setSmoothed(val);
    }, v);
}

async function setClockSkew(
    page: Page,
    skew: number | null,
    jitter: number | null,
): Promise<void> {
    await page.evaluate(
        ({ s, j }) => {
            const w = window as unknown as {
                __locastClockSkew?: {
                    setSkewJitter: (skewMs: number | null, jitterMs: number | null) => void;
                };
            };
            if (!w.__locastClockSkew) {
                throw new Error("clock skew seam not present on window");
            }
            w.__locastClockSkew.setSkewJitter(s, j);
        },
        { s: skew, j: jitter },
    );
}

test("drift indicator visible at 2.5s with low jitter; hidden at 2.5s with high jitter", async ({
    page,
}) => {
    await mountRoom(page);
    // Set the smoother to 2.5s drift. With low jitter (50ms)
    // the threshold is 2s, so the indicator must be visible.
    await setSmoothed(page, 2500);
    await setClockSkew(page, 0, 50);
    await page.waitForTimeout(50);
    await expect(page.locator('[data-testid="drift-indicator"]')).toHaveCount(1);
    // Now widen the threshold by setting jitter to 250 (>200).
    // The indicator must disappear because the threshold
    // widens to 3s, and 2.5s is below the widened threshold.
    await setClockSkew(page, 0, 250);
    await page.waitForTimeout(50);
    await expect(page.locator('[data-testid="drift-indicator"]')).toHaveCount(0);
    // Drive drift above the widened threshold (3.5s) and
    // assert the indicator reappears.
    await setSmoothed(page, 3500);
    await page.waitForTimeout(50);
    await expect(page.locator('[data-testid="drift-indicator"]')).toHaveCount(1);
});
