// Recovery from dropped drawing events (DRAW_SYNC), through the real
// drawing event bridge and stores.
//
// The server stamps every DRAW_* rebroadcast with the room's drawing
// sequence number and, when this client's room subscription drops
// events, sends `drawing://sync` with the room's whole drawing state.
// These tests play that stream at the Tauri-event boundary: a dropped
// undo / clear followed by a sync, duplicates and replays, the local
// canvas, and a sync for another room. The server side (lag
// detection, sequence numbers, snapshot contents) is covered by the
// Rust tests in apps/server/src/rooms/dispatch.rs.

import { test, expect, injectLocastShim } from "./fixtures/vite-app";
import type { Page } from "@playwright/test";

const ROOM = {
    id: "r-drawsync-room",
    code: "DSYNC1",
    title: "Drawing sync",
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
const REMOTE = "99999999-9999-9999-9999-999999999999";

test.beforeEach(async ({ page, locast }) => {
    await injectLocastShim(page);
    await page.goto("/");
    await page.waitForLoadState("domcontentloaded");
    await locast.waitForBridge();
    await page.evaluate((to) => {
        window.history.pushState({}, "", to);
        window.dispatchEvent(new PopStateEvent("popstate"));
    }, `/rooms/${ROOM.id}`);
    await page.waitForFunction(() => (window as { __locastRoomStore?: unknown }).__locastRoomStore !== undefined);
    await page.evaluate((s) => {
        (window as unknown as { __locastRoomStore: { setSummary: (s: unknown) => void } }).__locastRoomStore.setSummary(s);
    }, ROOM);
    await page.waitForFunction(() => (window as { __locastStore?: unknown }).__locastStore !== undefined);
    await page.evaluate(() => {
        const w = window as unknown as {
            __locastStore: { setMediaSrc: (s: string) => void; setMediaReady: (r: boolean) => void };
        };
        w.__locastStore.setMediaSrc("/test/asset.mp4");
        w.__locastStore.setMediaReady(true);
    });
    await page.waitForFunction(
        () => (window as { __locastDrawingStore?: unknown }).__locastDrawingStore !== undefined &&
            (window as { __locastDrawing?: unknown }).__locastDrawing !== undefined &&
            (window as { __locast_drawing_subscribed?: boolean }).__locast_drawing_subscribed === true,
    );
    await page.evaluate((roomId) => {
        (window as unknown as { __locastDrawingStore: { setRoomId: (id: string) => void } }).__locastDrawingStore.setRoomId(roomId);
    }, ROOM.id);
});

async function emit(page: Page, name: string, payload: unknown): Promise<void> {
    await page.evaluate(
        ({ name, payload }) =>
            import("/tests/playwright/shim/tauriShim.ts" as string).then((m) => m.__emit(name, payload)),
        { name, payload },
    );
}

async function remote(page: Page): Promise<Array<{ id: string; points: number; ended: boolean }>> {
    return await page.evaluate(() => {
        const w = window as unknown as {
            __locastDrawingStore: { getAllStrokes: () => Array<{ id: string; points: unknown[]; endedAt: number }> };
        };
        return w.__locastDrawingStore
            .getAllStrokes()
            .map((s) => ({ id: s.id, points: s.points.length, ended: s.endedAt !== 0 }));
    });
}

const base = { room_id: ROOM.id, sender_id: REMOTE };
const begin = (id: string, seq: number) =>
    ({ ...base, stroke_id: id, tool: "pen", color: "#f00", width: 2, x: 0.1, y: 0.1, pressure: 0.5, ts_ms: 1, seq });
const point = (id: string, seq: number, x = 0.2) =>
    ({ ...base, stroke_id: id, x, y: 0.2, pressure: 0.5, ts_ms: 2, seq });
const end = (id: string, seq: number) => ({ ...base, stroke_id: id, ts_ms: 3, seq });

function syncStroke(id: string, points: number, ended: boolean, owner = REMOTE) {
    return {
        stroke_id: id,
        owner_id: owner,
        begin: { tool: "pen", color: "#0f0", width: 3, x: 0.5, y: 0.5, pressure: 0.5, ts_ms: 1 },
        points: Array.from({ length: points }, (_, i) => ({ x: i / 10, y: 0.3, pressure: 0.5, ts_ms: 2 + i })),
        end_ts_ms: ended ? 9 : null,
    };
}

test("a dropped DRAW_UNDO is recovered by DRAW_SYNC", async ({ page }) => {
    await emit(page, "drawing://begin", begin("a", 1));
    await emit(page, "drawing://end", end("a", 2));
    await emit(page, "drawing://begin", begin("b", 3));
    await emit(page, "drawing://end", end("b", 4));
    // seq 5 (DRAW_UNDO of b) and the start of c (6, 7) are dropped.
    await expect.poll(() => remote(page)).toEqual([
        { id: "a", points: 1, ended: true },
        { id: "b", points: 1, ended: true },
    ]);
    await emit(page, "drawing://sync", {
        room_id: ROOM.id,
        seq: 7,
        strokes: [syncStroke("a", 0, true), syncStroke("c", 1, false)],
    });
    // Live again after the snapshot.
    await emit(page, "drawing://point", point("c", 8));
    await emit(page, "drawing://end", end("c", 9));
    await expect.poll(() => remote(page)).toEqual([
        { id: "a", points: 1, ended: true },
        { id: "c", points: 3, ended: true },
    ]);
});

test("a dropped DRAW_CLEAR is recovered by DRAW_SYNC", async ({ page }) => {
    await emit(page, "drawing://begin", begin("a", 1));
    await emit(page, "drawing://end", end("a", 2));
    await emit(page, "drawing://begin", begin("b", 3));
    await emit(page, "drawing://end", end("b", 4));
    // seq 5 (DRAW_CLEAR) and stroke d (6..8) are dropped.
    await emit(page, "drawing://sync", { room_id: ROOM.id, seq: 8, strokes: [syncStroke("d", 2, true)] });
    await expect.poll(() => remote(page)).toEqual([{ id: "d", points: 3, ended: true }]);
});

test("duplicates and events a snapshot already covers change nothing", async ({ page }) => {
    await emit(page, "drawing://begin", begin("a", 1));
    await emit(page, "drawing://point", point("a", 2));
    await emit(page, "drawing://point", point("a", 2));
    await emit(page, "drawing://begin", begin("a", 1));
    await expect.poll(() => remote(page)).toEqual([{ id: "a", points: 2, ended: false }]);
    await emit(page, "drawing://sync", { room_id: ROOM.id, seq: 6, strokes: [syncStroke("a", 3, true)] });
    // Stale replays from before the snapshot: the undo of a, a
    // point, a BEGIN of a stroke the snapshot does not have.
    await emit(page, "drawing://undo", { ...base, stroke_id: "a", seq: 5 });
    await emit(page, "drawing://point", point("a", 3));
    await emit(page, "drawing://begin", begin("ghost", 4));
    await expect.poll(() => remote(page)).toEqual([{ id: "a", points: 4, ended: true }]);
    // A real later undo still applies.
    await emit(page, "drawing://undo", { ...base, stroke_id: "a", seq: 7 });
    await expect.poll(() => remote(page)).toEqual([]);
});

test("the local canvas drops strokes the server no longer has and keeps the rest once", async ({ page }) => {
    const ids = await page.evaluate(() => {
        const d = (window as unknown as {
            __locastDrawing: { beginStroke: () => string; endStroke: (t?: number) => void };
        }).__locastDrawing;
        const kept = d.beginStroke();
        d.endStroke(5);
        const undone = d.beginStroke();
        d.endStroke(6);
        const drawing = d.beginStroke();
        return { kept, undone, drawing };
    });
    await emit(page, "drawing://sync", {
        room_id: ROOM.id,
        seq: 4,
        strokes: [syncStroke(ids.kept, 2, true, ROOM.host_user_id), syncStroke("r", 1, true)],
    });
    const local = await page.evaluate(() =>
        (window as unknown as { __locastDrawing: { getStrokes: () => Array<{ id: string }> } }).__locastDrawing
            .getStrokes()
            .map((s) => s.id),
    );
    // Undone while we were behind: gone. Being drawn now: kept.
    expect(local.sort()).toEqual([ids.drawing, ids.kept].sort());
    // Our own stroke is not duplicated into the remote layer.
    await expect.poll(() => remote(page)).toEqual([{ id: "r", points: 2, ended: true }]);
});

test("a DRAW_SYNC for another room is ignored", async ({ page }) => {
    await emit(page, "drawing://begin", begin("a", 1));
    await emit(page, "drawing://end", end("a", 2));
    await emit(page, "drawing://sync", { room_id: "some-other-room", seq: 50, strokes: [] });
    await emit(page, "drawing://begin", begin("b", 3));
    await expect.poll(() => remote(page)).toEqual([
        { id: "a", points: 1, ended: true },
        { id: "b", points: 1, ended: false },
    ]);
});
