// Slice 3: host media sharing and viewer download, as the room UI sees it.
//
// What this spec proves: the React layer calls the right commands with the
// right arguments (`manifest_publish` with the chosen item, `download_open`
// per shared item, `room_join` with the invite), reacts to the real event
// names (`manifest://state`, `download://state`, `download://progress`),
// keeps the blocking modal up while a download is active, skips it on a
// dedup hit, and hands the finished item to the P1-T10 player.
//
// What it does NOT prove: the Rust side. Signature / trust-anchor checks,
// dedup, the WebRTC transfer, chunk and BLAKE3 verification and the atomic
// install are covered by the Rust tests and the real host/viewer smoke
// (`cargo test -j 1 -p locast-client --test smoke_host_viewer -- --ignored`).
// Here a small fake backend stands in for those commands.

import { test, expect, injectLocastShim } from "./fixtures/vite-app";
import type { Page } from "@playwright/test";

const HOST_ID = "00000000-0000-7000-8000-0000000000a1";
const VIEWER_ID = "00000000-0000-7000-8000-0000000000b2";
const ROOM_ID = "00000000-0000-7000-8000-0000000000c3";
const INVITE = "locast://join/ABCDEF?h=11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo&v=1";
const DLG = '[data-testid="dlm-dialog"]';

interface LibItem {
    id: string;
    sha256: string;
    filename: string;
    size_bytes: number;
    duration_ms: null;
    width: null;
    height: null;
    video_codec: null;
    audio_codec: null;
    container: null;
    status: "permanent" | "temporary";
    created_at: number;
}

function libItem(id: string, filename: string, sha: string): LibItem {
    return {
        id,
        sha256: sha.padEnd(64, "0"),
        filename,
        size_bytes: 3 * 1024 * 1024,
        duration_ms: null,
        width: null,
        height: null,
        video_codec: null,
        audio_codec: null,
        container: null,
        status: "permanent",
        created_at: 1_700_000_000_000,
    };
}

function summary(you: string) {
    return {
        id: ROOM_ID,
        code: "ABCDEF",
        title: "Movie night",
        host_user_id: HOST_ID,
        host_migration_enabled: false,
        created_ms: 1,
        participants: [
            { user_id: HOST_ID, display_name: "Host", joined_ms: 1, status: "Connected", last_seen_ms: 1, is_host: true },
            { user_id: VIEWER_ID, display_name: "Viewer", joined_ms: 2, status: "Connected", last_seen_ms: 2, is_host: false },
        ],
        host_disconnected: false,
        host_disconnect_deadline_ms: null,
        you_cap_set: 0,
        you_user_id: you,
    };
}

interface DownloadReply {
    reply: {
        download_id: string;
        media_id: string;
        state: string;
        dedup_hit: boolean;
        total_bytes: number;
        transferred_bytes: number;
        on_disk_path: string | null;
        transfer_started: boolean;
    };
    /** The `download://state` Rust emits for this outcome. */
    emit: { id: string; media_id: string; state: string } | null;
}

interface BackendConfig {
    summary: ReturnType<typeof summary> | null;
    library: LibItem[];
    downloadReplies?: DownloadReply[];
    fetchError?: string | null;
}

/** Wrap the harness invoke with a fake for the slice-3 commands. Runs as
 * an init script after the harness shim, so even the first mount sees it. */
async function setup(page: Page, cfg: BackendConfig): Promise<void> {
    await injectLocastShim(page);
    await page.addInitScript((c: BackendConfig) => {
        // eslint-disable-next-line @typescript-eslint/no-explicit-any -- in-page shim over the untyped Tauri IPC surface
        const w = window as unknown as Record<string, any>;
        w.__locast_library = { items: c.library, failList: false, nextPick: null, imported: [], serial: 1 };
        const be = (w.__slice3 = {
            log: [] as Array<{ name: string; args: unknown }>,
            summary: c.summary,
            manifest: null as unknown,
            downloadReplies: c.downloadReplies ?? [],
            fetchError: c.fetchError ?? null,
        });
        const emit = (name: string, payload: unknown) =>
            import("/tests/playwright/shim/tauriShim.ts" as string).then((m) => m.__emit(name, payload));
        w.__slice3_publish = (manifest: { version: number }) => {
            be.manifest = manifest;
            return emit("manifest://state", { room_id: ROOM, manifest_hash: "h", version: manifest.version });
        };
        const ROOM = c.summary?.id ?? "";
        const orig = w.__TAURI_INTERNALS__.invoke;
        // eslint-disable-next-line @typescript-eslint/no-explicit-any -- in-page shim over the untyped Tauri IPC surface
        w.__TAURI_INTERNALS__.invoke = (name: string, args: any) => {
            const record = () => be.log.push({ name, args });
            switch (name) {
                case "room_get_state":
                    return Promise.resolve(be.summary);
                case "room_invite_url":
                    record();
                    return Promise.resolve("locast://join/ABCDEF?h=11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo&v=1");
                case "manifest_current":
                    return Promise.resolve(be.manifest);
                case "manifest_fetch":
                    record();
                    return be.fetchError !== null
                        ? Promise.reject({ kind: "Other", message: be.fetchError })
                        : Promise.reject({ kind: "Other", message: "room error: InvalidState" });
                case "manifest_publish": {
                    record();
                    const ids: string[] = args.mediaIds;
                    const items = w.__locast_library.items.filter((i: LibItem) => ids.includes(i.id));
                    const version = ((be.manifest as { version?: number } | null)?.version ?? 0) + 1;
                    return w
                        .__slice3_publish({
                            room_id: ROOM,
                            version,
                            media: items.map((i: LibItem) => ({
                                id: i.id,
                                filename: i.filename,
                                size_bytes: i.size_bytes,
                                mime: "video/mp4",
                                sha256: i.sha256,
                            })),
                        })
                        .then(() => null);
                }
                case "download_open": {
                    record();
                    const next = be.downloadReplies.shift();
                    if (next === undefined) return Promise.reject({ kind: "Other", message: "unexpected download_open" });
                    const done = next.emit === null ? Promise.resolve() : emit("download://state", { v: 1, error_message: null, ...next.emit });
                    return done.then(() => next.reply);
                }
                case "room_join":
                case "room_connect_signaling":
                    record();
                    return Promise.resolve(name === "room_join" ? be.summary : null);
                default:
                    return orig(name, args);
            }
        };
    }, cfg);
}

async function calls(page: Page, name: string): Promise<unknown[]> {
    return await page.evaluate(
        (n) =>
            (window as unknown as { __slice3: { log: Array<{ name: string; args: unknown }> } }).__slice3.log
                .filter((e) => e.name === n)
                .map((e) => e.args),
        name,
    );
}

async function hostShares(page: Page, item: LibItem): Promise<void> {
    await page.evaluate((i) => {
        const w = window as unknown as { __slice3_publish: (m: unknown) => Promise<void> };
        return w.__slice3_publish({
            room_id: "00000000-0000-7000-8000-0000000000c3",
            version: 1,
            media: [{ id: i.id, filename: i.filename, size_bytes: i.size_bytes, mime: "video/mp4", sha256: i.sha256 }],
        });
    }, item);
}

const video = (page: Page) => page.locator('[data-testid="locast-player-video"]');
const mediaItem = (page: Page) => page.getByTestId("room-media-item");

test("host picks a library item, shares it, sees it shared and can play it", async ({ page }) => {
    const a = libItem("host-a", "Alpha.mp4", "aa11");
    const b = libItem("host-b", "Bravo.mp4", "bb22");
    await setup(page, { summary: summary(HOST_ID), library: [a, b] });
    await page.goto(`/rooms/${ROOM_ID}`);

    await expect(page.getByTestId("room-media-host")).toBeVisible();
    await expect(page.getByTestId("room-media-invite")).toHaveValue(INVITE);
    await expect(page.getByTestId("room-media-empty")).toHaveText("Nothing shared yet.");

    await page.getByTestId("room-media-select").selectOption("host-b");
    await page.getByTestId("room-media-share").click();

    expect(await calls(page, "manifest_publish")).toEqual([{ mediaIds: ["host-b"] }]);
    await expect(mediaItem(page)).toHaveCount(1);
    await expect(mediaItem(page)).toHaveAttribute("data-media-id", "host-b");
    await expect(mediaItem(page)).toContainText("Bravo.mp4");
    await expect(mediaItem(page)).toContainText("Shared with the room");
    await expect(page.getByTestId("room-media-share")).toHaveText("Shared");
    // The host never downloads its own media.
    expect(await calls(page, "download_open")).toEqual([]);

    await page.getByTestId("room-media-play").click();
    await expect(video(page)).toHaveAttribute(
        "src",
        `http://locast.localhost/media/${b.sha256.slice(0, 16)}/Bravo.mp4`,
    );
});

test("a viewer gets no host controls", async ({ page }) => {
    await setup(page, { summary: summary(VIEWER_ID), library: [] });
    await page.goto(`/rooms/${ROOM_ID}`);
    await expect(page.getByTestId("room-media")).toBeVisible();
    await expect(page.getByTestId("room-media-host")).toHaveCount(0);
    await expect(page.getByTestId("room-media-empty")).toHaveText("Waiting for the host to share media.");
    expect(await calls(page, "room_invite_url")).toEqual([]);
    expect(await calls(page, "manifest_publish")).toEqual([]);
});

test("viewer that already has the media: dedup hit, no transfer, no modal, playable", async ({ page }) => {
    const shared = libItem("host-a", "Alpha.mp4", "aa11");
    const mine = { ...libItem("viewer-copy", "Alpha.mp4", "aa11") };
    await setup(page, {
        summary: summary(VIEWER_ID),
        library: [mine],
        downloadReplies: [
            {
                reply: {
                    download_id: "dl-dedup",
                    media_id: "viewer-copy",
                    state: "complete",
                    dedup_hit: true,
                    total_bytes: shared.size_bytes,
                    transferred_bytes: shared.size_bytes,
                    on_disk_path: "C:/lib/library/aa/11/x/Alpha.mp4",
                    transfer_started: false,
                },
                emit: { id: "dl-dedup", media_id: "viewer-copy", state: "complete" },
            },
        ],
    });
    await page.goto(`/rooms/${ROOM_ID}`);
    await expect(page.getByTestId("room-media-empty")).toBeVisible();

    await hostShares(page, shared);

    await expect(page.getByTestId("room-media-status")).toHaveText("Already in your library");
    await expect(page.locator(DLG)).toHaveCount(0);
    expect(await calls(page, "download_open")).toEqual([{ mediaId: "host-a" }]);

    await page.getByTestId("room-media-play").click();
    await expect(video(page)).toHaveAttribute(
        "src",
        `http://locast.localhost/media/${mine.sha256.slice(0, 16)}/Alpha.mp4`,
    );
});

test("viewer without the media: waits for a source, downloads behind the modal, then plays", async ({
    page,
    locast,
}) => {
    const shared = libItem("host-a", "Alpha.mp4", "aa11");
    // What Rust inserts for the downloaded file (same id as the manifest
    // entry; status temporary).
    const installed = { ...shared, status: "temporary" as const };
    const pending = {
        download_id: "dl-1",
        media_id: "host-a",
        state: "pending",
        dedup_hit: false,
        total_bytes: shared.size_bytes,
        transferred_bytes: 0,
        on_disk_path: null,
    };
    await setup(page, {
        summary: summary(VIEWER_ID),
        library: [installed],
        downloadReplies: [
            // No source DataChannel open yet (Rust leaves the row pending).
            { reply: { ...pending, transfer_started: false }, emit: { id: "dl-1", media_id: "host-a", state: "pending" } },
            // Retry on the same row: the transfer starts.
            { reply: { ...pending, transfer_started: true }, emit: { id: "dl-1", media_id: "host-a", state: "pending" } },
        ],
    });
    await page.goto(`/rooms/${ROOM_ID}`);
    await expect(page.getByTestId("room-media-empty")).toBeVisible();

    await hostShares(page, shared);

    // The pending download blocks the room behind the modal.
    await expect(page.locator(DLG)).toBeVisible();
    await expect(page.getByTestId("room-media")).toHaveCount(0);
    // The bridge (outside the guard) retries until the transfer starts.
    await expect.poll(async () => (await calls(page, "download_open")).length, { timeout: 5000 }).toBe(2);
    expect(await calls(page, "download_open")).toEqual([{ mediaId: "host-a" }, { mediaId: "host-a" }]);

    await locast.emitDownloadState({ id: "dl-1", media_id: "host-a", state: "transferring" });
    await locast.emitDownloadProgress({
        id: "dl-1",
        state: "transferring",
        transferred_bytes: shared.size_bytes / 2,
        total_bytes: shared.size_bytes,
        bytes_per_sec_ema: 1024 * 1024,
        eta_seconds: 2,
    });
    await expect(page.getByTestId("dlm-progress")).toHaveAttribute("aria-valuenow", "50");
    await page.keyboard.press("Escape");
    await expect(page.locator(DLG)).toBeVisible();

    await locast.emitDownloadState({ id: "dl-1", media_id: "host-a", state: "verifying" });
    await expect(page.locator(DLG)).toBeVisible();
    await locast.emitDownloadState({ id: "dl-1", media_id: "host-a", state: "complete" });

    await expect(page.locator(DLG)).toHaveCount(0);
    await expect(page.getByTestId("room-media-status")).toHaveText("Downloaded to your library");
    await page.getByTestId("room-media-play").click();
    await expect(video(page)).toHaveAttribute(
        "src",
        `http://locast.localhost/media/${installed.sha256.slice(0, 16)}/Alpha.mp4`,
    );
    // No further transfer was opened after completion.
    expect((await calls(page, "download_open")).length).toBe(2);
});

test("a manifest Rust rejects as untrusted is never downloaded", async ({ page }) => {
    await setup(page, {
        summary: summary(VIEWER_ID),
        library: [],
        fetchError: "manifest rejected: no trust anchor installed (set_expected_host_pubkey)",
    });
    await page.goto(`/rooms/${ROOM_ID}`);
    await expect(page.getByTestId("room-media-empty")).toHaveText(
        "The shared media could not be verified. Join with the host's invite link to receive it.",
    );
    expect((await calls(page, "manifest_fetch")).length).toBe(1);
    expect(await calls(page, "download_open")).toEqual([]);
    await expect(page.locator(DLG)).toHaveCount(0);
});

test("joining with an invite link fills the code and passes the link to room_join", async ({ page }) => {
    await setup(page, { summary: summary(VIEWER_ID), library: [] });
    await page.goto("/rooms/join");
    await page.getByTestId("join-invite").fill(INVITE);
    await expect(page.getByRole("textbox", { name: "Room code" })).toHaveValue("ABCDEF");
    await page.getByPlaceholder("Your name").fill("Viewer");
    await page.getByRole("button", { name: "Join" }).click();
    await expect(page).toHaveURL(new RegExp(`/rooms/${ROOM_ID}$`));
    expect(await calls(page, "room_join")).toEqual([
        { code: "ABCDEF", displayName: "Viewer", inviteUrl: INVITE },
    ]);
});
