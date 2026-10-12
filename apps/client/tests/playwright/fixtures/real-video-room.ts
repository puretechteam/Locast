// Shared setup for specs that need a REAL <video> in a room: a media file
// served over HTTP Range (see media-route.ts) that Chromium decodes, with the
// harness's shimmed IPC around it.

import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import type { Page } from "@playwright/test";
import { serveMedia } from "./media-route";

export const FIXTURES_DIR = dirname(fileURLToPath(import.meta.url));
/** A tiny 10 s VP8 clip, small enough to be tracked in git. */
export const CLIP_10S = join(FIXTURES_DIR, "clip_10s.webm");

export const HOST_ID = "11111111-1111-1111-1111-111111111111";
export const VIEWER_ID = "22222222-2222-2222-2222-222222222222";
export const OTHER_HOST_ID = "33333333-3333-3333-3333-333333333333";

export const HOST_ROOM = {
    id: "r-real-video-host",
    code: "REAL01",
    title: "real video",
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

export const VIEWER_ROOM = {
    ...HOST_ROOM,
    id: "r-real-video-viewer",
    host_user_id: OTHER_HOST_ID,
    participants: [
        { ...HOST_ROOM.participants[0]!, user_id: OTHER_HOST_ID },
        {
            user_id: VIEWER_ID,
            display_name: "viewer",
            joined_ms: 1_700_000_000_500,
            status: "Connected" as const,
            last_seen_ms: 1_700_000_000_500,
            is_host: false,
        },
    ],
};

export type RoomFixture = typeof HOST_ROOM & {
    playback?: {
        paused: boolean;
        media_position_ms: number;
        server_ts_ms: number;
        server_seq: number;
        sender_id: string;
        monotonic_seq: number;
    } | null;
};

export type SentCommand = { action: string; media_position_ms: number; monotonic_seq: number };

async function spaNavigate(page: Page, path: string): Promise<void> {
    await page.evaluate((to) => {
        window.history.pushState({}, "", to);
        window.dispatchEvent(new PopStateEvent("popstate"));
    }, path);
}

/** Enter `room` as `localUserId` with a real media file loaded in the player,
 *  and wait until the browser has decoded its metadata. */
export async function mountRoomWithRealVideo(
    page: Page,
    room: RoomFixture,
    localUserId: string,
    media: { path?: string; contentType?: string; urlPath?: string } = {},
): Promise<void> {
    const path = media.path ?? CLIP_10S;
    const urlPath = media.urlPath ?? "/test/clip.webm";
    await serveMedia(page, `**${urlPath}`, path, media.contentType ?? "video/webm");
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
    await page.evaluate((src) => {
        const w = window as unknown as { __locastStore: { setMediaSrc: (s: string) => void } };
        w.__locastStore.setMediaSrc(src);
    }, urlPath);
    await page.waitForFunction(
        () => {
            const v = document.querySelector<HTMLVideoElement>('[data-testid="locast-player-video"]');
            return v !== null && v.readyState >= 1 && Number.isFinite(v.duration);
        },
        undefined,
        { timeout: 15_000 },
    );
}

/** Every `playback_send` the app made, in order. */
export async function sentCommands(page: Page): Promise<SentCommand[]> {
    return await page.evaluate(() => {
        const w = window as unknown as {
            __locast_invoke_log?: Array<{ name: string; args: { cmd: SentCommand } }>;
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

/** A user gesture on the native controls: the element's own API, unmarked
 *  (the app marks only its own moves). */
export async function userGesture(page: Page, body: string): Promise<void> {
    await page.evaluate(async (code) => {
        const v = document.querySelector<HTMLVideoElement>('[data-testid="locast-player-video"]')!;
        const run = new Function("v", `return (async () => { ${code} })();`);
        await run(v);
    }, body);
}

/** Read a property of the real <video>. */
export async function videoState(
    page: Page,
): Promise<{ currentTime: number; paused: boolean; duration: number; ended: boolean }> {
    return await page.evaluate(() => {
        const v = document.querySelector<HTMLVideoElement>('[data-testid="locast-player-video"]')!;
        return { currentTime: v.currentTime, paused: v.paused, duration: v.duration, ended: v.ended };
    });
}
