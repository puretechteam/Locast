// Layout regression coverage for the redesigned shell and screens.
//
// Every screen is mounted in the Vite harness at three desktop window sizes and
// checked for: no horizontal overflow, the primary action still on screen, and
// one `<h1>`. Set LOCAST_UI_SHOTS=1 to also write a screenshot of each state to
// test-results/ui-shots for manual inspection. The assertions are structural
// (overflow, visibility, roles); none depends on exact pixel positions.

import { mkdirSync } from "node:fs";
import { join } from "node:path";
import { test, expect, injectLocastShim } from "./fixtures/vite-app";
import type { Page } from "@playwright/test";
import { HOST_ID, HOST_ROOM, VIEWER_ID, VIEWER_ROOM, mountRoomWithRealVideo } from "./fixtures/real-video-room";

const SIZES = [
    { width: 1280, height: 720 },
    { width: 1600, height: 900 },
    { width: 900, height: 650 },
    // The window's configured minimum (tauri.conf.json minWidth / minHeight).
    { width: 800, height: 600 },
] as const;

const SHOT_DIR = join(process.cwd(), "test-results", "ui-shots");
const SHOTS = process.env["LOCAST_UI_SHOTS"] === "1";

interface Item {
    id: string;
    sha256: string;
    filename: string;
    size_bytes: number;
    duration_ms: number | null;
    width: number | null;
    height: number | null;
    video_codec: string | null;
    audio_codec: string | null;
    container: string | null;
    status: "permanent" | "temporary";
    created_at: number;
}

function item(n: number, over: Partial<Item> = {}): Item {
    return {
        id: `item-${n}`,
        sha256: String(n).padStart(64, "0"),
        filename: `File ${n}.mkv`,
        size_bytes: 700_000_000 * n,
        duration_ms: 5_400_000 + n * 60_000,
        width: 1920,
        height: 1080,
        video_codec: "h264",
        audio_codec: "aac",
        container: "matroska",
        status: "permanent",
        created_at: 1_700_000_000_000 + n,
        ...over,
    };
}

const LIBRARY: Item[] = [
    item(1, { filename: "Spirited Away (2001) [1080p] [BluRay] [5.1] [YTS.MX] - an exceedingly long file name that must not break the row layout.mkv" }),
    item(2, { filename: "Holiday.mp4", container: "mov,mp4,m4a,3gp,3g2,mj2", status: "temporary" }),
    item(3, { filename: "Concert.webm", duration_ms: null, width: null, height: null, video_codec: null, container: null }),
    ...Array.from({ length: 6 }, (_, i) => item(i + 4)),
];

async function shot(page: Page, name: string): Promise<void> {
    if (!SHOTS) return;
    mkdirSync(SHOT_DIR, { recursive: true });
    const vp = page.viewportSize();
    await page.screenshot({ path: join(SHOT_DIR, `${name}-${vp?.width}x${vp?.height}.png`), animations: "disabled" });
}

async function noHorizontalOverflow(page: Page): Promise<void> {
    const overflow = await page.evaluate(() => {
        const el = document.documentElement;
        // Report the widest element too, so a failure names the culprit.
        let culprit = "";
        for (const node of document.body.querySelectorAll<HTMLElement>("*")) {
            const r = node.getBoundingClientRect();
            if (r.width > 0 && r.right > el.clientWidth + 1 && getComputedStyle(node).position !== "fixed") {
                culprit = `${node.tagName}.${node.className} right=${Math.round(r.right)}`;
                break;
            }
        }
        return { over: el.scrollWidth - el.clientWidth, culprit };
    });
    expect(overflow.over, overflow.culprit).toBeLessThanOrEqual(0);
}

async function seedLibrary(page: Page, items: Item[], failList = false): Promise<void> {
    await injectLocastShim(page);
    await page.addInitScript(
        (seed) => {
            (window as unknown as { __locast_library: unknown }).__locast_library = {
                items: seed.items,
                failList: seed.failList,
                nextPick: null,
                imported: [],
                serial: 1,
            };
        },
        { items, failList },
    );
}

async function stubRecentRooms(page: Page, rows: unknown[]): Promise<void> {
    await page.addInitScript((seed) => {
        const w = window as unknown as {
            __TAURI_INTERNALS__: {
                invoke: (name: string, args?: unknown, options?: unknown) => Promise<unknown>;
            };
        };
        const original = w.__TAURI_INTERNALS__.invoke;
        w.__TAURI_INTERNALS__.invoke = (name, args, options) =>
            name === "recent_rooms_list" ? Promise.resolve(seed) : original(name, args, options);
    }, rows);
}

for (const size of SIZES) {
    test.describe(`${size.width}x${size.height}`, () => {
        test.use({ viewport: size });

        test("empty library: one Import action, balanced empty state", async ({ page }) => {
            await seedLibrary(page, []);
            await page.goto("/library");
            await expect(page.getByTestId("library-empty")).toBeVisible();
            await expect(page.getByRole("button", { name: "Import files" })).toBeVisible();
            await expect(page.getByRole("heading", { level: 1 })).toHaveText("Library");
            await noHorizontalOverflow(page);
            await shot(page, "library-empty");
        });

        test("populated library: rows reflow, long names truncate, actions stay reachable", async ({ page }) => {
            await seedLibrary(page, LIBRARY);
            await page.goto("/library");
            await expect(page.getByTestId("library-tile")).toHaveCount(LIBRARY.length);
            await expect(page.getByRole("button", { name: "Import files" })).toBeVisible();
            const first = page.getByTestId("library-tile").first();
            await expect(first.getByRole("button", { name: /^Play / })).toBeVisible();
            await noHorizontalOverflow(page);
            await shot(page, "library-populated");
        });

        test("library: no search matches is distinct from an empty library", async ({ page }) => {
            await seedLibrary(page, LIBRARY);
            await page.goto("/library");
            await page.getByRole("searchbox", { name: "Search library" }).fill("zzzz");
            await expect(page.getByTestId("library-no-matches")).toBeVisible();
            await expect(page.getByTestId("library-empty")).toHaveCount(0);
            await noHorizontalOverflow(page);
            await shot(page, "library-no-matches");
            await page.getByRole("button", { name: "Clear search" }).click();
            await expect(page.getByTestId("library-tile")).toHaveCount(LIBRARY.length);
        });

        test("library: load failure shows a recoverable error", async ({ page }) => {
            await seedLibrary(page, [], true);
            await page.goto("/library");
            await expect(page.getByRole("alert")).toContainText("Could not load your library");
            await expect(page.getByRole("button", { name: "Try again" })).toBeVisible();
            await noHorizontalOverflow(page);
            await shot(page, "library-error");
        });

        test("library: delete confirmation and import progress stay inside the window", async ({ page }) => {
            await seedLibrary(page, LIBRARY);
            await page.addInitScript(() => {
                const w = window as unknown as {
                    __TAURI_INTERNALS__: {
                        invoke: (name: string, args?: unknown, options?: unknown) => Promise<unknown>;
                    };
                    __locast_library: { nextPick: string[] | null };
                };
                w.__locast_library.nextPick = ["C:\\media\\New Movie.mkv"];
                const original = w.__TAURI_INTERNALS__.invoke;
                // Hold the import open so the in-progress state can be inspected.
                w.__TAURI_INTERNALS__.invoke = (name, args, options) =>
                    name === "media_import"
                        ? new Promise((resolve) => {
                              window.setTimeout(() => resolve(original(name, args, options)), 1500);
                          })
                        : original(name, args, options);
            });
            await page.goto("/library");
            const first = page.getByTestId("library-tile").first();
            await first.getByRole("button", { name: /^Delete / }).click();
            await expect(first.getByRole("button", { name: "Confirm delete" })).toBeVisible();
            await expect(first.getByRole("button", { name: "Cancel" })).toBeVisible();
            await noHorizontalOverflow(page);
            await shot(page, "library-confirm-delete");
            await first.getByRole("button", { name: "Cancel" }).click();

            await page.getByRole("button", { name: "Import files" }).click();
            await expect(page.getByRole("button", { name: "Importing..." })).toBeDisabled();
            await expect(page.getByText("Importing your files.")).toBeVisible();
            await noHorizontalOverflow(page);
            await shot(page, "library-importing");
            await expect(page.getByTestId("library-notice")).toBeVisible();
            await expect(page.getByText("Importing your files.")).toHaveCount(0);
        });

        test("library: a failing search keeps the search box so it can be corrected", async ({ page }) => {
            await seedLibrary(page, LIBRARY);
            await page.addInitScript(() => {
                const w = window as unknown as {
                    __TAURI_INTERNALS__: {
                        invoke: (name: string, args?: unknown, options?: unknown) => Promise<unknown>;
                    };
                };
                const original = w.__TAURI_INTERNALS__.invoke;
                w.__TAURI_INTERNALS__.invoke = (name, args, options) => {
                    const q = (args as { query?: string | null } | undefined)?.query;
                    return name === "library_list" && q === "boom"
                        ? Promise.reject({ kind: "Other", message: "search index unavailable" })
                        : original(name, args, options);
                };
            });
            await page.goto("/library");
            const search = page.getByRole("searchbox", { name: "Search library" });
            await search.fill("boom");
            await expect(page.getByRole("alert")).toContainText("search index unavailable");
            await expect(search).toBeVisible();
            await search.fill("File");
            await expect(page.getByRole("alert")).toHaveCount(0);
            await expect(page.getByTestId("library-tile").first()).toBeVisible();
        });

        test("rooms: no active or recent rooms", async ({ page }) => {
            await injectLocastShim(page);
            await stubRecentRooms(page, []);
            await page.goto("/rooms");
            await expect(page.getByRole("heading", { level: 1 })).toHaveText("Rooms");
            await expect(page.getByRole("link", { name: "Create a room" })).toBeVisible();
            await expect(page.getByRole("link", { name: "Join a room" })).toBeVisible();
            await expect(page.getByText("You are not in a room")).toBeVisible();
            await noHorizontalOverflow(page);
            await shot(page, "rooms-empty");
        });

        test("rooms: active and recent rooms with real data", async ({ page }) => {
            await injectLocastShim(page);
            await page.addInitScript(() => {
                const w = window as unknown as {
                    __TAURI_INTERNALS__: {
                        invoke: (name: string, args?: unknown, options?: unknown) => Promise<unknown>;
                    };
                };
                const original = w.__TAURI_INTERNALS__.invoke;
                const now = Date.now();
                w.__TAURI_INTERNALS__.invoke = (name, args, options) => {
                    if (name === "recent_rooms_list") {
                        return Promise.resolve([
                            {
                                room_id: "r-1",
                                code: "ABC234",
                                title: "Friday movie night with an extremely long title that needs to truncate gracefully",
                                host_user_id: "h1",
                                host_display_name: "Sam",
                                role: "guest",
                                last_seen_ms: now - 3_600_000,
                                last_ended_ms: null,
                                created_ms: now - 7_200_000,
                            },
                            {
                                room_id: "r-2",
                                code: "XYZ789",
                                title: "Anime marathon",
                                host_user_id: "h2",
                                host_display_name: "You",
                                role: "host",
                                last_seen_ms: now - 86_400_000,
                                last_ended_ms: now - 80_000_000,
                                created_ms: now - 90_000_000,
                            },
                        ]);
                    }
                    return original(name, args, options);
                };
            });
            await page.goto("/rooms");
            await expect(page.getByText("Friday movie night")).toBeVisible();
            await expect(page.getByRole("link", { name: "Join again" })).toHaveCount(1);
            await expect(page.getByText("Ended", { exact: true })).toBeVisible();
            await noHorizontalOverflow(page);
            await shot(page, "rooms-recent");
        });

        test("create room form", async ({ page }) => {
            await injectLocastShim(page);
            await page.goto("/rooms/new");
            await expect(page.getByRole("heading", { level: 1 })).toHaveText("Create a room");
            const create = page.getByRole("button", { name: /create/i });
            await expect(create).toBeDisabled();
            await page.getByLabel("Room title").fill("Movie night");
            await expect(create).toBeEnabled();
            await noHorizontalOverflow(page);
            await shot(page, "create-room");
        });

        test("join room form, with a prefilled code from a recent room", async ({ page }) => {
            await injectLocastShim(page);
            await page.goto("/rooms/join?code=abc234");
            await expect(page.getByLabel("Room code")).toHaveValue("ABC234");
            await expect(page.getByRole("button", { name: "Join room" })).toBeDisabled();
            await page.getByLabel("Display name").fill("Alex");
            await expect(page.getByRole("button", { name: "Join room" })).toBeEnabled();
            await noHorizontalOverflow(page);
            await shot(page, "join-room");
        });

        test("settings", async ({ page }) => {
            await injectLocastShim(page);
            await page.addInitScript(() => {
                const w = window as unknown as {
                    __TAURI_INTERNALS__: {
                        invoke: (name: string, args?: unknown, options?: unknown) => Promise<unknown>;
                    };
                };
                const original = w.__TAURI_INTERNALS__.invoke;
                w.__TAURI_INTERNALS__.invoke = (name, args, options) => {
                    if (name === "settings_get_server") {
                        return Promise.resolve({
                            configured_url: "wss://next.example.com/ws",
                            active_url: "ws://127.0.0.1:8787/ws",
                            next_url: "wss://next.example.com/ws",
                            env_override: false,
                        });
                    }
                    return original(name, args, options);
                };
            });
            await page.goto("/settings");
            await expect(page.getByTestId("settings-active-url")).toHaveText("ws://127.0.0.1:8787/ws");
            await expect(page.getByTestId("settings-restart")).toContainText("wss://next.example.com/ws");
            await noHorizontalOverflow(page);
            await shot(page, "settings");
        });

        test("not found keeps the shell and a way back", async ({ page }) => {
            await injectLocastShim(page);
            await page.goto("/nowhere");
            await expect(page.getByRole("link", { name: "Back to library" })).toBeVisible();
            await expect(page.getByRole("navigation", { name: "Primary" })).toBeVisible();
            await noHorizontalOverflow(page);
        });

        test("room as host: video stays central, controls and sidebar fit", async ({ page }) => {
            await injectLocastShim(page);
            await page.addInitScript(() => {
                const w = window as unknown as { __locast_library?: unknown };
                w.__locast_library = { items: [], failList: false, nextPick: null, imported: [], serial: 1 };
            });
            await page.goto("/");
            await mountRoomWithRealVideo(page, HOST_ROOM, HOST_ID);
            await expect(page.getByTestId("locast-playback-play")).toBeVisible();
            await expect(page.getByRole("button", { name: "Leave" })).toBeVisible();
            const video = await page.getByTestId("locast-player-video").boundingBox();
            expect(video).not.toBeNull();
            // The picture is the largest thing on screen.
            expect(video!.width).toBeGreaterThan(size.width * 0.45);
            await noHorizontalOverflow(page);
            await shot(page, "room-host");
        });

        test("room as viewer", async ({ page }) => {
            await injectLocastShim(page);
            await page.goto("/");
            await mountRoomWithRealVideo(page, VIEWER_ROOM, VIEWER_ID);
            await expect(page.getByTestId("locast-playback-play")).toBeDisabled();
            await noHorizontalOverflow(page);
            await shot(page, "room-viewer");
        });

        test("dialogs fit the window and trap focus", async ({ page }) => {
            await injectLocastShim(page);
            await page.goto("/");
            await mountRoomWithRealVideo(page, HOST_ROOM, HOST_ID);
            await page.getByRole("button", { name: "Leave" }).click();
            const dialog = page.getByRole("dialog");
            await expect(dialog).toBeVisible();
            const box = await dialog.boundingBox();
            expect(box!.x).toBeGreaterThanOrEqual(0);
            expect(box!.y).toBeGreaterThanOrEqual(0);
            expect(box!.x + box!.width).toBeLessThanOrEqual(size.width);
            expect(box!.y + box!.height).toBeLessThanOrEqual(size.height);
            // Tab never leaves the dialog.
            for (let i = 0; i < 8; i++) {
                await page.keyboard.press("Tab");
                const inside = await page.evaluate(() => document.activeElement?.closest("dialog") !== null);
                expect(inside).toBe(true);
            }
            await shot(page, "dialog-leave");
            await page.keyboard.press("Escape");
            await expect(dialog).toHaveCount(0);
            await page.getByRole("button", { name: "Permissions" }).click();
            await expect(page.getByRole("dialog")).toBeVisible();
            await shot(page, "dialog-permissions");
        });

        test("busy room: people, chat and shared media share the screen without overflow", async ({ page, locast }) => {
            await injectLocastShim(page);
            await page.goto("/");
            const person = (id: string, name: string, status: string, host = false) => ({
                user_id: id,
                display_name: name,
                joined_ms: 1_700_000_000_000,
                status,
                last_seen_ms: 1_700_000_000_000,
                is_host: host,
                cap_set: 0,
            });
            const room = {
                ...HOST_ROOM,
                id: "r-busy",
                title: "Friday movie night with an extremely long room title that should truncate cleanly",
                you_cap_set: 0xff,
                participants: [
                    person(HOST_ID, "Taylor", "Connected", true),
                    person("u-2", "Alexandria Montgomery-Featherstone", "Connected"),
                    person("u-3", "Sam", "Reconnecting"),
                    person("u-4", "Jordan", "Disconnected"),
                ],
            };
            await mountRoomWithRealVideo(page, room as unknown as typeof HOST_ROOM, HOST_ID);
            await page.evaluate(() => {
                const w = window as unknown as {
                    __locastSharedMediaStore: {
                        getState: () => {
                            setIsHost: (b: boolean) => void;
                            setManifest: (m: unknown) => void;
                        };
                    };
                };
                const st = w.__locastSharedMediaStore.getState();
                st.setIsHost(true);
                st.setManifest({
                    room_id: "r-busy",
                    version: 1,
                    media: [
                        {
                            id: "m1",
                            filename: "Spirited Away (2001) [1080p] [BluRay] - a very long filename that must not push the layout wider.mkv",
                            size_bytes: 3_221_225_472,
                            mime: "video/x-matroska",
                            sha256: "a".repeat(64),
                        },
                    ],
                });
            });
            const msg = (sender: string, name: string, text: string, t: number) => ({
                room_id: "r-busy",
                sender_id: sender,
                sender_name: name,
                text,
                reply_to: null,
                ts_ms: 1_700_000_000_000 + t,
            });
            await locast.emitChatMessage(msg("u-2", "Alexandria Montgomery-Featherstone", "Is everyone ready? I have the popcorn.", 1000));
            await locast.emitChatMessage(msg(HOST_ID, "Taylor", "Starting in a minute, grab your seats.", 2000));
            await locast.emitChatMessage(msg("u-3", "Sam", "averyveryveryveryveryveryveryveryveryverylongwordwithoutanyspaces", 3000));
            await expect(page.getByTestId("chat-message")).toHaveCount(3);
            await expect(page.getByTestId("room-media-item")).toHaveCount(1);
            await expect(page.getByRole("button", { name: "Leave" })).toBeVisible();
            await noHorizontalOverflow(page);
            await shot(page, "room-busy");
        });

        test("download dialog shows progress and blocks the room", async ({ page, locast }) => {
            await injectLocastShim(page);
            await page.goto("/");
            await mountRoomWithRealVideo(page, VIEWER_ROOM, VIEWER_ID);
            await locast.emitDownloadState({ id: "dl-1", media_id: "m-1", state: "transferring" });
            await locast.emitDownloadProgress({
                id: "dl-1",
                state: "transferring",
                transferred_bytes: 1_400_000_000,
                total_bytes: 3_200_000_000,
                bytes_per_sec_ema: 48_000_000,
                eta_seconds: 38,
            });
            await expect(page.getByTestId("dlm-dialog")).toBeVisible();
            await expect(page.getByTestId("dlm-progress")).toHaveAttribute("aria-valuenow", "44");
            await shot(page, "dialog-download");
            await locast.emitDownloadState({ id: "dl-1", media_id: "m-1", state: "failed", error_message: "disk full: the destination drive has no space left for this file" });
            await expect(page.getByTestId("dlm-error")).toContainText("disk full");
            await shot(page, "dialog-download-failed");
        });
    });
}

test("navigation links are real links and mark the current page", async ({ page }) => {
    await seedLibrary(page, LIBRARY);
    await stubRecentRooms(page, []);
    await page.goto("/library");
    const nav = page.getByRole("navigation", { name: "Primary" });
    await expect(nav.getByRole("link", { name: "Library" })).toHaveAttribute("aria-current", "page");
    await nav.getByRole("link", { name: "Rooms" }).click();
    await expect(page).toHaveURL(/\/rooms$/);
    await expect(nav.getByRole("link", { name: "Rooms" })).toHaveAttribute("aria-current", "page");
    await page.getByRole("link", { name: "Settings" }).click();
    await expect(page).toHaveURL(/\/settings$/);
    await page.getByRole("link", { name: "Back to library" }).click();
    await expect(page).toHaveURL(/\/library$/);
});
