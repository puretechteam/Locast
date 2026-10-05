// Production host path: `isHost` is `summary.host_user_id ===
// localUserId`, and `localUserId` comes from
// `summary.you_user_id`. None of these specs call the
// `__locastRoomStore.setLocalUserId` test seam.
//
// The server never echoes a PLAYBACK_CMD back to the host that
// sent it, so the host must apply every command to its own
// <video> before sending and record it as its own
// `lastApplied` (`services/hostPlayback.ts`). The <video>
// element is instrumented (Chromium ignores `currentTime` and
// rejects `play()` on a source that never loads) so the specs
// can see exactly what the host's own element was told to do.

import { test, expect, injectLocastShim, nth } from "./fixtures/vite-app";
import type { Page } from "@playwright/test";

const HOST_ID = "11111111-1111-4111-8111-111111111111";
const VIEWER_ID = "22222222-2222-4222-8222-222222222222";
const ROOM_ID = "0190a000-0000-7000-8000-0000000c0de5";
const ALL_CAPS = 0xfff;
const CHAT = 0x80;
// PLAYBACK_CONTROL | DRAW | LASER | CHAT | UNDO_OWN
const EDITOR_CAPS = 0x01 | 0x02 | 0x04 | 0x80 | 0x200;

function room(hostUserId: string, youUserId: string, youCapSet: number, viewerCaps = 0) {
    return {
        id: ROOM_ID,
        code: "HOSTPR",
        title: "production host",
        host_user_id: hostUserId,
        host_migration_enabled: true,
        created_ms: 1_700_000_000_000,
        participants: [
            {
                user_id: HOST_ID,
                display_name: "host",
                joined_ms: 1_700_000_000_000,
                status: "Connected" as const,
                last_seen_ms: 1_700_000_000_000,
                is_host: hostUserId === HOST_ID,
                cap_set: hostUserId === HOST_ID ? ALL_CAPS : CHAT,
            },
            {
                user_id: VIEWER_ID,
                display_name: "viewer",
                joined_ms: 1_700_000_000_500,
                status: "Connected" as const,
                last_seen_ms: 1_700_000_000_500,
                is_host: hostUserId === VIEWER_ID,
                cap_set: viewerCaps,
            },
        ],
        host_disconnected: false,
        host_disconnect_deadline_ms: null,
        you_cap_set: youCapSet,
        you_user_id: youUserId,
    };
}

type InvokeEntry = {
    name: string;
    args: {
        cmd?: { action: string; monotonic_seq: number; media_position_ms: number };
        targetUserId?: string;
        addCapSet?: number;
        removeCapSet?: number;
    };
    failed?: boolean;
};

async function spaNavigate(page: Page, path: string): Promise<void> {
    await page.evaluate((to) => {
        window.history.pushState({}, "", to);
        window.dispatchEvent(new PopStateEvent("popstate"));
    }, path);
}

/** Mount the room page from `summary` alone (no
 *  `setLocalUserId`), load media, and instrument the
 *  <video>: `__videoCalls` records play / pause / seeks and
 *  `__setVideoTime` moves the fake playhead. */
async function mountRoom(page: Page, summary: ReturnType<typeof room>): Promise<void> {
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
    }, summary);
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
    await page.waitForSelector('[data-testid="locast-player-video"]', { timeout: 5_000 });
    await page.evaluate(() => {
        const v = document.querySelector(
            '[data-testid="locast-player-video"]',
        ) as HTMLVideoElement;
        const calls: string[] = [];
        let t = 0;
        let paused = true;
        Object.defineProperty(v, "currentTime", {
            configurable: true,
            get: () => t,
            set: (x: number) => {
                t = x;
                calls.push(`seek:${x}`);
            },
        });
        Object.defineProperty(v, "paused", { configurable: true, get: () => paused });
        v.play = () => {
            paused = false;
            calls.push("play");
            return Promise.resolve();
        };
        v.pause = () => {
            paused = true;
            calls.push("pause");
        };
        const w = window as unknown as {
            __videoCalls: string[];
            __setVideoTime: (sec: number) => void;
        };
        w.__videoCalls = calls;
        w.__setVideoTime = (sec) => {
            t = sec;
        };
    });
}

async function videoCalls(page: Page): Promise<string[]> {
    return await page.evaluate(
        () => (window as unknown as { __videoCalls: string[] }).__videoCalls.slice(),
    );
}

async function setVideoTime(page: Page, sec: number): Promise<void> {
    await page.evaluate(
        (s) => (window as unknown as { __setVideoTime: (s: number) => void }).__setVideoTime(s),
        sec,
    );
}

async function invokes(page: Page, name: string): Promise<InvokeEntry[]> {
    return await page.evaluate(
        (n) =>
            (
                (window as unknown as { __locast_invoke_log?: InvokeEntry[] })
                    .__locast_invoke_log ?? []
            ).filter((e) => e.name === n),
        name,
    );
}

async function lastApplied(page: Page): Promise<{
    sender_id: string;
    kind: string;
    media_position_ms: number;
    monotonic_seq: number;
} | null> {
    return await page.evaluate(() => {
        const w = window as unknown as {
            __locastStore?: { getLastApplied: () => unknown };
        };
        return (w.__locastStore?.getLastApplied() ?? null) as {
            sender_id: string;
            kind: string;
            media_position_ms: number;
            monotonic_seq: number;
        } | null;
    });
}

const play = '[data-testid="locast-playback-play"]';
const pause = '[data-testid="locast-playback-pause"]';
const seek60 = '[data-testid="locast-playback-seek60"]';

test.beforeEach(async ({ page, locast }) => {
    await injectLocastShim(page);
    await page.goto("/");
    await page.waitForLoadState("domcontentloaded");
    await locast.waitForBridge();
    await locast.resetInvokeLog();
});

test("you_user_id === host_user_id makes the local user the host (no test seam)", async ({
    page,
}) => {
    await mountRoom(page, room(HOST_ID, HOST_ID, ALL_CAPS));
    const seamId = await page.evaluate(
        () =>
            (
                window as unknown as {
                    __locastRoomStore?: { getLocalUserId: () => string | null };
                }
            ).__locastRoomStore?.getLocalUserId() ?? null,
    );
    expect(seamId).toBeNull();
    await expect(page.getByRole("button", { name: "Permissions" })).toBeVisible();
    await expect(page.locator('[data-testid="viewer-positions"]')).toBeVisible();
    await expect(page.locator(play)).toBeEnabled();
    // The room is still `Open`: the server would reject PAUSE
    // and SEEK, so they wait for the first PLAY.
    await expect(page.locator(pause)).toBeDisabled();
    await expect(page.locator(seek60)).toBeDisabled();
});

test("a viewer (you_user_id !== host_user_id) gets no host UI", async ({ page }) => {
    await mountRoom(page, room(HOST_ID, VIEWER_ID, CHAT));
    await expect(page.getByRole("button", { name: "Permissions" })).toHaveCount(0);
    await expect(page.locator('[data-testid="viewer-positions"]')).toHaveCount(0);
    await expect(page.locator(play)).toBeDisabled();
    await expect(page.locator(pause)).toBeDisabled();
    await expect(page.locator(seek60)).toBeDisabled();
});

test("host Play / Pause / Seek drive the host's own <video> and send seq 1, 2, 3", async ({
    page,
}) => {
    await mountRoom(page, room(HOST_ID, HOST_ID, ALL_CAPS));
    await setVideoTime(page, 12.5);

    await page.locator(play).click();
    await expect.poll(async () => (await invokes(page, "playback_send")).length).toBe(1);
    let sends = await invokes(page, "playback_send");
    // PLAY carries where the host's video actually is.
    expect(nth(sends, 0).args.cmd).toEqual({
        action: "play",
        monotonic_seq: 1,
        media_position_ms: 12_500,
    });
    expect(await videoCalls(page)).toContain("play");
    // Recorded as the host's own command: the host has a
    // "last host command" even though the server never
    // echoes it back.
    await expect.poll(async () => (await lastApplied(page))?.sender_id).toBe(HOST_ID);
    expect(await lastApplied(page)).toMatchObject({
        kind: "play",
        media_position_ms: 12_500,
        monotonic_seq: 1,
    });

    await expect(page.locator(pause)).toBeEnabled();
    await setVideoTime(page, 20);
    await page.locator(pause).click();
    await expect.poll(async () => (await invokes(page, "playback_send")).length).toBe(2);
    sends = await invokes(page, "playback_send");
    expect(nth(sends, 1).args.cmd).toEqual({
        action: "pause",
        monotonic_seq: 2,
        media_position_ms: 20_000,
    });

    await page.locator(seek60).click();
    await expect.poll(async () => (await invokes(page, "playback_send")).length).toBe(3);
    sends = await invokes(page, "playback_send");
    expect(nth(sends, 2).args.cmd).toEqual({
        action: "seek",
        monotonic_seq: 3,
        media_position_ms: 60_000,
    });
    await expect.poll(async () => (await lastApplied(page))?.kind).toBe("seek");

    // Each command reached the <video> exactly once: the
    // Player's host-echo check did not re-apply the recorded
    // commands on top of the local apply.
    const calls = await videoCalls(page);
    expect(calls.filter((c) => c === "play")).toHaveLength(1);
    expect(calls.filter((c) => c === "pause")).toHaveLength(1);
    expect(calls.filter((c) => c === "seek:60")).toHaveLength(1);
});

test("a playback_send that fails locally gives its monotonic_seq back", async ({ page }) => {
    await mountRoom(page, room(HOST_ID, HOST_ID, ALL_CAPS));
    await page.evaluate(() => {
        (window as unknown as { __locast_playbackSendFailures: number })
            .__locast_playbackSendFailures = 1;
    });
    await page.locator(play).click();
    await expect.poll(async () => (await invokes(page, "playback_send")).length).toBe(1);
    // Nothing was recorded, so Pause / Seek stay gated.
    expect(await lastApplied(page)).toBeNull();
    await expect(page.locator(pause)).toBeDisabled();

    await page.locator(play).click();
    await expect.poll(async () => (await invokes(page, "playback_send")).length).toBe(2);
    const sends = await invokes(page, "playback_send");
    expect(nth(sends, 0).failed).toBe(true);
    expect(nth(sends, 1).failed).toBeUndefined();
    // The retry reuses seq 1: the server never saw the first.
    expect(sends.map((s) => s.args.cmd?.monotonic_seq)).toEqual([1, 1]);
    await expect(page.locator(pause)).toBeEnabled();
});

test("host Sync to Host works from the host's own recorded command", async ({ page }) => {
    await mountRoom(page, room(HOST_ID, HOST_ID, ALL_CAPS));
    const sync = page.locator('[data-testid="sync-button"]');
    // No command yet: nothing to sync to.
    await expect(sync).toBeDisabled();

    await setVideoTime(page, 5);
    await page.locator(play).click();
    await expect(sync).toBeEnabled();
    await sync.click();
    await expect.poll(async () => (await invokes(page, "playback_send")).length).toBe(2);
    const sends = await invokes(page, "playback_send");
    expect(nth(sends, 1).args.cmd?.action).toBe("seek");
    expect(nth(sends, 1).args.cmd?.monotonic_seq).toBe(2);
    // Projected from the PLAY at 5 s; well under a few seconds.
    expect(nth(sends, 1).args.cmd?.media_position_ms).toBeGreaterThanOrEqual(5_000);
    expect(nth(sends, 1).args.cmd?.media_position_ms).toBeLessThan(10_000);
});

test("a host migration onto the local user turns the host UI on", async ({ page, locast }) => {
    await mountRoom(page, room(HOST_ID, VIEWER_ID, CHAT | 0x02));
    await expect(page.locator(play)).toBeDisabled();
    await expect(page.getByRole("button", { name: "Permissions" })).toHaveCount(0);

    // The Rust room client's HOST_MIGRATED -> room://event, with
    // the promoted participant's cap set (`cap::HOST`).
    await locast.emitCapabilityUpdate(room(VIEWER_ID, VIEWER_ID, ALL_CAPS));
    await expect(page.locator(play)).toBeEnabled();
    await expect(page.getByRole("button", { name: "Permissions" })).toBeVisible();
    await expect(page.locator('[data-testid="viewer-positions"]')).toBeVisible();
});

test("Permissions Apply replaces only the changed participants' cap sets", async ({
    page,
    locast,
}) => {
    await mountRoom(page, room(HOST_ID, HOST_ID, ALL_CAPS));
    await page.getByRole("button", { name: "Permissions" }).click();
    const editor = page.locator(`input[name="preset-${VIEWER_ID}"][value="editor"]`);
    await editor.check();
    await page.getByRole("button", { name: "Apply" }).click();
    await expect.poll(async () => (await invokes(page, "room_permission_set")).length).toBe(1);
    let sets = await invokes(page, "room_permission_set");
    expect(nth(sets, 0).args).toEqual({
        targetUserId: VIEWER_ID,
        addCapSet: EDITOR_CAPS,
        removeCapSet: 0xffff_ffff,
    });

    // The server's CAPABILITY_UPDATE lands; the modal now opens
    // on the viewer's real preset, and moving them back down to
    // Viewer replaces (revokes) the Editor caps.
    await locast.emitCapabilityUpdate(room(HOST_ID, HOST_ID, ALL_CAPS, EDITOR_CAPS));
    await page.getByRole("button", { name: "Permissions" }).click();
    await expect(editor).toBeChecked();
    await page.getByRole("button", { name: "Apply" }).click();
    // Unchanged: nothing sent.
    await page.waitForTimeout(100);
    expect(await invokes(page, "room_permission_set")).toHaveLength(1);

    await page.getByRole("button", { name: "Permissions" }).click();
    await page.locator(`input[name="preset-${VIEWER_ID}"][value="viewer"]`).check();
    await page.getByRole("button", { name: "Apply" }).click();
    await expect.poll(async () => (await invokes(page, "room_permission_set")).length).toBe(2);
    sets = await invokes(page, "room_permission_set");
    expect(nth(sets, 1).args).toEqual({
        targetUserId: VIEWER_ID,
        addCapSet: CHAT,
        removeCapSet: 0xffff_ffff,
    });
});

test("a paused host's Sync to Host seeks to the frozen paused position, not an extrapolated one", async ({
    page,
}) => {
    await mountRoom(page, room(HOST_ID, HOST_ID, ALL_CAPS));
    const sync = page.locator('[data-testid="sync-button"]');
    const hostPaused = async (): Promise<boolean> =>
        await page.evaluate(
            () =>
                (
                    window as unknown as {
                        __locastStore?: { getHostPaused: () => boolean };
                    }
                ).__locastStore?.getHostPaused() ?? false,
        );

    await setVideoTime(page, 5);
    await page.locator(play).click();
    await expect.poll(hostPaused).toBe(false);

    // The host's own PAUSE is recorded via recordHostCommand (the
    // server never echoes it back), so it must freeze the clock.
    await setVideoTime(page, 20);
    await page.locator(pause).click();
    await expect.poll(hostPaused).toBe(true);
    await page.waitForTimeout(1_200);

    await expect(sync).toBeEnabled();
    await sync.click();
    await expect.poll(async () => (await invokes(page, "playback_send")).length).toBe(3);
    let sends = await invokes(page, "playback_send");
    expect(nth(sends, 2).args.cmd?.action).toBe("seek");
    // Exactly the paused position: 1.2 s of wall time did not leak in.
    expect(nth(sends, 2).args.cmd?.media_position_ms).toBe(20_000);
    // A SEEK keeps the room paused, so the next Sync stays frozen too.
    expect(await hostPaused()).toBe(true);
    await page.waitForTimeout(500);
    await sync.click();
    await expect.poll(async () => (await invokes(page, "playback_send")).length).toBe(4);
    sends = await invokes(page, "playback_send");
    expect(nth(sends, 3).args.cmd?.media_position_ms).toBe(20_000);
});
