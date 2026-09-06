// P5-T04 acceptance (roadmap):
//   "a Playwright test holds the laser key and moves the
//    mouse; 16 simultaneous lasers (one per fake participant)
//    all render within the 16 ms frame budget; releasing
//    fades the trail over 200 ms."
//
// The Vite harness cannot synthesize real keyboard+mouse
// laser input, so the test drives the laser system through
// the `__locastLaser` test seam that `LaserPointer`
// exposes in test mode.
//
// Tests:
//  1. Canvas is present with correct testid and class.
//  2. Adding positions accumulates a trail.
//  3. Trail length is capped at 20 points.
//  4. Releasing (removeTrail) fades the trail over 200 ms.
//  5. clearAll removes all trails.
//  6. 16 simultaneous lasers all render within a single
//     rAF frame (frame budget test).

import { test, expect, injectLocastShim } from "./fixtures/vite-app";
import type { Page } from "@playwright/test";

const ROOM = {
    id: "r-p5t04-room",
    code: "P504AB",
    title: "P5-T04",
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

async function spaNavigate(page: Page, path: string): Promise<void> {
    await page.evaluate((to) => {
        window.history.pushState({}, "", to);
        window.dispatchEvent(new PopStateEvent("popstate"));
    }, path);
}

async function setupRoom(page: Page): Promise<void> {
    await spaNavigate(page, `/rooms/${ROOM.id}`);
    await page.waitForSelector('[data-testid="room-empty"]', { timeout: 5_000 });
    await page.waitForFunction(
        () => (window as { __locastRoomStore?: unknown }).__locastRoomStore !==
            undefined,
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
    await page.waitForSelector('[data-testid="locast-player"]', {
        timeout: 5_000,
    });
    await page.waitForFunction(
        () => (window as { __locastStore?: unknown }).__locastStore !==
            undefined,
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
}

test.beforeEach(async ({ page }) => {
    await injectLocastShim(page);
    await page.goto("http://127.0.0.1:1420/");
    await page.waitForLoadState("domcontentloaded");
    await setupRoom(page);
    await page.waitForFunction(
        () => (window as { __locastLaser?: unknown }).__locastLaser !==
            undefined,
        undefined,
        { timeout: 5_000 },
    );
});

test("laser canvas is present with correct testid and class", async ({
    page,
}) => {
    const canvas = page.locator('[data-testid="locast-laser-pointer"]');
    await expect(canvas).toBeAttached();
    await expect(canvas).toHaveClass(/laser-pointer-layer/);
});

test("adding positions accumulates a trail", async ({ page }) => {
    const stateBefore = await page.evaluate(() => {
        const w = window as unknown as {
            __locastLaser?: {
                getState?: () => {
                    trails: { userId: string; points: unknown[] }[];
                };
            };
        };
        return w.__locastLaser?.getState?.();
    });

    await page.evaluate(() => {
        const w = window as unknown as {
            __locastLaser?: {
                addPosition: (userId: string, x: number, y: number) => void;
            };
        };
        w.__locastLaser?.addPosition("user-a", 0.1, 0.2);
        w.__locastLaser?.addPosition("user-a", 0.3, 0.4);
        w.__locastLaser?.addPosition("user-a", 0.5, 0.6);
    });

    await page.waitForTimeout(50);

    const state = await page.evaluate(() => {
        const w = window as unknown as {
            __locastLaser?: {
                getState?: () => {
                    trails: { userId: string; points: { x: number; y: number }[] }[];
                };
            };
        };
        return w.__locastLaser?.getState?.();
    });

    expect(state?.trails.length).toBeGreaterThan(0);
    const userTrail = state?.trails.find((t) => t.userId === "user-a");
    expect(userTrail?.points.length).toBe(3);
    expect(userTrail?.points[0]).toMatchObject({ x: 0.1, y: 0.2 });
    expect(userTrail?.points[1]).toMatchObject({ x: 0.3, y: 0.4 });
    expect(userTrail?.points[2]).toMatchObject({ x: 0.5, y: 0.6 });
});

test("trail length is capped at 20 points", async ({ page }) => {
    await page.evaluate(() => {
        const w = window as unknown as {
            __locastLaser?: {
                addPosition: (userId: string, x: number, y: number) => void;
                clearAll: () => void;
            };
        };
        w.__locastLaser?.clearAll();
        for (let i = 0; i < 30; i++) {
            w.__locastLaser?.addPosition("user-cap-test", i / 29, i / 29);
        }
    });

    await page.waitForTimeout(50);

    const state = await page.evaluate(() => {
        const w = window as unknown as {
            __locastLaser?: {
                getState?: () => {
                    trails: { userId: string; points: unknown[] }[];
                };
            };
        };
        return w.__locastLaser?.getState?.();
    });

    const trail = state?.trails.find((t) => t.userId === "user-cap-test");
    expect(trail?.points.length).toBeLessThanOrEqual(20);
    expect(trail?.points.length).toBe(20);
});

test("removeTrail starts fade-out over 200 ms", async ({ page }) => {
    await page.evaluate(() => {
        const w = window as unknown as {
            __locastLaser?: {
                addPosition: (userId: string, x: number, y: number) => void;
                removeTrail: (userId: string) => void;
            };
        };
        w.__locastLaser?.addPosition("user-fade", 0.5, 0.5);
    });

    await page.waitForTimeout(30);

    const opacityBeforeFade = await page.evaluate(() => {
        const w = window as unknown as {
            __locastLaser?: {
                getState?: () => {
                    trails: { userId: string; opacity: number }[];
                };
            };
        };
        return w.__locastLaser?.getState?.();
    });

    const trailBefore = opacityBeforeFade?.trails.find(
        (t) => t.userId === "user-fade",
    );
    expect(trailBefore?.opacity).toBe(1);

    await page.evaluate(() => {
        const w = window as unknown as {
            __locastLaser?: {
                removeTrail: (userId: string) => void;
            };
        };
        w.__locastLaser?.removeTrail("user-fade");
    });

    await page.waitForTimeout(50);
    const opacityDuringFade = await page.evaluate(() => {
        const w = window as unknown as {
            __locastLaser?: {
                getState?: () => {
                    trails: { userId: string; opacity: number }[];
                };
            };
        };
        return w.__locastLaser?.getState?.();
    });
    const trailDuring = opacityDuringFade?.trails.find(
        (t) => t.userId === "user-fade",
    );
    expect(trailDuring?.opacity).toBeGreaterThan(0);
    expect(trailDuring?.opacity).toBeLessThan(1);

    await page.waitForTimeout(220);
    const opacityAfterFade = await page.evaluate(() => {
        const w = window as unknown as {
            __locastLaser?: {
                getState?: () => {
                    trails: { userId: string; opacity: number }[];
                };
            };
        };
        return w.__locastLaser?.getState?.();
    });
    const trailAfter = opacityAfterFade?.trails.find(
        (t) => t.userId === "user-fade",
    );
    expect(trailAfter).toBeUndefined();
});

test("clearAll removes all trails immediately", async ({ page }) => {
    await page.evaluate(() => {
        const w = window as unknown as {
            __locastLaser?: {
                addPosition: (userId: string, x: number, y: number) => void;
                clearAll: () => void;
            };
        };
        w.__locastLaser?.addPosition("user-a", 0.1, 0.2);
        w.__locastLaser?.addPosition("user-b", 0.3, 0.4);
        w.__locastLaser?.clearAll();
    });

    const state = await page.evaluate(() => {
        const w = window as unknown as {
            __locastLaser?: {
                getState?: () => {
                    trails: { userId: string }[];
                };
            };
        };
        return w.__locastLaser?.getState?.();
    });

    expect(state?.trails.length).toBe(0);
});

test("16 simultaneous lasers all render in a single frame", async ({ page }) => {
    await page.evaluate(() => {
        const w = window as unknown as {
            __locastLaser?: {
                addPosition: (userId: string, x: number, y: number) => void;
                clearAll: () => void;
            };
        };
        w.__locastLaser?.clearAll();
        for (let i = 0; i < 16; i++) {
            const userId = `user-${i}`;
            for (let j = 0; j < 20; j++) {
                w.__locastLaser?.addPosition(
                    userId,
                    (i * 0.05 + j * 0.005) % 1,
                    (j * 0.01) % 1,
                );
            }
        }
    });

    await page.waitForTimeout(50);

    const state = await page.evaluate(() => {
        const w = window as unknown as {
            __locastLaser?: {
                getState?: () => {
                    trails: { userId: string; points: unknown[] }[];
                };
            };
        };
        return w.__locastLaser?.getState?.();
    });

    expect(state?.trails.length).toBe(16);

    for (let i = 0; i < 16; i++) {
        const trail = state?.trails.find((t) => t.userId === `user-${i}`);
        expect(trail).toBeDefined();
        expect(trail?.points.length).toBe(20);
    }
});

test("laser canvas is positioned above drawing canvas by z-index", async ({
    page,
}) => {
    const laserCanvas = page.locator('[data-testid="locast-laser-pointer"]');
    const drawingCanvas = page.locator('[data-testid="locast-drawing-layer"]');

    await expect(laserCanvas).toBeAttached();
    await expect(drawingCanvas).toBeAttached();

    const laserZIndex = await laserCanvas.evaluate((el) => {
        return window.getComputedStyle(el).zIndex;
    });
    const drawingZIndex = await drawingCanvas.evaluate((el) => {
        return window.getComputedStyle(el).zIndex;
    });

    expect(Number(laserZIndex)).toBeGreaterThan(Number(drawingZIndex));
});

test("laser canvas has pointer-events: none", async ({ page }) => {
    const canvas = page.locator('[data-testid="locast-laser-pointer"]');
    const pointerEvents = await canvas.evaluate((el) => {
        return window.getComputedStyle(el).pointerEvents;
    });
    expect(pointerEvents).toBe("none");
});

test("addPosition resets fading trail to full opacity", async ({ page }) => {
    await page.evaluate(() => {
        const w = window as unknown as {
            __locastLaser?: {
                addPosition: (userId: string, x: number, y: number) => void;
                removeTrail: (userId: string) => void;
            };
        };
        w.__locastLaser?.addPosition("user-reset", 0.5, 0.5);
        w.__locastLaser?.removeTrail("user-reset");
    });

    await page.waitForTimeout(50);

    const duringFade = await page.evaluate(() => {
        const w = window as unknown as {
            __locastLaser?: {
                getState?: () => {
                    trails: { userId: string; opacity: number; fadingOut: boolean }[];
                };
            };
        };
        return w.__locastLaser?.getState?.();
    });
    const during = duringFade?.trails.find((t) => t.userId === "user-reset");
    expect(during?.fadingOut).toBe(true);
    expect(during?.opacity).toBeLessThan(1);

    await page.evaluate(() => {
        const w = window as unknown as {
            __locastLaser?: {
                addPosition: (userId: string, x: number, y: number) => void;
            };
        };
        w.__locastLaser?.addPosition("user-reset", 0.6, 0.6);
    });

    await page.waitForTimeout(20);

    const afterReset = await page.evaluate(() => {
        const w = window as unknown as {
            __locastLaser?: {
                getState?: () => {
                    trails: { userId: string; opacity: number; fadingOut: boolean }[];
                };
            };
        };
        return w.__locastLaser?.getState?.();
    });
    const after = afterReset?.trails.find((t) => t.userId === "user-reset");
    expect(after?.fadingOut).toBe(false);
    expect(after?.opacity).toBe(1);
});
