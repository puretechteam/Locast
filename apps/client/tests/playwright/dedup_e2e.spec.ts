// P4-T07 acceptance (roadmap): "a unit test replays a
// stream with a duplicate seq, a gap, and an out-of-order
// seq; duplicates are dropped, the gap is filled within
// 5 s, the out-of-order SEEK is dropped."
//
// This Playwright spec exercises the per-sender dedup
// logic end-to-end through the React layer by injecting
// synthetic `playback://state` events at the same
// Tauri-event boundary the real Rust backend uses, then
// asserting on the store's lastApplied event and the
// dedup state via the Vite test seam.
//
// Scenarios (one per `test`):
//  - duplicate `monotonic_seq` is dropped (lastApplied
//    remains the original event; dedup `lastAppliedSeq`
//    unchanged).
//  - gap (seq=3 after seq=1) buffers, then the missing
//    seq=2 fills the gap AND drains the parked seq=3 in
//    the same call.
//  - out-of-order SEEK: a tighter-gap event parked first,
//    then a wider-gap event replaces it (older parked
//    dropped). A replay of the original tighter-gap event
//    after the wider-gap is parked is dropped as
//    out-of-order.
//  - buffer timeout: a parked event that never has its
//    gap filled is force-applied after the 5 s grace
//    window. Driving the seam's `tickDedup(now)` advances
//    the clock deterministically (no real `setTimeout`).
//  - per-sender independence: two senders each maintain
//    their own `lastAppliedSeq`; a duplicate from one
//    does not affect the other.
//  - room change: switching rooms clears the dedup state
//    so a fresh sender with seq=1 is not mistaken for a
//    duplicate from the previous room.

import { test, expect, injectLocastShim } from "./fixtures/vite-app";
import type { Page } from "@playwright/test";

const ROOM_A = {
    id: "r-p4t07-room-a",
    code: "AAAA11",
    title: "P4-T07 room A",
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

const ROOM_B = {
    id: "r-p4t07-room-b",
    code: "BBBB22",
    title: "P4-T07 room B",
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

const HOST_ID = "11111111-1111-1111-1111-111111111111";
const VIEWER_ID = "22222222-2222-2222-2222-222222222222";

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

async function mountRoomWithPlayer(
    page: Page,
    roomSummary: typeof ROOM_A,
): Promise<void> {
    await spaNavigate(page, `/rooms/${roomSummary.id}`);
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
    }, roomSummary);
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
}

interface DedupSenderRow {
    senderId: string;
    lastAppliedSeq: number;
    pendingSeq: number | null;
    pendingParkedAtMs: number | null;
}

async function readDedupSnapshot(
    page: Page,
): Promise<{ bySender: DedupSenderRow[] }> {
    return await page.evaluate(() => {
        const w = window as unknown as {
            __locastStore?: { getDedupSnapshot: () => unknown };
        };
        if (!w.__locastStore) {
            throw new Error("playback store shim not present on window");
        }
        return w.__locastStore.getDedupSnapshot() as { bySender: DedupSenderRow[] };
    });
}

async function tickDedup(page: Page, nowMs: number): Promise<number> {
    return await page.evaluate((now) => {
        const w = window as unknown as {
            __locastStore?: { tickDedup: (now: number) => number };
        };
        if (!w.__locastStore) {
            throw new Error("playback store shim not present on window");
        }
        return w.__locastStore.tickDedup(now);
    }, nowMs);
}

function makePlaybackEvent(opts: {
    room_id: string;
    server_seq: number;
    monotonic_seq: number;
    sender_id: string;
    kind: "play" | "pause" | "seek";
    media_position_ms: number;
    server_ts_ms: number;
}) {
    return {
        room_id: opts.room_id,
        server_seq: opts.server_seq,
        monotonic_seq: opts.monotonic_seq,
        sender_id: opts.sender_id,
        kind: opts.kind,
        media_position_ms: opts.media_position_ms,
        server_ts_ms: opts.server_ts_ms,
    };
}

test("duplicate monotonic_seq is dropped (lastApplied unchanged)", async ({
    page,
    locast,
}) => {
    await mountRoomWithPlayer(page, ROOM_A);
    await locast.emitPlaybackState(
        makePlaybackEvent({
            room_id: ROOM_A.id,
            server_seq: 1,
            monotonic_seq: 1,
            sender_id: HOST_ID,
            kind: "seek",
            media_position_ms: 12_000,
            server_ts_ms: 1_700_000_000_000,
        }),
    );
    await locast.emitPlaybackState(
        makePlaybackEvent({
            room_id: ROOM_A.id,
            server_seq: 2,
            monotonic_seq: 1,
            sender_id: HOST_ID,
            kind: "seek",
            media_position_ms: 99_000,
            server_ts_ms: 1_700_000_000_100,
        }),
    );
    await expect
    .poll(
        async () => {
            const snap = await readDedupSnapshot(page);
            const row = snap.bySender.find((r) => r.senderId === HOST_ID);
            return row?.lastAppliedSeq;
        },
        { timeout: 1_000 },
    )
    .toBe(1);
    await expect
    .poll(
        async () => {
            const la = await page.evaluate(() => {
                const w = window as unknown as {
                    __locastStore?: { getLastApplied: () => unknown };
                };
                return w.__locastStore?.getLastApplied() as
                    | { media_position_ms: number }
                    | null;
            });
            return la?.media_position_ms;
        },
        { timeout: 1_000 },
    )
    .toBe(12_000);
});

test("a gap (seq=3 after seq=1) is buffered, then the missing seq=2 fills it and applies the held SEEK", async ({
    page,
    locast,
}) => {
    await mountRoomWithPlayer(page, ROOM_A);
    // seq=1.
    await locast.emitPlaybackState(
        makePlaybackEvent({
            room_id: ROOM_A.id,
            server_seq: 1,
            monotonic_seq: 1,
            sender_id: HOST_ID,
            kind: "play",
            media_position_ms: 0,
            server_ts_ms: 1_700_000_000_000,
        }),
    );
    // seq=3 (gap; seq=2 missing).
    await locast.emitPlaybackState(
        makePlaybackEvent({
            room_id: ROOM_A.id,
            server_seq: 2,
            monotonic_seq: 3,
            sender_id: HOST_ID,
            kind: "seek",
            media_position_ms: 30_000,
            server_ts_ms: 1_700_000_000_200,
        }),
    );
    // The seq=3 SEEK is parked; lastApplied is still the
    // seq=1 PLAY (server_seq=2 is also a duplicate of
    // itself? No: server_seq=2 > 1, so the store should
    // accept it for the server_seq layer. The per-sender
    // dedup layer is what holds it back.).
    // The dedup module keeps it pending until the missing
    // seq=2 arrives.
    await expect
    .poll(
        async () => {
            const snap = await readDedupSnapshot(page);
            const row = snap.bySender.find((r) => r.senderId === HOST_ID);
            return row?.pendingSeq;
        },
        { timeout: 1_000 },
    )
    .toBe(3);
    await expect
    .poll(
        async () => {
            const snap = await readDedupSnapshot(page);
            const row = snap.bySender.find((r) => r.senderId === HOST_ID);
            return row?.lastAppliedSeq;
        },
        { timeout: 1_000 },
    )
    .toBe(1);

    // seq=2 arrives and fills the gap. The store applies
    // the parked seq=3 (the drained event) because the gap
    // is filled exactly; the dedup state advances to 3.
    await locast.emitPlaybackState(
        makePlaybackEvent({
            room_id: ROOM_A.id,
            server_seq: 3,
            monotonic_seq: 2,
            sender_id: HOST_ID,
            kind: "play",
            media_position_ms: 0,
            server_ts_ms: 1_700_000_000_300,
        }),
    );
    await expect
    .poll(
        async () => {
            const snap = await readDedupSnapshot(page);
            const row = snap.bySender.find((r) => r.senderId === HOST_ID);
            return row?.lastAppliedSeq;
        },
        { timeout: 1_000 },
    )
    .toBe(3);
    await expect
    .poll(
        async () => {
            const snap = await readDedupSnapshot(page);
            const row = snap.bySender.find((r) => r.senderId === HOST_ID);
            return row?.pendingSeq;
        },
        { timeout: 1_000 },
    )
    .toBe(null);
});

test("an out-of-order SEEK (replayed seq=3 after seq=5 parked) is dropped", async ({
    page,
    locast,
}) => {
    await mountRoomWithPlayer(page, ROOM_A);
    // Bootstrap with seq=1.
    await locast.emitPlaybackState(
        makePlaybackEvent({
            room_id: ROOM_A.id,
            server_seq: 1,
            monotonic_seq: 1,
            sender_id: HOST_ID,
            kind: "play",
            media_position_ms: 0,
            server_ts_ms: 1_700_000_000_000,
        }),
    );
    // Park seq=3 (tight gap).
    await locast.emitPlaybackState(
        makePlaybackEvent({
            room_id: ROOM_A.id,
            server_seq: 2,
            monotonic_seq: 3,
            sender_id: HOST_ID,
            kind: "seek",
            media_position_ms: 10_000,
            server_ts_ms: 1_700_000_000_200,
        }),
    );
    // Park seq=5 (wider gap; replaces the parked seq=3).
    await locast.emitPlaybackState(
        makePlaybackEvent({
            room_id: ROOM_A.id,
            server_seq: 3,
            monotonic_seq: 5,
            sender_id: HOST_ID,
            kind: "seek",
            media_position_ms: 50_000,
            server_ts_ms: 1_700_000_000_400,
        }),
    );
    await expect
    .poll(
        async () => {
            const snap = await readDedupSnapshot(page);
            const row = snap.bySender.find((r) => r.senderId === HOST_ID);
            return row?.pendingSeq;
        },
        { timeout: 1_000 },
    )
    .toBe(5);

    // Replay the original seq=3 (now older than the
    // parked seq=5). The dedup math must drop it as
    // out-of-order and the parked seq=5 stays intact.
    await locast.emitPlaybackState(
        makePlaybackEvent({
            room_id: ROOM_A.id,
            server_seq: 4,
            monotonic_seq: 3,
            sender_id: HOST_ID,
            kind: "seek",
            media_position_ms: 10_000,
            server_ts_ms: 1_700_000_000_500,
        }),
    );
    await expect
    .poll(
        async () => {
            const snap = await readDedupSnapshot(page);
            const row = snap.bySender.find((r) => r.senderId === HOST_ID);
            return row?.pendingSeq;
        },
        { timeout: 1_000 },
    )
    .toBe(5);
    await expect
    .poll(
        async () => {
            const snap = await readDedupSnapshot(page);
            const row = snap.bySender.find((r) => r.senderId === HOST_ID);
            return row?.lastAppliedSeq;
        },
        { timeout: 1_000 },
    )
    .toBe(1);
});

test("a parked event whose gap never fills is force-applied after the 5 s timeout", async ({
    page,
    locast,
}) => {
    await mountRoomWithPlayer(page, ROOM_A);
    // Bootstrap.
    const parkedAtMs = await page.evaluate(() => Date.now());
    await locast.emitPlaybackState(
        makePlaybackEvent({
            room_id: ROOM_A.id,
            server_seq: 1,
            monotonic_seq: 1,
            sender_id: HOST_ID,
            kind: "play",
            media_position_ms: 0,
            server_ts_ms: parkedAtMs,
        }),
    );
    // Park seq=5 (no seq=2,3,4 ever arrive).
    await locast.emitPlaybackState(
        makePlaybackEvent({
            room_id: ROOM_A.id,
            server_seq: 2,
            monotonic_seq: 5,
            sender_id: HOST_ID,
            kind: "seek",
            media_position_ms: 60_000,
            server_ts_ms: parkedAtMs + 100,
        }),
    );
    // Drive the dedup tick deterministically. The
    // dedup module stored `parkedAtMs = Date.now()`
    // inside the React handler; we advance a clock that
    // is 6 s past the parking time (the grace window is
    // 5 s per the roadmap + architecture §13.2).
    const applied = await tickDedup(page, parkedAtMs + 6_000);
    expect(applied).toBe(1);
    await expect
    .poll(
        async () => {
            const snap = await readDedupSnapshot(page);
            const row = snap.bySender.find((r) => r.senderId === HOST_ID);
            return row?.lastAppliedSeq;
        },
        { timeout: 1_000 },
    )
    .toBe(5);
});

test("per-sender independence: a duplicate from one sender does not affect another", async ({
    page,
    locast,
}) => {
    await mountRoomWithPlayer(page, ROOM_A);
    await locast.emitPlaybackState(
        makePlaybackEvent({
            room_id: ROOM_A.id,
            server_seq: 1,
            monotonic_seq: 1,
            sender_id: HOST_ID,
            kind: "seek",
            media_position_ms: 1_000,
            server_ts_ms: 1_700_000_000_000,
        }),
    );
    await locast.emitPlaybackState(
        makePlaybackEvent({
            room_id: ROOM_A.id,
            server_seq: 2,
            monotonic_seq: 1,
            sender_id: VIEWER_ID,
            kind: "seek",
            media_position_ms: 2_000,
            server_ts_ms: 1_700_000_000_100,
        }),
    );
    // Both senders' seq=1 apply. Now duplicate each.
    await locast.emitPlaybackState(
        makePlaybackEvent({
            room_id: ROOM_A.id,
            server_seq: 3,
            monotonic_seq: 1,
            sender_id: HOST_ID,
            kind: "seek",
            media_position_ms: 99_000,
            server_ts_ms: 1_700_000_000_200,
        }),
    );
    await locast.emitPlaybackState(
        makePlaybackEvent({
            room_id: ROOM_A.id,
            server_seq: 4,
            monotonic_seq: 1,
            sender_id: VIEWER_ID,
            kind: "seek",
            media_position_ms: 88_000,
            server_ts_ms: 1_700_000_000_300,
        }),
    );
    // Both duplicates drop; both senders' lastAppliedSeq
    // remain at 1.
    const snap = await readDedupSnapshot(page);
    const hostRow = snap.bySender.find((r) => r.senderId === HOST_ID);
    const viewerRow = snap.bySender.find((r) => r.senderId === VIEWER_ID);
    expect(hostRow?.lastAppliedSeq).toBe(1);
    expect(viewerRow?.lastAppliedSeq).toBe(1);
});

test("switching rooms clears the dedup state (no seq counter leak across rooms)", async ({
    page,
    locast,
}) => {
    await mountRoomWithPlayer(page, ROOM_A);
    await locast.emitPlaybackState(
        makePlaybackEvent({
            room_id: ROOM_A.id,
            server_seq: 1,
            monotonic_seq: 1,
            sender_id: HOST_ID,
            kind: "play",
            media_position_ms: 0,
            server_ts_ms: 1_700_000_000_000,
        }),
    );
    await expect
    .poll(
        async () => {
            const snap = await readDedupSnapshot(page);
            return snap.bySender.length;
        },
        { timeout: 1_000 },
    )
    .toBe(1);

    // Switch rooms.
    await page.evaluate((s) => {
        const w = window as unknown as {
            __locastRoomStore?: { setSummary: (s: unknown) => void };
        };
        if (!w.__locastRoomStore) {
            throw new Error("room store shim not present on window");
        }
        w.__locastRoomStore.setSummary(s);
    }, ROOM_B);
    await page.waitForFunction(
        () => {
            const w = window as unknown as {
                __locastStore?: { getLastApplied: () => unknown };
            };
            return w.__locastStore?.getLastApplied() === null;
        },
        undefined,
        { timeout: 5_000 },
    );
    // The dedup map is empty after the room change.
    const snap = await readDedupSnapshot(page);
    expect(snap.bySender).toEqual([]);

    // A seq=1 event from the same sender in the new room
    // applies (it is not mistaken for a duplicate of the
    // previous room's last event).
    await locast.emitPlaybackState(
        makePlaybackEvent({
            room_id: ROOM_B.id,
            server_seq: 1,
            monotonic_seq: 1,
            sender_id: HOST_ID,
            kind: "play",
            media_position_ms: 0,
            server_ts_ms: 1_700_000_001_000,
        }),
    );
    await expect
    .poll(
        async () => {
            const s = await readDedupSnapshot(page);
            const row = s.bySender.find((r) => r.senderId === HOST_ID);
            return row?.lastAppliedSeq;
        },
        { timeout: 1_000 },
    )
    .toBe(1);
});