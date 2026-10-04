// P5-T04 network transport (roadmap): the laser pointer is
// shared with the room. These specs drive the production React
// wiring (`useLaserTransport` inside `LaserPointer`) through the
// Vite harness:
//
//  - inbound `laser://move` / `laser://off` events (emitted
//    through the Tauri shim, as the Rust room client would)
//    render, fade and expire remote trails, and are filtered by
//    room and by the local user's own id;
//  - outbound: the local laser produces `laser_send` invokes at
//    <= 60 Hz and a LASER_OFF on pointerup;
//  - 16 remote senders render within the 16 ms frame budget;
//  - the production `localUserId` comes from
//    `summary.you_user_id` (no `setLocalUserId` test seam).

import { test, expect, injectLocastShim } from "./fixtures/vite-app";
import type { Page } from "@playwright/test";

const HOST_ID = "11111111-1111-4111-8111-111111111111";
const VIEWER_ID = "22222222-2222-4222-8222-222222222222";
const ROOM_ID = "0190a000-0000-7000-8000-00000000a504";
const OTHER_ROOM_ID = "0190a000-0000-7000-8000-00000000b504";
const ALL_CAPS = 0xfff;

function room(youUserId: string, youCapSet: number) {
    return {
        id: ROOM_ID,
        code: "P504TX",
        title: "P5-T04 transport",
        host_user_id: HOST_ID,
        host_migration_enabled: true,
        created_ms: 1_700_000_000_000,
        participants: [
            {
                user_id: HOST_ID,
                display_name: "host",
                joined_ms: 1_700_000_000_000,
                status: "Connected" as const,
                last_seen_ms: 1_700_000_000_000,
                is_host: true,
            },
            {
                user_id: VIEWER_ID,
                display_name: "viewer",
                joined_ms: 1_700_000_000_000,
                status: "Connected" as const,
                last_seen_ms: 1_700_000_000_000,
                is_host: false,
            },
        ],
        host_disconnected: false,
        host_disconnect_deadline_ms: null,
        you_cap_set: youCapSet,
        you_user_id: youUserId,
    };
}

type Trail = {
    userId: string;
    color: string;
    opacity: number;
    fadingOut: boolean;
    remote?: boolean;
    points: { x: number; y: number }[];
};

async function spaNavigate(page: Page, path: string): Promise<void> {
    await page.evaluate((to) => {
        window.history.pushState({}, "", to);
        window.dispatchEvent(new PopStateEvent("popstate"));
    }, path);
}

/** Mount the room page as `youUserId` WITHOUT the
 *  `setLocalUserId` test seam: the production path reads
 *  `summary.you_user_id`. */
async function setupRoom(page: Page, youUserId: string, youCapSet: number): Promise<void> {
    await spaNavigate(page, `/rooms/${ROOM_ID}`);
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
        if (!w.__locastRoomStore) throw new Error("room store shim not present on window");
        w.__locastRoomStore.setSummary(s);
    }, room(youUserId, youCapSet));
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
        if (!w.__locastStore) throw new Error("playback store shim not present on window");
        w.__locastStore.setMediaSrc("/test/asset.mp4");
        w.__locastStore.setMediaReady(true);
    });
    await page.waitForFunction(
        () => (window as { __locastLaser?: unknown }).__locastLaser !== undefined,
        undefined,
        { timeout: 5_000 },
    );
}

async function trails(page: Page): Promise<Trail[]> {
    return await page.evaluate(() => {
        const w = window as unknown as {
            __locastLaser?: { getState: () => { trails: Trail[] } };
        };
        return w.__locastLaser?.getState().trails ?? [];
    });
}

async function trailOf(page: Page, userId: string): Promise<Trail | undefined> {
    return (await trails(page)).find((t) => t.userId === userId);
}

async function pressLaserKey(page: Page): Promise<void> {
    await page.evaluate(() => {
        window.dispatchEvent(new KeyboardEvent("keydown", { key: "l", bubbles: true }));
    });
    await page.waitForFunction(
        () =>
            (window as unknown as { __locastKeyboardScope?: { laserActive: boolean } })
                .__locastKeyboardScope?.laserActive === true,
        undefined,
        { timeout: 5_000 },
    );
}

async function laserSends(
    page: Page,
): Promise<Array<{ action: string; x?: number; y?: number; t: number }>> {
    return await page.evaluate(() => {
        const w = window as unknown as {
            __locast_invoke_log?: Array<{
                name: string;
                args: { input?: { action: string; x?: number; y?: number } };
                t?: number;
            }>;
        };
        return (w.__locast_invoke_log ?? [])
            .filter((e) => e.name === "laser_send")
            .map((e) => ({ ...(e.args.input ?? { action: "?" }), t: e.t ?? 0 }));
    });
}

test.describe("as the host (production localUserId from you_user_id)", () => {
    test.beforeEach(async ({ page, locast }) => {
        await injectLocastShim(page);
        await page.goto("http://127.0.0.1:1420/");
        await page.waitForLoadState("domcontentloaded");
        await setupRoom(page, HOST_ID, ALL_CAPS);
        await locast.waitForLaserBridge();
    });

    test("the local user id comes from summary.you_user_id", async ({ page }) => {
        const id = await page.evaluate(
            () =>
                (window as unknown as { __locastLaser?: { localUserId: string } })
                    .__locastLaser?.localUserId,
        );
        expect(id).toBe(HOST_ID);

        await pressLaserKey(page);
        await page.mouse.move(400, 300);
        await expect
            .poll(async () => (await trailOf(page, HOST_ID))?.color, { timeout: 2_000 })
            .toBe("#ff0000");
    });

    test("remote moves from two senders render two separate trails", async ({ page, locast }) => {
        const a = "aaaaaaaa-0000-4000-8000-000000000001";
        const b = "bbbbbbbb-0000-4000-8000-000000000002";
        await locast.emitLaserMove({ room_id: ROOM_ID, sender_id: a, x: 0.1, y: 0.2 });
        await locast.emitLaserMove({ room_id: ROOM_ID, sender_id: b, x: 0.8, y: 0.9 });
        await locast.emitLaserMove({ room_id: ROOM_ID, sender_id: a, x: 0.15, y: 0.25 });

        const ta = await trailOf(page, a);
        const tb = await trailOf(page, b);
        expect(ta?.points).toEqual([
            expect.objectContaining({ x: 0.1, y: 0.2 }),
            expect.objectContaining({ x: 0.15, y: 0.25 }),
        ]);
        expect(tb?.points).toEqual([expect.objectContaining({ x: 0.8, y: 0.9 })]);
        expect(ta?.remote).toBe(true);
        expect(ta?.color).not.toBe("#ff0000");
        expect(tb?.color).not.toBe("#ff0000");
        expect(ta?.color).not.toBe(tb?.color);
    });

    test("events for another room, the local user, or with bad data are ignored", async ({
        page,
        locast,
    }) => {
        const a = "aaaaaaaa-0000-4000-8000-000000000001";
        await locast.emitLaserMove({ room_id: OTHER_ROOM_ID, sender_id: a, x: 0.5, y: 0.5 });
        await locast.emitLaserMove({ room_id: ROOM_ID, sender_id: HOST_ID, x: 0.5, y: 0.5 });
        await locast.emitLaserMove({ room_id: ROOM_ID, sender_id: "", x: 0.5, y: 0.5 });
        await locast.emitLaserMove({ room_id: ROOM_ID, sender_id: a, x: 1.5, y: 0.5 });
        await page.waitForTimeout(50);
        expect(await trails(page)).toEqual([]);

        // A valid one afterwards still renders (the listener is alive).
        await locast.emitLaserMove({ room_id: ROOM_ID, sender_id: a, x: 0.5, y: 0.5 });
        expect((await trailOf(page, a))?.points.length).toBe(1);

        // An off for another room leaves the trail alone.
        await locast.emitLaserOff({ room_id: OTHER_ROOM_ID, sender_id: a });
        await page.waitForTimeout(50);
        expect((await trailOf(page, a))?.fadingOut).toBe(false);
    });

    test("laser://off fades the remote trail out over ~200 ms", async ({ page, locast }) => {
        const a = "aaaaaaaa-0000-4000-8000-000000000001";
        await locast.emitLaserMove({ room_id: ROOM_ID, sender_id: a, x: 0.4, y: 0.4 });
        expect((await trailOf(page, a))?.opacity).toBe(1);
        await locast.emitLaserOff({ room_id: ROOM_ID, sender_id: a });
        await page.waitForTimeout(60);
        const during = await trailOf(page, a);
        expect(during?.fadingOut).toBe(true);
        expect(during?.opacity).toBeGreaterThan(0);
        expect(during?.opacity).toBeLessThan(1);
        await page.waitForTimeout(250);
        expect(await trailOf(page, a)).toBeUndefined();
    });

    test("a remote trail with no updates expires after ~3 s", async ({ page, locast }) => {
        const a = "aaaaaaaa-0000-4000-8000-000000000001";
        const sentAt = await page.evaluate(() => Date.now());
        await locast.emitLaserMove({ room_id: ROOM_ID, sender_id: a, x: 0.4, y: 0.4 });
        await page.waitForTimeout(2_500);
        const before = await trailOf(page, a);
        expect(before?.fadingOut, "still live before 3 s").toBe(false);
        expect(before?.opacity).toBe(1);
        await expect
            .poll(async () => await trailOf(page, a), { timeout: 2_000, intervals: [50] })
            .toBeUndefined();
        const goneAt = await page.evaluate(() => Date.now());
        expect(goneAt - sentAt).toBeGreaterThanOrEqual(3_000);

        // A keepalive-style update resets the idle clock.
        await locast.emitLaserMove({ room_id: ROOM_ID, sender_id: a, x: 0.4, y: 0.4 });
        await page.waitForTimeout(2_000);
        await locast.emitLaserMove({ room_id: ROOM_ID, sender_id: a, x: 0.4, y: 0.4 });
        await page.waitForTimeout(2_000);
        expect((await trailOf(page, a))?.fadingOut, "refreshed by the update").toBe(false);
    });

    test("moving the local laser sends laser_send moves at <= 60 Hz and an off on pointerup", async ({
        page,
        locast,
    }) => {
        await pressLaserKey(page);
        await locast.resetInvokeLog();
        // A fast pointer: synthetic pointermove events every few ms
        // for one second (real mouse input from Playwright is paced
        // to the frame rate, which would not exercise the gate).
        await page.mouse.move(300, 250);
        const i = await page.evaluate(async () => {
            const start = performance.now();
            let n = 0;
            while (performance.now() - start < 1_000) {
                window.dispatchEvent(
                    new PointerEvent("pointermove", {
                        clientX: 300 + (n % 200),
                        clientY: 250 + (n % 100),
                        bubbles: true,
                    }),
                );
                n++;
                await new Promise<void>((r) => setTimeout(r, 1));
            }
            return n;
        });
        // Let the last coalesced position flush.
        await page.waitForTimeout(60);
        const moves = (await laserSends(page)).filter((s) => s.action === "move");
        console.log(`laser rate: ${i} pointermove events -> ${moves.length} laser_send moves`);
        expect(i, "the pointer moved well above 60 Hz").toBeGreaterThan(120);
        expect(moves.length, "some moves were sent").toBeGreaterThan(10);
        // ~1.06 s of sending at most, 60 Hz ceiling.
        const span = moves[moves.length - 1]!.t - moves[0]!.t;
        expect(moves.length).toBeLessThanOrEqual(Math.floor(span / (1000 / 60)) + 2);
        let minGap = Infinity;
        for (let k = 1; k < moves.length; k++) {
            minGap = Math.min(minGap, moves[k]!.t - moves[k - 1]!.t);
        }
        expect(minGap, "no two sends closer than ~1/60 s").toBeGreaterThanOrEqual(15);
        for (const m of moves) {
            expect(m.x).toBeGreaterThanOrEqual(0);
            expect(m.x).toBeLessThanOrEqual(1);
            expect(m.y).toBeGreaterThanOrEqual(0);
            expect(m.y).toBeLessThanOrEqual(1);
        }
        // The last sent position is the local trail's head (same
        // normalized coordinates).
        const head = (await trailOf(page, HOST_ID))?.points.at(-1);
        expect(moves[moves.length - 1]!.x).toBeCloseTo(head!.x, 6);
        expect(moves[moves.length - 1]!.y).toBeCloseTo(head!.y, 6);

        await page.evaluate(() => {
            window.dispatchEvent(new PointerEvent("pointerup", { bubbles: true }));
        });
        await expect
            .poll(async () => (await laserSends(page)).filter((s) => s.action === "off").length)
            .toBe(1);
        const all = await laserSends(page);
        expect(all[all.length - 1]!.action, "off is the last send").toBe("off");
    });

    test("16 remote senders through laser://move render within the 16 ms frame budget", async ({
        page,
    }) => {
        // Settle, then measure from a clean slate.
        await page.waitForTimeout(50);
        const result = await page.evaluate(async (roomId) => {
            const w = window as unknown as {
                __tauriShim?: { __emit: (event: string, payload: unknown) => number };
                __locastLaser?: {
                    getState: () => { trails: { userId: string; points: unknown[] }[] };
                    getRenderStats: () => { count: number; maxMs: number; maxTrails: number };
                    resetRenderStats: () => void;
                    setIntrinsicSize: (width: number, height: number) => void;
                };
            };
            const shim = w.__tauriShim!;
            const laser = w.__locastLaser!;
            // A 1080p backing store, as for real media.
            laser.setIntrinsicSize(1920, 1080);
            const frame = (j: number): number => {
                const t0 = performance.now();
                for (let i = 0; i < 16; i++) {
                    shim.__emit("laser://move", {
                        room_id: roomId,
                        sender_id: `${(i + 1).toString(16).padStart(8, "0")}-0000-4000-8000-000000000000`,
                        x: (i * 0.05 + j * 0.005) % 1,
                        y: (j * 0.01) % 1,
                    });
                }
                return performance.now() - t0;
            };
            const nextFrame = (): Promise<void> =>
                new Promise<void>((r) => requestAnimationFrame(() => r()));
            // Warm up (JIT, canvas allocation), then measure.
            for (let j = 0; j < 5; j++) {
                frame(j);
                await nextFrame();
            }
            laser.resetRenderStats();
            let maxBatchMs = 0;
            // 20 frames, each with one move from each of 16 senders.
            for (let j = 5; j < 25; j++) {
                maxBatchMs = Math.max(maxBatchMs, frame(j));
                await nextFrame();
            }
            await new Promise<void>((r) => requestAnimationFrame(() => r()));
            await new Promise<void>((r) => requestAnimationFrame(() => r()));
            const st = laser.getState();
            return {
                maxBatchMs,
                render: laser.getRenderStats(),
                trailCount: st.trails.length,
                pointCounts: st.trails.map((t) => t.points.length),
            };
        }, ROOM_ID);

        console.log(
            `16 lasers: max event batch ${result.maxBatchMs.toFixed(2)} ms, ` +
                `max frame render ${result.render.maxMs.toFixed(2)} ms over ${result.render.count} frames`,
        );
        expect(result.trailCount).toBe(16);
        for (const n of result.pointCounts) expect(n).toBe(20);
        expect(result.render.maxTrails).toBe(16);
        expect(result.render.count).toBeGreaterThan(0);
        expect(result.maxBatchMs, "handling 16 events fits in a frame").toBeLessThan(16);
        expect(result.render.maxMs, "drawing 16 trails fits in a frame").toBeLessThan(16);
    });
});

test.describe("as a viewer", () => {
    test("a viewer without LASER sends nothing; with LASER it sends", async ({ page, locast }) => {
        await injectLocastShim(page);
        await page.goto("http://127.0.0.1:1420/");
        await page.waitForLoadState("domcontentloaded");
        // CHAT only (the default viewer cap set).
        await setupRoom(page, VIEWER_ID, 0x80);
        await locast.waitForLaserBridge();

        const id = await page.evaluate(
            () =>
                (window as unknown as { __locastLaser?: { localUserId: string } })
                    .__locastLaser?.localUserId,
        );
        expect(id, "viewers get their real id too").toBe(VIEWER_ID);
        // Not the host: no host-only UI.
        await expect(page.locator('[data-testid="viewer-positions"]')).toHaveCount(0);

        await pressLaserKey(page);
        await locast.resetInvokeLog();
        for (let i = 0; i < 20; i++) await page.mouse.move(300 + i * 5, 300);
        await page.waitForTimeout(100);
        // The local trail still renders.
        expect((await trailOf(page, VIEWER_ID))?.color).toBe("#ff0000");
        expect(await laserSends(page), "no LASER cap: nothing sent").toEqual([]);

        // The host grants LASER: the next summary carries it.
        await page.evaluate((s) => {
            const w = window as unknown as {
                __locastRoomStore?: { setSummary: (s: unknown) => void };
            };
            w.__locastRoomStore!.setSummary(s);
        }, room(VIEWER_ID, 0x80 | 0x04));
        await expect
            .poll(async () => {
                await page.mouse.move(350 + Math.random() * 50, 320);
                return (await laserSends(page)).length;
            }, { timeout: 3_000 })
            .toBeGreaterThan(0);

        // Own-id remote events are still ignored for a viewer.
        await locast.emitLaserMove({ room_id: ROOM_ID, sender_id: HOST_ID, x: 0.3, y: 0.3 });
        expect((await trailOf(page, HOST_ID))?.points.length).toBe(1);
        const before = (await trailOf(page, VIEWER_ID))?.points.length ?? 0;
        await locast.emitLaserMove({ room_id: ROOM_ID, sender_id: VIEWER_ID, x: 0.99, y: 0.99 });
        const after = await trailOf(page, VIEWER_ID);
        expect(after?.points.length).toBe(before);
        expect(after?.points.some((p) => p.x === 0.99 && p.y === 0.99)).toBe(false);
    });
});
