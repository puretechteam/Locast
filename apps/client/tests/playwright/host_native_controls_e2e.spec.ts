// The host can run the room from the <video>'s own controls.
//
// Until now only the Play / Pause / "Seek 60s" buttons (and Sync) sent a
// PLAYBACK_CMD: the seek bar and the native play/pause button moved the host's
// video and told nobody, so a host who scrubbed to the next scene left every
// viewer behind, and the Sync button then pulled the host back to its old
// command. These tests use a REAL <video> (a tiny VP8 clip served over HTTP
// Range, decoded by Chromium) so the browser's own `seeked` / `play` / `pause`
// events drive the code under test. The IPC behind it is still the harness shim.
//
// A gesture on the native controls is simulated by calling the element's own
// API directly: the app's code only marks its own moves (via
// services/hostNativeControls), so an unmarked event is what a user produces.

import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { test, expect, injectLocastShim } from "./fixtures/vite-app";
import { serveMedia } from "./fixtures/media-route";
import type { Page } from "@playwright/test";

const CLIP = join(dirname(fileURLToPath(import.meta.url)), "fixtures", "clip_10s.webm");
const HOST_ID = "11111111-1111-1111-1111-111111111111";

const HOST_ROOM = {
    id: "r-native-controls",
    code: "NATV01",
    title: "native controls",
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
    ],
    host_disconnected: false,
    host_disconnect_deadline_ms: null,
};

const VIEWER_ROOM = {
    ...HOST_ROOM,
    id: "r-native-controls-viewer",
    host_user_id: "33333333-3333-3333-3333-333333333333",
    participants: [
        { ...HOST_ROOM.participants[0]!, user_id: "33333333-3333-3333-3333-333333333333" },
        {
            user_id: "22222222-2222-2222-2222-222222222222",
            display_name: "viewer",
            joined_ms: 1_700_000_000_500,
            status: "Connected" as const,
            last_seen_ms: 1_700_000_000_500,
            is_host: false,
        },
    ],
};

type Sent = { action: string; media_position_ms: number; monotonic_seq: number };

async function spaNavigate(page: Page, path: string): Promise<void> {
    await page.evaluate((to) => {
        window.history.pushState({}, "", to);
        window.dispatchEvent(new PopStateEvent("popstate"));
    }, path);
}

async function mountRoomWithRealVideo(
    page: Page,
    room: typeof HOST_ROOM,
    localUserId: string,
): Promise<void> {
    await serveMedia(page, "**/test/clip.webm", CLIP, "video/webm");
    await spaNavigate(page, `/rooms/${room.id}`);
    await page.waitForSelector('[data-testid="room-empty"]', { timeout: 5_000 });
    await page.waitForFunction(
        () => (window as { __locastRoomStore?: unknown }).__locastRoomStore !== undefined,
        undefined,
        { timeout: 5_000 },
    );
    await page.evaluate(
        ({ s, lid }) => {
            const w = window as unknown as {
                __locastRoomStore: {
                    setSummary: (s: unknown) => void;
                    setLocalUserId: (id: string | null) => void;
                };
            };
            w.__locastRoomStore.setLocalUserId(lid);
            w.__locastRoomStore.setSummary(s);
        },
        { s: room, lid: localUserId },
    );
    await page.waitForSelector('[data-testid="locast-player"]', { timeout: 5_000 });
    await page.waitForFunction(
        () => (window as { __locastStore?: unknown }).__locastStore !== undefined,
        undefined,
        { timeout: 5_000 },
    );
    await page.evaluate(() => {
        const w = window as unknown as { __locastStore: { setMediaSrc: (s: string) => void } };
        w.__locastStore.setMediaSrc("/test/clip.webm");
    });
    // The real element must have decoded the clip's metadata.
    await page.waitForFunction(
        () => {
            const v = document.querySelector<HTMLVideoElement>('[data-testid="locast-player-video"]');
            return v !== null && v.readyState >= 1 && Number.isFinite(v.duration);
        },
        undefined,
        { timeout: 10_000 },
    );
}

async function sent(page: Page): Promise<Sent[]> {
    return await page.evaluate(() => {
        const w = window as unknown as {
            __locast_invoke_log?: Array<{ name: string; args: { cmd: Sent } }>;
        };
        return (w.__locast_invoke_log ?? [])
            .filter((e) => e.name === "playback_send")
            .map((e) => ({
                action: e.args.cmd.action,
                media_position_ms: e.args.cmd.media_position_ms,
                monotonic_seq: e.args.cmd.monotonic_seq,
            }));
    });
}

/** A user gesture on the native controls: the element's own API, unmarked. */
async function userGesture(page: Page, body: string): Promise<void> {
    await page.evaluate(async (code) => {
        const v = document.querySelector<HTMLVideoElement>('[data-testid="locast-player-video"]')!;
        const run = new Function("v", `return (async () => { ${code} })();`);
        await run(v);
    }, body);
}

test.beforeEach(async ({ page, locast }) => {
    await injectLocastShim(page);
    await page.goto("/");
    await page.waitForLoadState("domcontentloaded");
    await locast.waitForBridge();
    await locast.resetInvokeLog();
});

test("the clip really loads in the browser with its true duration", async ({ page }) => {
    await mountRoomWithRealVideo(page, HOST_ROOM, HOST_ID);
    const duration = await page.evaluate(
        () => document.querySelector<HTMLVideoElement>('[data-testid="locast-player-video"]')!.duration,
    );
    expect(duration).toBeGreaterThan(9.9);
    expect(duration).toBeLessThan(10.1);
});

test("the host's native play, scrub and pause each reach the viewers", async ({ page }) => {
    await mountRoomWithRealVideo(page, HOST_ROOM, HOST_ID);

    await userGesture(page, "await v.play();");
    await expect.poll(async () => (await sent(page)).map((s) => s.action)).toEqual(["play"]);

    await userGesture(page, "v.currentTime = 7.5;");
    await expect.poll(async () => (await sent(page)).map((s) => s.action)).toEqual(["play", "seek"]);
    const seek = (await sent(page))[1]!;
    expect(seek.media_position_ms).toBeGreaterThan(7_400);
    expect(seek.media_position_ms).toBeLessThan(7_700);

    await userGesture(page, "v.pause();");
    await expect
        .poll(async () => (await sent(page)).map((s) => s.action))
        .toEqual(["play", "seek", "pause"]);

    // Distinct, consecutive sequence numbers: the server accepts only seq+1.
    const seqs = (await sent(page)).map((s) => s.monotonic_seq);
    expect(seqs).toEqual([seqs[0]!, seqs[0]! + 1, seqs[0]! + 2]);
});

test("the app's own moves of the host's video are not sent twice", async ({ page }) => {
    await mountRoomWithRealVideo(page, HOST_ROOM, HOST_ID);
    await page.getByTestId("locast-playback-play").click();
    await expect.poll(async () => (await sent(page)).map((s) => s.action)).toEqual(["play"]);
    await page.getByTestId("locast-playback-pause").click();
    await expect.poll(async () => (await sent(page)).map((s) => s.action)).toEqual(["play", "pause"]);

    // "Seek 60s" moves the video itself, then sends one SEEK. The clip is
    // 10 s long, so the browser clamps the target and still fires `seeked`.
    // (Paused here: seeking a PLAYING clip to its end also ends it, which is
    // a user-visible pause and is covered by the near-end test below.)
    await page.getByTestId("locast-playback-seek60").click();
    await expect
        .poll(async () => (await sent(page)).map((s) => s.action))
        .toEqual(["play", "pause", "seek"]);

    // Let the browser deliver any queued media events, then check that nothing
    // extra went out. A real user gesture still goes through afterwards.
    await userGesture(page, "await new Promise((r) => requestAnimationFrame(() => r(null)));");
    expect((await sent(page)).map((s) => s.action)).toEqual(["play", "pause", "seek"]);
    await userGesture(page, "v.currentTime = 2;");
    await expect
        .poll(async () => (await sent(page)).map((s) => s.action))
        .toEqual(["play", "pause", "seek", "seek"]);
    expect((await sent(page))[3]!.media_position_ms).toBeGreaterThan(1_900);
});

test("playing to the end of the media tells the viewers to pause at the end", async ({ page }) => {
    await mountRoomWithRealVideo(page, HOST_ROOM, HOST_ID);
    await userGesture(page, "await v.play();");
    await expect.poll(async () => (await sent(page)).map((s) => s.action)).toEqual(["play"]);

    // Scrub to 0.3 s before the end and let it play out for real.
    await userGesture(page, "v.currentTime = v.duration - 0.3;");
    await expect
        .poll(async () => (await sent(page)).map((s) => s.action), { timeout: 8_000 })
        .toEqual(["play", "seek", "pause"]);
    const last = (await sent(page))[2]!;
    expect(last.media_position_ms).toBeGreaterThan(9_900);
    expect(last.media_position_ms).toBeLessThanOrEqual(10_100);

    // Pressing Play again at the end makes the browser restart from 0, so
    // the viewers are told to play AND to go back to the start.
    await userGesture(page, "await v.play();");
    await expect
        .poll(async () => (await sent(page)).map((s) => s.action).sort(), { timeout: 5_000 })
        .toEqual(["pause", "play", "play", "seek", "seek"]);
    const all = await sent(page);
    expect(all[all.length - 1]!.media_position_ms).toBeLessThan(500);
});

test("a native pause or scrub before any room command is not sent", async ({ page }) => {
    // The server rejects PAUSE and SEEK while a room is still Open; sending
    // them would burn a sequence number and wedge every later command.
    await mountRoomWithRealVideo(page, HOST_ROOM, HOST_ID);
    await userGesture(page, "v.currentTime = 3;");
    await userGesture(page, "await new Promise((r) => requestAnimationFrame(() => r(null)));");
    expect(await sent(page)).toEqual([]);
});

test("a viewer's native controls stay local and send nothing", async ({ page }) => {
    await mountRoomWithRealVideo(page, VIEWER_ROOM, "22222222-2222-2222-2222-222222222222");
    await userGesture(page, "await v.play(); v.currentTime = 5; v.pause();");
    await userGesture(page, "await new Promise((r) => requestAnimationFrame(() => r(null)));");
    expect(await sent(page)).toEqual([]);
});
