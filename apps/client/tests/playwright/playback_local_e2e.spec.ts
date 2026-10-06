// P1-T10 acceptance (roadmap), the parts that can run in the Vite harness:
//   "plays a local mp4 from the library ... the player registers and listens
//    to a no-op `room://event` channel for the next phase."
//
// The harness has no Rust backend and no `locast://` handler, so it checks the
// frontend's half of the contract: Library -> Play resolves the item through
// `media_resolve_url`, maps the URL onto the platform's webview scheme, opens
// the player, requests that URL with a Range header, and keeps exactly one
// no-op `room://event` listener alive per mounted Player. The handler's own
// Range behaviour is covered by tests/protocol_playback.rs; actual audio and
// video playback needs the real desktop app (see docs/MANUAL_TEST_P1-T10.md).

import { test, expect, injectLocastShim } from "./fixtures/vite-app";
import type { Page } from "@playwright/test";

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

function item(n: number, filename = `File ${n}.mp4`): Item {
    return {
        id: `item-${n}`,
        sha256: `${String(n).padStart(2, "0")}abcdef0123456789`.padEnd(64, "0"),
        filename,
        size_bytes: 1_000_000 * n,
        duration_ms: null,
        width: null,
        height: null,
        video_codec: null,
        audio_codec: null,
        container: null,
        status: "permanent",
        created_at: 1_700_000_000_000 + n,
    };
}

const sha16 = (it: Item): string => it.sha256.slice(0, 16);

async function openLibrary(
    page: Page,
    items: Item[],
    opts: { schemeMode?: "windows" | "unix" } = {},
): Promise<void> {
    await injectLocastShim(page);
    await page.addInitScript(
        (seed) => {
            const w = window as unknown as Record<string, unknown>;
            w["__locast_library"] = {
                items: seed.items,
                failList: false,
                nextPick: null,
                imported: [],
                serial: 1,
            };
            w["__locast_scheme_mode"] = seed.mode;
        },
        { items, mode: opts.schemeMode ?? "windows" },
    );
    await page.goto("/library");
}

const video = (page: Page) => page.locator('[data-testid="locast-player-video"]');

async function listeners(page: Page): Promise<{ active: number; received: number }> {
    return await page.evaluate(() => {
        const s = (window as unknown as {
            __locastPlayerRoomEvents: { active: () => number; received: () => number };
        }).__locastPlayerRoomEvents;
        return { active: s.active(), received: s.received() };
    });
}

test("Play opens the local player on the webview URL for the library item (Windows form)", async ({
    page,
}) => {
    const it = item(1);
    await openLibrary(page, [it]);

    await page.getByRole("button", { name: "Play File 1.mp4" }).click();

    await expect(page).toHaveURL(/\/rooms\/local$/);
    await expect(page.getByTestId("room-local")).toContainText("File 1.mp4");
    await expect(video(page)).toHaveAttribute(
        "src",
        `http://locast.localhost/media/${sha16(it)}/File%201.mp4`,
    );
    // The native controls (play/pause and the seek bar) stay on.
    await expect(video(page)).toHaveJSProperty("controls", true);
});

test("on macOS and Linux the URL uses the locast://localhost form", async ({ page }) => {
    const it = item(2);
    await openLibrary(page, [it], { schemeMode: "unix" });
    await page.getByRole("button", { name: "Play File 2.mp4" }).click();
    await expect(video(page)).toHaveAttribute(
        "src",
        `locast://localhost/media/${sha16(it)}/File%202.mp4`,
    );
});

test("file names with spaces and non-ASCII characters stay percent-encoded", async ({ page }) => {
    const it = item(3, "Movie Night é #1.mp4");
    await openLibrary(page, [it]);
    await page.getByRole("button", { name: `Play ${it.filename}` }).click();
    await expect(video(page)).toHaveAttribute(
        "src",
        `http://locast.localhost/media/${sha16(it)}/Movie%20Night%20%C3%A9%20%231.mp4`,
    );
});

test("the media element asks the handler for the file with a Range header", async ({ page }) => {
    const it = item(4);
    const seen: Array<{ url: string; range: string | undefined }> = [];
    await page.route("http://locast.localhost/**", async (route) => {
        const req = route.request();
        seen.push({ url: req.url(), range: req.headers()["range"] });
        await route.fulfill({ status: 404, body: "not served in the harness" });
    });
    await openLibrary(page, [it]);
    await page.getByRole("button", { name: "Play File 4.mp4" }).click();

    await expect.poll(() => seen.length, { timeout: 10_000 }).toBeGreaterThan(0);
    expect(seen[0]?.url).toBe(`http://locast.localhost/media/${sha16(it)}/File%204.mp4`);
    // Chromium opens media with `Range: bytes=0-`; the handler caps the answer
    // and the element requests further slices as it plays or seeks.
    expect(seen[0]?.range).toMatch(/^bytes=0-/);
});

test("the player keeps exactly one no-op room://event listener across remounts", async ({
    page,
}) => {
    await openLibrary(page, [item(5)]);
    await page.getByRole("button", { name: "Play File 5.mp4" }).click();
    await expect(video(page)).toBeVisible();
    await expect.poll(async () => (await listeners(page)).active).toBe(1);

    // Leave the player: the listener is removed.
    await page.getByRole("link", { name: "Back to library" }).click();
    await expect(page.getByTestId("library-grid")).toBeVisible();
    await expect.poll(async () => (await listeners(page)).active).toBe(0);

    // Come back twice: still one listener, never two.
    for (let i = 0; i < 2; i++) {
        await page.getByRole("button", { name: "Play File 5.mp4" }).click();
        await expect(video(page)).toBeVisible();
        await expect.poll(async () => (await listeners(page)).active).toBe(1);
        await page.getByRole("link", { name: "Back to library" }).click();
        await expect(page.getByTestId("library-grid")).toBeVisible();
        await expect.poll(async () => (await listeners(page)).active).toBe(0);
    }
});

const ROOM = {
    id: "r-p1t10",
    code: "ABCD12",
    title: "P1-T10",
    host_user_id: "11111111-1111-1111-1111-111111111111",
    host_migration_enabled: true,
    created_ms: 1_700_000_000_000,
    participants: [],
    host_disconnected: false,
    host_disconnect_deadline_ms: null,
};

/** Join `ROOM` through the harness seam, then return to the library inside the SPA. */
async function joinRoomThenOpenLibrary(page: Page): Promise<void> {
    await page.evaluate(() => {
        window.history.pushState({}, "", "/rooms/r-p1t10");
        window.dispatchEvent(new PopStateEvent("popstate"));
    });
    await page.waitForFunction(
        () => (window as { __locastRoomStore?: unknown }).__locastRoomStore !== undefined,
    );
    await page.evaluate((room) => {
        const w = window as unknown as { __locastRoomStore: { setSummary: (s: unknown) => void } };
        w.__locastRoomStore.setSummary(room);
        window.history.pushState({}, "", "/library");
        window.dispatchEvent(new PopStateEvent("popstate"));
    }, ROOM);
}

test("a room://event is received and changes nothing about playback", async ({ page, locast }) => {
    const it = item(6);
    await openLibrary(page, [it]);
    await joinRoomThenOpenLibrary(page);
    await page.getByRole("button", { name: "Play File 6.mp4" }).click();
    await expect(page).toHaveURL(/\/rooms\/r-p1t10$/);
    await expect(video(page)).toBeVisible();
    await expect.poll(async () => (await listeners(page)).active).toBe(1);
    const snapshot = (): Promise<unknown> =>
        page.evaluate(async () => {
            const { usePlaybackStore } = await import("/src/stores/usePlaybackStore.ts");
            const v = document.querySelector<HTMLVideoElement>('[data-testid="locast-player-video"]');
            const st = usePlaybackStore.getState();
            return {
                src: v?.getAttribute("src") ?? null,
                paused: v?.paused ?? null,
                currentTime: v?.currentTime ?? null,
                lastApplied: st.lastApplied,
                mediaSrc: st.mediaSrc,
            };
        });
    const before = await snapshot();
    const receivedBefore = (await listeners(page)).received;

    // A same-room update, as the server sends for capability changes. The room
    // page applies it to its summary; the Player's listener must just observe it.
    await locast.emitCapabilityUpdate({ ...ROOM, title: "P1-T10 renamed" } as never);
    await expect.poll(async () => (await listeners(page)).received).toBe(receivedBefore + 1);

    const after = await snapshot();
    expect(after).toEqual(before);
    expect((await listeners(page)).active).toBe(1);
});

test("inside a room, Play loads the item into that room's player", async ({ page }) => {
    const it = item(7);
    await openLibrary(page, [it]);
    await joinRoomThenOpenLibrary(page);
    await page.getByRole("button", { name: "Play File 7.mp4" }).click();

    await expect(page).toHaveURL(/\/rooms\/r-p1t10$/);
    await expect(page.getByTestId("room-local")).toHaveCount(0);
    await expect(video(page)).toHaveAttribute(
        "src",
        `http://locast.localhost/media/${sha16(it)}/File%207.mp4`,
    );
});

test("a room with no media offers a link to pick one from the library", async ({ page }) => {
    await openLibrary(page, [item(9)]);
    await joinRoomThenOpenLibrary(page);
    await page.evaluate(() => {
        window.history.pushState({}, "", "/rooms/r-p1t10");
        window.dispatchEvent(new PopStateEvent("popstate"));
    });
    await expect(page.getByTestId("locast-player")).toContainText("No media loaded yet.");
    await page.getByRole("link", { name: "Choose a file from your library" }).click();
    await expect(page).toHaveURL(/\/library$/);
    await expect(page.getByTestId("library-tile")).toHaveCount(1);
});

test("an item that cannot be resolved shows an error and stays on the library", async ({ page }) => {
    await openLibrary(page, [item(8)]);
    await expect(page.getByTestId("library-tile")).toHaveCount(1);
    // The row vanishes behind the UI's back (e.g. deleted elsewhere).
    await page.evaluate(() => {
        (window as unknown as { __locast_library: { items: unknown[] } }).__locast_library.items = [];
    });
    await page.getByRole("button", { name: "Play File 8.mp4" }).click();
    await expect(page.getByRole("alert")).toContainText("Could not play File 8.mp4");
    await expect(page).toHaveURL(/\/library$/);
});

// Playing from the library while not in a room leaves the file in the playback
// store. Creating a room used to open with that unrelated file still loaded.
test("starting a new room drops the file that was playing locally", async ({ page }) => {
    const it = item(5);
    await openLibrary(page, [it]);
    await page.getByRole("button", { name: "Play File 5.mp4" }).click();
    await expect(page.getByTestId("room-local")).toContainText("File 5.mp4");

    const goTo = (to: string) =>
        page.evaluate((target) => {
            window.history.pushState({}, "", target);
            window.dispatchEvent(new PopStateEvent("popstate"));
        }, to);
    await goTo("/rooms/new");
    await page.getByRole("textbox").first().fill("Movie night");
    await page.getByRole("button", { name: /create/i }).click();

    // Back on the local page: the earlier file must be gone.
    await goTo("/rooms/local");
    await expect(page.getByTestId("room-local")).toHaveCount(0);
});
