// P4-T04 acceptance: the DriftIndicator is hidden by default
// and only becomes visible when the smoothed local-vs-host
// offset exceeds 2.0 s (architecture §25.3.2 / roadmap
// "drift indicator visible only when smoothed offset > 2 s").
//
// The Vite-only harness cannot load arbitrary media into
// `<video>` (Chromium requires a real, valid source for
// `currentTime` to be settable before metadata loads). We
// therefore drive the indicator's visibility via the test
// seam exposed by `useDriftSmoother`
// (`window.__locastDrift.setSmoothed(v)`) which sets the
// EMA's smoothed value directly. The seam is gated on
// `MODE === "test"` so it is not present in production
// builds.
//
// The math itself (threshold strict `>`, EMA seeding, sign
// convention, room median, stale exclusion) is covered by
// `apps/client/src/drift/drift.smoke.ts` (run via
// `pnpm -C apps/client smoke:drift`). This Playwright spec
// only asserts the UI: the indicator's `data-testid`
// appears / disappears at the right threshold.

import { test, expect, injectLocastShim } from "./fixtures/vite-app";
import type { Page } from "@playwright/test";

const ROOM = {
    id: "r-p4t04-room",
    code: "EFGH34",
    title: "P4-T04",
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

async function mountRoomWithPlayer(page: Page): Promise<void> {
    await spaNavigate(page, `/rooms/${ROOM.id}`);
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
    }, ROOM);
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
    // Wait for the drift hook's test seam to mount.
    await page.waitForFunction(
        () => (window as { __locastDrift?: unknown }).__locastDrift !== undefined,
        undefined,
        { timeout: 5_000 },
    );
}

type DriftSeam = {
    getSample: () => {
        smoothedDriftMs: number | null;
        direction: "ahead" | "behind" | "none";
        indicatorVisible: boolean;
    };
    setSmoothed: (v: number | null) => void;
};

async function readDrift(page: Page): Promise<DriftSeam["getSample"] extends () => infer R ? R : never> {
    return await page.evaluate(() => {
        const w = window as unknown as { __locastDrift?: DriftSeam };
        if (!w.__locastDrift) {
            throw new Error("drift seam not present on window");
        }
        return w.__locastDrift.getSample();
    });
}

async function setSmoothed(page: Page, v: number | null): Promise<void> {
    await page.evaluate((val) => {
        const w = window as unknown as { __locastDrift?: DriftSeam };
        if (!w.__locastDrift) {
            throw new Error("drift seam not present on window");
        }
        w.__locastDrift.setSmoothed(val);
    }, v);
}

test("drift indicator is hidden when smoothed offset is below the 2.0 s threshold", async ({
    page,
}) => {
    await mountRoomWithPlayer(page);
    // 1.5 s AHEAD of the host (positive drift = local
    // AHEAD, per architecture §12.4 sign convention):
    // smoothed offset is 1500 ms, which is below the
    // 2.0 s threshold. The indicator must NOT be visible.
    await setSmoothed(page, 1500);
    const before = await readDrift(page);
    expect(before.smoothedDriftMs).toBe(1500);
    expect(before.indicatorVisible).toBe(false);
    expect(before.direction).toBe("ahead");
    const indicator = page.locator('[data-testid="drift-indicator"]');
    await expect(indicator).toHaveCount(0);
});

test("drift indicator becomes visible when smoothed offset crosses 2.0 s (behind)", async ({
    page,
}) => {
    await mountRoomWithPlayer(page);
    // 2.5 s behind the host: smoothed offset exceeds the
    // 2.0 s threshold. The indicator must be visible AND
    // must report "behind" (the local is behind the host).
    await setSmoothed(page, -2500);
    const sample = await readDrift(page);
    expect(sample.smoothedDriftMs).toBe(-2500);
    expect(sample.indicatorVisible).toBe(true);
    expect(sample.direction).toBe("behind");
    const indicator = page.locator('[data-testid="drift-indicator"]');
    await expect(indicator).toHaveCount(1);
    await expect(indicator).toHaveAttribute("data-direction", "behind");
});

test("drift indicator becomes visible when smoothed offset crosses 2.0 s (ahead)", async ({
    page,
}) => {
    await mountRoomWithPlayer(page);
    // 3.0 s AHEAD of the host. The indicator must still
    // appear (visibility is gated on absolute magnitude,
    // not direction) AND must report "ahead".
    await setSmoothed(page, 3000);
    const sample = await readDrift(page);
    expect(sample.smoothedDriftMs).toBe(3000);
    expect(sample.indicatorVisible).toBe(true);
    expect(sample.direction).toBe("ahead");
    const indicator = page.locator('[data-testid="drift-indicator"]');
    await expect(indicator).toHaveCount(1);
    await expect(indicator).toHaveAttribute("data-direction", "ahead");
});

test("drift indicator is hidden again when smoothed offset returns under the threshold", async ({
    page,
}) => {
    await mountRoomWithPlayer(page);
    // Cross the threshold, then return below it. The
    // indicator must track the smoothed value
    // deterministically.
    await setSmoothed(page, 3000);
    await expect(page.locator('[data-testid="drift-indicator"]')).toHaveCount(1);
    await setSmoothed(page, 500);
    await expect(page.locator('[data-testid="drift-indicator"]')).toHaveCount(0);
    const sample = await readDrift(page);
    expect(sample.smoothedDriftMs).toBe(500);
    expect(sample.indicatorVisible).toBe(false);
});

test("exactly at the 2.0 s threshold: indicator is NOT visible (strict >)", async ({
    page,
}) => {
    await mountRoomWithPlayer(page);
    // Architecture §25.3.2 uses "exceeds 2.0 seconds" and
    // the roadmap's P4-T04 goal uses "> 2 s". Strictly
    // greater than 2000 must hide the indicator.
    await setSmoothed(page, 2000);
    const sample = await readDrift(page);
    expect(sample.indicatorVisible).toBe(false);
    await expect(page.locator('[data-testid="drift-indicator"]')).toHaveCount(0);
});

test("drift indicator does not appear when the local user is the host (no remote reports)", async ({
    page,
}) => {
    // In this test the local user is the host (per the
    // room summary). The smoother's hook reads
    // `usePlaybackStore.lastApplied` for the host's
    // command. With no host command emitted, the smoother
    // produces no raw drift and the EMA is null. The
    // indicator must be hidden.
    await mountRoomWithPlayer(page);
    // The smoother state is initialized with
    // smoothedDriftMs = null. The indicator must NOT be
    // visible regardless of which player role is active.
    const sample = await readDrift(page);
    expect(sample.smoothedDriftMs).toBeNull();
    expect(sample.indicatorVisible).toBe(false);
    await expect(page.locator('[data-testid="drift-indicator"]')).toHaveCount(0);
});
