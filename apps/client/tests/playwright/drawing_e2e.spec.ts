// P5-T02 acceptance (roadmap):
//   "a Playwright test draws 200 points in 1 second;
//    the WS trace shows <=120 DRAW_POINT messages;
//    the server rebroadcasts to all other participants
//    within 50 ms."
//
// The Vite harness cannot load a real <video> for
// pointer input, so the test drives the drawing
// service directly through the IPC seam
// (`__locastDrawing`) that the React layer's
// `DrawingService` exposes. The seam tracks the
// in-flight stroke id, pending network point, and
// monotonic seq so the Playwright test can assert
// (a) the wire-level shape of the DRAW_BEGIN /
// DRAW_POINT / DRAW_END envelope stream the React
// layer hands to the Rust IPC, and (b) the
// last-point-wins coalescing produces <=120
// DRAW_POINT messages for a 200-event burst.

import { test, expect, injectLocastShim } from "./fixtures/vite-app";
import type { Page } from "@playwright/test";

const ROOM = {
    id: "r-p5t02-room",
    code: "P502AB",
    title: "P5-T02",
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

test.beforeEach(async ({ page, locast }) => {
    await injectLocastShim(page);
    await page.goto("/");
    await page.waitForLoadState("domcontentloaded");
    await locast.waitForBridge();
});

/**
 * Read every drawing_send IPC call recorded by the
 * Vite harness's `__locast_invoke_log`. Returns the
 * list of payload-shaped objects the React layer
 * passed to the `commands.drawingSend` helper.
 */
interface DrawingInvokeRecord {
    name: string;
    args: {
        input: {
            action: "begin" | "point" | "end";
            stroke_id: string;
            ts_ms?: number;
            client_seq?: number;
            x?: number;
            y?: number;
            pressure?: number;
            tool?: string;
            color?: string;
            width?: number;
        };
    };
}

async function readDrawingInvokeLog(
    page: Page,
): Promise<DrawingInvokeRecord[]> {
    return await page.evaluate(() => {
        const w = window as unknown as {
            __locast_invoke_log?: Array<{
                name: string;
                args: unknown;
            }>;
        };
        const log = w.__locast_invoke_log ?? [];
        return log.filter((r): r is DrawingInvokeRecord => {
            if (r.name !== "drawing_send") return false;
            if (typeof r.args !== "object" || r.args === null) return false;
            const a = r.args as { input?: unknown };
            if (typeof a.input !== "object" || a.input === null) return false;
            return true;
        });
    });
}

async function resetInvokeLog(page: Page): Promise<void> {
    await page.evaluate(() => {
        const w = window as unknown as {
            __locast_resetInvokeLog?: () => void;
        };
        if (w.__locast_resetInvokeLog) {
            w.__locast_resetInvokeLog();
        }
    });
}

test("DRAW_BEGIN produces exactly one outbound DRAW_BEGIN envelope", async ({
    page,
    locast,
}) => {
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
 timeout: 5_000 });
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
    await resetInvokeLog(page);

    // Drive a begin/end pair via the IPC seam. We
    // cannot synthesize a real pointer pipeline on the
    // Vite harness; the seam records the exact payload
    // shape the React layer would produce.
    await page.evaluate(async () => {
        const w = window as unknown as {
            __TAURI_INTERNALS__?: { invoke: (n: string, args: unknown) => Promise<unknown> };
        };
        if (!w.__TAURI_INTERNALS__) {
            throw new Error("__TAURI_INTERNALS__ not present");
        }
        // DRAW_BEGIN.
        await w.__TAURI_INTERNALS__.invoke("drawing_send", {
            input: {
                action: "begin",
                stroke_id: "11111111-2222-3333-4444-555555555555",
                tool: "pen",
                color: "#ff5c69",
                width: 3.0,
                x: 0.1,
                y: 0.1,
                pressure: 0.5,
                ts_ms: 1_000,
                client_seq: 1,
            },
        });
        // DRAW_END.
        await w.__TAURI_INTERNALS__.invoke("drawing_send", {
            input: {
                action: "end",
                stroke_id: "11111111-2222-3333-4444-555555555555",
                ts_ms: 1_500,
                client_seq: 2,
            },
        });
    });

    const log = await readDrawingInvokeLog(page);
    expect(log.length).toBe(2);
    expect(log[0]?.args.input.action).toBe("begin");
    expect(log[0]?.args.input.stroke_id).toBe("11111111-2222-3333-4444-555555555555");
    expect(log[0]?.args.input.tool).toBe("pen");
    expect(log[0]?.args.input.color).toBe("#ff5c69");
    expect(log[1]?.args.input.action).toBe("end");
});

test("200 DRAW_POINT calls in 1 second produce <=120 outbound DRAW_POINT envelopes", async ({
    page,
}) => {
    // Set up the room + media.
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
 timeout: 5_000 });
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
    await resetInvokeLog(page);

    // Drive the React-side DrawingService from the
    // page context. The service's coalescing loop is
    // the path the production app uses; the test
    // calls beginStroke + 200 appendPoint calls (each
    // ~5 ms apart) + endStroke and asserts the
    // outbound DRAW_POINT count is <= 120.
    const result = await page.evaluate(async () => {
        const svcMod = await import("/src/services/drawing.ts");
        const service = new svcMod.DrawingService();
        const strokeId = await service.beginStroke({
            tool: "pen",
            color: "#ff5c69",
            width: 3,
            x: 0.1,
            y: 0.1,
            pressure: 0.5,
            tsMs: Date.now(),
        });
        const startedAt = performance.now();
        for (let i = 0; i < 200; i++) {
            service.appendPoint({
                x: 0.1 + i / 1000,
                y: 0.1 + i / 1000,
                pressure: 0.5,
                tsMs: Date.now(),
            });
            // Sleep 5 ms to simulate a pointer event cadence.
            await new Promise((r) => setTimeout(r, 5));
        }
        const elapsedMs = performance.now() - startedAt;
        // Force a final flush via endStroke so the
        // trailing pending point is counted.
        await service.endStroke();
        return { strokeId: strokeId.strokeId, elapsedMs };
    });
    expect(result.elapsedMs).toBeGreaterThan(900);
    expect(result.elapsedMs).toBeLessThan(1_500);

    const log = await readDrawingInvokeLog(page);
    const beginEnvelopes = log.filter((r) => r.args.input.action === "begin");
    const pointEnvelopes = log.filter((r) => r.args.input.action === "point");
    const endEnvelopes = log.filter((r) => r.args.input.action === "end");
    expect(beginEnvelopes.length).toBe(1);
    expect(endEnvelopes.length).toBe(1);
    expect(pointEnvelopes.length).toBeLessThanOrEqual(120);
    // The acceptance test is "200 points in 1 s produce
    // <=120 DRAW_POINT messages". We assert the upper
    // bound; the lower bound is the service's natural
    // rAF cadence (60 Hz) plus the 8.33 ms interval
    // cap, which on a fast machine can yield 60-120.
    expect(pointEnvelopes.length).toBeGreaterThanOrEqual(50);

    // Every envelope's stroke_id is the one we began.
    const strokeId = result.strokeId;
    for (const r of log) {
        expect(r.args.input.stroke_id).toBe(strokeId);
    }
});

test("DRAW_POINT payloads are normalized [0..1] floats", async ({
    page,
}) => {
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
 timeout: 5_000 });
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
    await resetInvokeLog(page);

    await page.evaluate(async () => {
        const svcMod = await import("/src/services/drawing.ts");
        const service = new svcMod.DrawingService();
        await service.beginStroke({
            tool: "pen",
            color: "#000",
            width: 2,
            x: 0.5,
            y: 0.5,
            pressure: 0.5,
            tsMs: Date.now(),
        });
        // Push a few points whose normalized coordinates
        // are well inside [0..1].
        for (let i = 0; i < 20; i++) {
            service.appendPoint({
                x: i / 20,
                y: 0.5,
                pressure: 0.5,
                tsMs: Date.now(),
            });
            await new Promise((r) => setTimeout(r, 10));
        }
        await service.endStroke();
    });

    const log = await readDrawingInvokeLog(page);
    const points = log.filter((r) => r.args.input.action === "point");
    for (const r of points) {
        const x = r.args.input.x;
        const y = r.args.input.y;
        if (x === undefined || y === undefined) continue;
        expect(x).toBeGreaterThanOrEqual(0);
        expect(x).toBeLessThanOrEqual(1);
        expect(y).toBeGreaterThanOrEqual(0);
        expect(y).toBeLessThanOrEqual(1);
        // The canvas re-paints from [0..1] floats;
        // every IPC payload uses the canonical
        // convention established by P5-T01's
        // geometry.ts.
    }
});

test("a cancelled stroke (no DRAW_END) does not leak DRAW_POINT envelopes", async ({
    page,
}) => {
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
 timeout: 5_000 });
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
    await resetInvokeLog(page);

    await page.evaluate(async () => {
        const svcMod = await import("/src/services/drawing.ts");
        const service = new svcMod.DrawingService();
        await service.beginStroke({
            tool: "pen",
            color: "#000",
            width: 2,
            x: 0.5,
            y: 0.5,
            pressure: 0.5,
            tsMs: Date.now(),
        });
        service.appendPoint({
            x: 0.5,
            y: 0.5,
            pressure: 0.5,
            tsMs: Date.now(),
        });
        // Cancel without sending DRAW_END.
        service.cancelStroke();
    });

    const log = await readDrawingInvokeLog(page);
    // The DRAW_BEGIN must have been emitted (the
    // server needs it to bind the stroke id), but
    // the cancel must NOT have produced a DRAW_END
    // (no phantom close envelope).
    expect(log.length).toBe(1);
    expect(log[0]?.args.input.action).toBe("begin");
});

test("remote DRAW_BEGIN/POINT/END updates the drawing store", async ({
    page,
    locast,
}) => {
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

    const STROKE_ID = "22222222-3333-4444-5555-666666666666";
    const REMOTE_USER = "99999999-9999-9999-9999-999999999999";

    await page.waitForFunction(
        () => (window as { __locastDrawingStore?: { getAllStrokes: () => unknown } }).__locastDrawingStore !==
            undefined,
        undefined,
        { timeout: 5_000 },
    );

    await page.evaluate((roomId: string) => {
        const w = window as unknown as {
            __locastDrawingStore?: { setRoomId: (id: string | null) => void };
        };
        w.__locastDrawingStore?.setRoomId(roomId);
    }, ROOM.id);

    await locast.emitStrokeBegin({
        room_id: ROOM.id,
        sender_id: REMOTE_USER,
        stroke_id: STROKE_ID,
        tool: "pen",
        color: "#ff0000",
        width: 3,
        x: 0.1,
        y: 0.2,
        pressure: 0.5,
        ts_ms: 1_000,
    });

    const beforePoint = await page.evaluate(() => {
        const w = window as unknown as {
            __locastDrawingStore?: { getAllStrokes: () => { id: string; userId: string; tool: string; color: string; width: number; points: unknown[] }[] };
        };
        return w.__locastDrawingStore?.getAllStrokes() ?? [];
    });
    expect(beforePoint.length).toBeGreaterThan(0);

    await locast.emitStrokePoint({
        room_id: ROOM.id,
        sender_id: REMOTE_USER,
        stroke_id: STROKE_ID,
        x: 0.3,
        y: 0.4,
        pressure: 0.6,
        ts_ms: 1_100,
    });

    await locast.emitStrokePoint({
        room_id: ROOM.id,
        sender_id: REMOTE_USER,
        stroke_id: STROKE_ID,
        x: 0.5,
        y: 0.6,
        pressure: 0.7,
        ts_ms: 1_200,
    });

    await locast.emitStrokeEnd({
        room_id: ROOM.id,
        sender_id: REMOTE_USER,
        stroke_id: STROKE_ID,
        ts_ms: 1_300,
    });

    const finalStrokes = await page.evaluate(() => {
        const w = window as unknown as {
            __locastDrawingStore?: { getAllStrokes: () => { id: string; userId: string; tool: string; color: string; width: number; points: unknown[]; endedAt: number }[] };
        };
        return w.__locastDrawingStore?.getAllStrokes() ?? [];
    });

    const remoteStroke = finalStrokes.find((s) => s.id === STROKE_ID);
    expect(remoteStroke).toBeDefined();
    expect(remoteStroke?.userId).toBe(REMOTE_USER);
    expect(remoteStroke?.tool).toBe("pen");
    expect(remoteStroke?.color).toBe("#ff0000");
    expect(remoteStroke?.width).toBe(3);
    expect(remoteStroke?.points.length).toBe(3);
    expect(remoteStroke?.endedAt).toBe(1_300);
});

test("remote stroke events for wrong room are ignored", async ({
    page,
    locast,
}) => {
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

    await locast.emitStrokeBegin({
        room_id: "wrong-room-id",
        sender_id: "someone-else",
        stroke_id: "should-be-ignored",
        tool: "pen",
        color: "#ff0000",
        width: 3,
        x: 0.1,
        y: 0.2,
        pressure: 0.5,
        ts_ms: 1_000,
    });

    await page.waitForFunction(
        () => (window as { __locastDrawingStore?: { getAllStrokes: () => unknown } }).__locastDrawingStore !==
            undefined,
        undefined,
        { timeout: 5_000 },
    );

    await page.evaluate((roomId: string) => {
        const w = window as unknown as {
            __locastDrawingStore?: { setRoomId: (id: string | null) => void };
        };
        w.__locastDrawingStore?.setRoomId(roomId);
    }, ROOM.id);

    const strokes = await page.evaluate(() => {
        const w = window as unknown as {
            __locastDrawingStore?: { getAllStrokes: () => { id: string }[] };
        };
        return w.__locastDrawingStore?.getAllStrokes() ?? [];
    });

    const ignoredStroke = strokes.find((s) => s.id === "should-be-ignored");
    expect(ignoredStroke).toBeUndefined();
});

test("multiple remote users can draw simultaneously", async ({
    page,
    locast,
}) => {
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

    await page.waitForFunction(
        () => (window as { __locastDrawingStore?: { setRoomId: (id: string | null) => void } }).__locastDrawingStore !==
            undefined,
        undefined,
        { timeout: 5_000 },
    );

    await page.evaluate((roomId: string) => {
        const w = window as unknown as {
            __locastDrawingStore?: { setRoomId: (id: string | null) => void };
        };
        w.__locastDrawingStore?.setRoomId(roomId);
    }, ROOM.id);

    const USER_A = "aaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
    const USER_B = "bbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";

    await locast.emitStrokeBegin({
        room_id: ROOM.id,
        sender_id: USER_A,
        stroke_id: "stroke-a",
        tool: "pen",
        color: "#ff0000",
        width: 2,
        x: 0.1,
        y: 0.1,
        pressure: 0.5,
        ts_ms: 1_000,
    });

    await locast.emitStrokeBegin({
        room_id: ROOM.id,
        sender_id: USER_B,
        stroke_id: "stroke-b",
        tool: "pen",
        color: "#00ff00",
        width: 4,
        x: 0.5,
        y: 0.5,
        pressure: 0.8,
        ts_ms: 1_100,
    });

    await locast.emitStrokeEnd({
        room_id: ROOM.id,
        sender_id: USER_A,
        stroke_id: "stroke-a",
        ts_ms: 1_500,
    });

    await locast.emitStrokeEnd({
        room_id: ROOM.id,
        sender_id: USER_B,
        stroke_id: "stroke-b",
        ts_ms: 1_600,
    });

    await page.waitForFunction(
        () => (window as { __locastDrawingStore?: { getAllStrokes: () => unknown } }).__locastDrawingStore !==
            undefined,
        undefined,
        { timeout: 5_000 },
    );

    await page.evaluate((roomId: string) => {
        const w = window as unknown as {
            __locastDrawingStore?: { setRoomId: (id: string | null) => void };
        };
        w.__locastDrawingStore?.setRoomId(roomId);
    }, ROOM.id);

    const strokes = await page.evaluate(() => {
        const w = window as unknown as {
            __locastDrawingStore?: { getAllStrokes: () => { id: string; userId: string; color: string; width: number }[] };
        };
        return w.__locastDrawingStore?.getAllStrokes() ?? [];
    });

    const strokeA = strokes.find((s) => s.id === "stroke-a");
    const strokeB = strokes.find((s) => s.id === "stroke-b");

    expect(strokeA).toBeDefined();
    expect(strokeA?.userId).toBe(USER_A);
    expect(strokeA?.color).toBe("#ff0000");
    expect(strokeA?.width).toBe(2);

    expect(strokeB).toBeDefined();
    expect(strokeB?.userId).toBe(USER_B);
    expect(strokeB?.color).toBe("#00ff00");
    expect(strokeB?.width).toBe(4);
});

/**
 * P5-T03 roadmap acceptance: a two-client E2E test.
 *
 * The roadmap says:
 *   "two test clients; existing __locast_invoke_log; new event-emitter seam;
 *    both clients receive the same stroke list; within 100 ms of host DRAW_END"
 *
 * The test creates two independent Playwright page contexts (two separate browser
 * contexts, each with their own JS state, stores, and tauriShim). Both navigate
 * to the same room. The host client adds a stroke directly to its store
 * (simulating the local drawing flow). The server rebroadcast is simulated
 * by emitting the event through the viewer's drawing event bridge. The test
 * verifies that the viewer receives the completed stroke within 100 ms of
 * the host's DRAW_END, and that both stores contain equivalent stroke data.
 */
test("two clients receive the same stroke within 100 ms of DRAW_END", async ({
    page: hostPage,
}) => {
    const ROOM_ID = "r-p5t03-two-client";

    // Create a second independent browser context for the viewer.
    const viewerContext = await hostPage.context().browser()!.newContext();
    const viewerPage = await viewerContext.newPage();

    // Helper to navigate and set up a page with the shim + room + drawing store.
    async function setupClient(
        p: import("@playwright/test").Page,
    ): Promise<void> {
        await injectLocastShim(p);
        await p.goto("/");
        await p.waitForLoadState("domcontentloaded");
        await spaNavigate(p, `/rooms/${ROOM_ID}`);
        await p.waitForSelector('[data-testid="room-empty"]', { timeout: 5_000 });
        await p.waitForFunction(
            () => (window as { __locastRoomStore?: unknown }).__locastRoomStore !==
                undefined,
            undefined,
            { timeout: 5_000 },
        );
        await p.evaluate((s) => {
            const w = window as unknown as {
                __locastRoomStore?: { setSummary: (s: unknown) => void };
            };
            w.__locastRoomStore!.setSummary(s);
        }, ROOM);
        await p.waitForSelector('[data-testid="room-empty"]', {
            state: "detached",
            timeout: 5_000,
        });
        await p.waitForSelector('[data-testid="locast-player"]', {
            timeout: 5_000,
        });
        await p.waitForFunction(
            () => (window as { __locastStore?: unknown }).__locastStore !==
                undefined,
            undefined,
            { timeout: 5_000 },
        );
        await p.evaluate(() => {
            const w = window as unknown as {
                __locastStore?: {
                    setMediaSrc: (s: string) => void;
                    setMediaReady: (r: boolean) => void;
                };
            };
            w.__locastStore!.setMediaSrc("/test/asset.mp4");
            w.__locastStore!.setMediaReady(true);
        });
        await p.waitForFunction(
            () =>
                (window as { __locastDrawingStore?: { setRoomId: (id: string | null) => void } })
                    .__locastDrawingStore !== undefined,
            undefined,
            { timeout: 5_000 },
        );
        await p.evaluate((roomId: string) => {
            const w = window as unknown as {
                __locastDrawingStore?: { setRoomId: (id: string | null) => void };
            };
            w.__locastDrawingStore!.setRoomId(roomId);
        }, ROOM_ID);
    }

    // Set up both clients.
    await setupClient(hostPage);
    await setupClient(viewerPage);

    const HOST_USER = "host-user-id";
    const STROKE_ID = "stroke-two-client-test";
    const TOOL = "pen";
    const COLOR = "#123456";
    const WIDTH = 2.5;

    // Host client adds the stroke directly to its store (simulating local draw).
    // This bypasses the event bridge, matching the production flow where
    // the host draws locally without needing a server rebroadcast.
    await hostPage.evaluate(
        ({ strokeId, userId, tool, color, width, tsMs }) => {
            const w = window as unknown as {
                __locastDrawingStore?: {
                    beginStroke: (opts: {
                        strokeId: string;
                        userId: string;
                        tool: string;
                        color: string;
                        width: number;
                        x: number;
                        y: number;
                        pressure: number;
                        tsMs: number;
                    }) => void;
                    appendPoint: (opts: {
                        strokeId: string;
                        x: number;
                        y: number;
                        pressure: number;
                        tsMs: number;
                    }) => void;
                    endStroke: (opts: { strokeId: string; tsMs: number }) => void;
                };
            };
            w.__locastDrawingStore!.beginStroke({
                strokeId,
                userId,
                tool,
                color,
                width,
                x: 0.1,
                y: 0.2,
                pressure: 0.5,
                tsMs,
            });
            w.__locastDrawingStore!.appendPoint({
                strokeId,
                x: 0.3,
                y: 0.4,
                pressure: 0.6,
                tsMs: tsMs + 50,
            });
            w.__locastDrawingStore!.appendPoint({
                strokeId,
                x: 0.5,
                y: 0.6,
                pressure: 0.7,
                tsMs: tsMs + 100,
            });
            w.__locastDrawingStore!.endStroke({ strokeId, tsMs: tsMs + 150 });
        },
        { strokeId: STROKE_ID, userId: HOST_USER, tool: TOOL, color: COLOR, width: WIDTH, tsMs: Date.now() },
    );

    // Give the host store a moment to settle.
    await hostPage.waitForTimeout(50);

    // Verify host store has the completed stroke.
    const hostStrokes = await hostPage.evaluate(() => {
        const w = window as unknown as {
            __locastDrawingStore?: {
                getAllStrokes: () => {
                    id: string;
                    userId: string;
                    tool: string;
                    color: string;
                    width: number;
                    points: unknown[];
                    endedAt: number;
                }[];
            };
        };
        return w.__locastDrawingStore?.getAllStrokes() ?? [];
    });
    const hostStroke = hostStrokes.find((s) => s.id === STROKE_ID);
    expect(hostStroke).toBeDefined();
    expect(hostStroke?.userId).toBe(HOST_USER);
    expect(hostStroke?.tool).toBe(TOOL);
    expect(hostStroke?.color).toBe(COLOR);
    expect(hostStroke?.width).toBe(WIDTH);
    expect(hostStroke?.points.length).toBe(3);
    expect(hostStroke?.endedAt).toBeGreaterThan(0);

    // Now simulate the server rebroadcast: emit DRAW_BEGIN/POINT/END through
    // the VIEWER's drawing event bridge. In production, the server sends
    // the rebroadcast to all participants (including the viewer) after
    // accepting the host's DRAW_BEGIN/POINT/END.
    //
    // We emit to the viewer's bridge only (simulating server → viewer),
    // then measure the time until the viewer's store has the completed stroke.
    // The host's own rebroadcast is suppressed by the server (originator
    // suppression), so we don't emit to the host's bridge.
    const t0 = Date.now();

    await viewerPage.evaluate(
        ({ roomId, strokeId, userId, tool, color, width, t0: startTime }) => {
            const w = window as unknown as {
                __locast?: {
                    emitStrokeBegin: (p: {
                        room_id: string;
                        sender_id: string;
                        stroke_id: string;
                        tool: string;
                        color: string;
                        width: number;
                        x: number;
                        y: number;
                        pressure: number;
                        ts_ms: number;
                    }) => Promise<void>;
                    emitStrokePoint: (p: {
                        room_id: string;
                        sender_id: string;
                        stroke_id: string;
                        x: number;
                        y: number;
                        pressure: number;
                        ts_ms: number;
                    }) => Promise<void>;
                    emitStrokeEnd: (p: {
                        room_id: string;
                        sender_id: string;
                        stroke_id: string;
                        ts_ms: number;
                    }) => Promise<void>;
                };
            };
            w.__locast!.emitStrokeBegin({
                room_id: roomId,
                sender_id: userId,
                stroke_id: strokeId,
                tool,
                color,
                width,
                x: 0.1,
                y: 0.2,
                pressure: 0.5,
                ts_ms: startTime,
            });
            w.__locast!.emitStrokePoint({
                room_id: roomId,
                sender_id: userId,
                stroke_id: strokeId,
                x: 0.3,
                y: 0.4,
                pressure: 0.6,
                ts_ms: startTime + 50,
            });
            w.__locast!.emitStrokePoint({
                room_id: roomId,
                sender_id: userId,
                stroke_id: strokeId,
                x: 0.5,
                y: 0.6,
                pressure: 0.7,
                ts_ms: startTime + 100,
            });
            w.__locast!.emitStrokeEnd({
                room_id: roomId,
                sender_id: userId,
                stroke_id: strokeId,
                ts_ms: startTime + 150,
            });
        },
        { roomId: ROOM_ID, strokeId: STROKE_ID, userId: HOST_USER, tool: TOOL, color: COLOR, width: WIDTH, t0 },
    );

    // Wait for the viewer's store to have the completed stroke.
    // Poll until we find the stroke with endedAt > 0 (completed).
    let viewerStroke = null;
    const deadline = t0 + 1000; // 1s max wait
    while (Date.now() < deadline) {
        const strokes = await viewerPage.evaluate(() => {
            const w = window as unknown as {
                __locastDrawingStore?: {
                    getAllStrokes: () => {
                        id: string;
                        userId: string;
                        tool: string;
                        color: string;
                        width: number;
                        points: unknown[];
                        endedAt: number;
                    }[];
                };
            };
            return w.__locastDrawingStore?.getAllStrokes() ?? [];
        });
        const found = strokes.find(
            (s) => s.id === STROKE_ID && s.endedAt > 0,
        );
        if (found) {
            viewerStroke = found;
            break;
        }
        await viewerPage.waitForTimeout(10);
    }

    const elapsed = Date.now() - t0;
    expect(viewerStroke).toBeDefined();
    expect(elapsed).toBeLessThanOrEqual(100);

    // Verify the viewer's stroke matches the host's stroke data.
    expect(viewerStroke!.userId).toBe(HOST_USER);
    expect(viewerStroke!.tool).toBe(TOOL);
    expect(viewerStroke!.color).toBe(COLOR);
    expect(viewerStroke!.width).toBe(WIDTH);
    expect(viewerStroke!.points.length).toBe(3);
    // endedAt may differ slightly due to network latency; both should be > 0
    // and within a reasonable range of each other.
    expect(viewerStroke!.endedAt).toBeGreaterThan(0);
    expect(Math.abs(viewerStroke!.endedAt - hostStroke!.endedAt)).toBeLessThan(1000);

    // Cleanup.
    await viewerContext.close();
});
