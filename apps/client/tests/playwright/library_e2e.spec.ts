// P1-T09 acceptance (roadmap):
//   "component test asserts the grid renders N tiles for N items, search
//    filters in < 50 ms, the 'Make permanent' action flips a Zustand store +
//    SQLite row, and an a11y test confirms the grid is keyboard navigable."
//
// The repository has no component-test runner (the client `test` script is a
// stub), so these run in the Playwright Vite harness like the other UI specs.
// The harness stands in for the Rust backend with an in-memory catalog
// (`window.__locast_library`, see fixtures/vite-app.ts) that plays the part of
// the `media_items` table; the real SQL behaviour is covered by the Rust
// integration tests in tests/library_catalog.rs.

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

function item(n: number, over: Partial<Item> = {}): Item {
    return {
        id: `item-${n}`,
        sha256: String(n).padStart(64, "0"),
        filename: `File ${n}.mkv`,
        size_bytes: 1_000_000 * n,
        duration_ms: null,
        width: null,
        height: null,
        video_codec: null,
        audio_codec: null,
        container: null,
        status: "permanent",
        created_at: 1_700_000_000_000 + n,
        ...over,
    };
}

async function openLibrary(
    page: Page,
    items: Item[],
    opts: { failList?: boolean; nextPick?: string[] | null } = {},
): Promise<void> {
    await injectLocastShim(page);
    await page.addInitScript(
        (seed) => {
            (window as unknown as { __locast_library: unknown }).__locast_library = {
                items: seed.items,
                failList: seed.failList,
                nextPick: seed.nextPick,
                imported: [],
                serial: 1,
            };
        },
        { items, failList: opts.failList ?? false, nextPick: opts.nextPick ?? null },
    );
    await page.goto("/library");
}

const tiles = (page: Page) => page.locator('[data-testid="library-tile"]');

async function catalog(page: Page): Promise<Item[]> {
    return await page.evaluate(
        () => (window as unknown as { __locast_library: { items: Item[] } }).__locast_library.items,
    );
}

async function storeItems(page: Page): Promise<Item[]> {
    return await page.evaluate(() => {
        const s = (window as unknown as {
            __locastMediaStore: { getState: () => { items: Item[] } };
        }).__locastMediaStore;
        return s.getState().items;
    });
}

test("an empty library shows a useful empty state and an Import button", async ({ page }) => {
    await openLibrary(page, []);
    await expect(page.getByTestId("library-empty")).toContainText("Your library is empty.");
    await expect(page.getByRole("button", { name: "Import files" })).toBeEnabled();
    await expect(tiles(page)).toHaveCount(0);
});

test("the grid renders one tile per item with identifying metadata", async ({ page }) => {
    const items = Array.from({ length: 12 }, (_, i) => item(i + 1));
    items[0] = item(1, {
        filename: "Big Movie.mkv",
        size_bytes: 3_221_225_472,
        duration_ms: 5_025_000,
        width: 1920,
        height: 1080,
        container: "matroska",
    });
    await openLibrary(page, items);

    await expect(tiles(page)).toHaveCount(12);
    await expect(page.getByRole("list", { name: "Media library" })).toBeVisible();
    const big = page.getByRole("article", { name: "Big Movie.mkv" });
    await expect(big).toContainText("3.0 GB");
    await expect(big).toContainText("1:23:45");
    await expect(big).toContainText("1920\u00d71080");
    await expect(big.getByTestId("library-tile-status")).toHaveText("Permanent");
});

test("search filters the grid in under 50 ms and shows a no-match state", async ({ page }) => {
    await openLibrary(page, [
        item(1, { filename: "Movie Night.mkv" }),
        item(2, { filename: "Holiday.mkv" }),
        item(3, { filename: "MovieTrailer.mp4" }),
        item(4, { filename: "Concert.webm" }),
    ]);
    await expect(tiles(page)).toHaveCount(4);

    const elapsedMs = await page.evaluate(async () => {
        const input = document.querySelector<HTMLInputElement>('input[aria-label="Search library"]');
        if (!input) throw new Error("search box missing");
        const setValue = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")?.set;
        if (!setValue) throw new Error("no value setter");
        const t0 = performance.now();
        setValue.call(input, "movie");
        input.dispatchEvent(new Event("input", { bubbles: true }));
        await new Promise<void>((resolve) => {
            const check = (): void => {
                if (document.querySelectorAll('[data-testid="library-tile"]').length === 2) resolve();
                else requestAnimationFrame(check);
            };
            check();
        });
        return performance.now() - t0;
    });
    expect(elapsedMs).toBeLessThan(50);
    await expect(page.getByRole("article", { name: "Movie Night.mkv" })).toBeVisible();
    await expect(page.getByRole("article", { name: "MovieTrailer.mp4" })).toBeVisible();

    await page.getByLabel("Search library").fill("zzz");
    await expect(page.getByTestId("library-no-matches")).toContainText("zzz");
    await expect(tiles(page)).toHaveCount(0);

    await page.getByLabel("Search library").fill("");
    await expect(tiles(page)).toHaveCount(4);
});

test("importing through the picker adds the files to the library", async ({ page }) => {
    await openLibrary(page, [], { nextPick: ["C:\\Videos\\Holiday.mp4", "/home/me/Concert.mkv"] });
    await expect(page.getByTestId("library-empty")).toBeVisible();

    await page.getByRole("button", { name: "Import files" }).click();

    await expect(page.getByTestId("library-notice")).toContainText("Imported 2 files.");
    await expect(tiles(page)).toHaveCount(2);
    await expect(page.getByRole("article", { name: "Holiday.mp4" })).toBeVisible();
    await expect(page.getByRole("article", { name: "Concert.mkv" })).toBeVisible();
    await expect(page.getByTestId("library-empty")).toHaveCount(0);
    const imported = await page.evaluate(
        () => (window as unknown as { __locast_library: { imported: string[][] } }).__locast_library.imported,
    );
    expect(imported).toEqual([["C:\\Videos\\Holiday.mp4", "/home/me/Concert.mkv"]]);
});

test("cancelling the picker imports nothing", async ({ page }) => {
    await openLibrary(page, [], { nextPick: null });
    await page.getByRole("button", { name: "Import files" }).click();
    await expect(page.getByRole("button", { name: "Import files" })).toBeEnabled();
    await expect(page.getByTestId("library-notice")).toHaveCount(0);
    expect((await catalog(page)).length).toBe(0);
});

test("Make permanent flips the store and the database row", async ({ page }) => {
    await openLibrary(page, [
        item(1, { filename: "Kept.mkv" }),
        item(2, { filename: "Temp.mkv", status: "temporary" }),
    ]);
    const temp = page.getByRole("article", { name: "Temp.mkv" });
    await expect(temp.getByTestId("library-tile-status")).toHaveText("Temporary");
    await expect(page.getByRole("article", { name: "Kept.mkv" }).getByRole("button", { name: "Make permanent" })).toHaveCount(0);

    await temp.getByRole("button", { name: "Make permanent" }).click();

    await expect(temp.getByTestId("library-tile-status")).toHaveText("Permanent");
    await expect(temp.getByRole("button", { name: "Make permanent" })).toHaveCount(0);
    expect((await storeItems(page)).find((i) => i.id === "item-2")?.status).toBe("permanent");
    expect((await catalog(page)).find((i) => i.id === "item-2")?.status).toBe("permanent");
});

test("Delete asks for confirmation, then removes the item everywhere", async ({ page }) => {
    await openLibrary(page, [item(1, { filename: "Keep.mkv" }), item(2, { filename: "Remove.mkv" })]);
    const doomed = page.getByRole("article", { name: "Remove.mkv" });

    await doomed.getByRole("button", { name: "Delete Remove.mkv" }).click();
    await expect(doomed).toContainText("Delete from your library?");
    await doomed.getByRole("button", { name: "Cancel" }).click();
    await expect(tiles(page)).toHaveCount(2);
    expect((await catalog(page)).length).toBe(2);

    await doomed.getByRole("button", { name: "Delete Remove.mkv" }).click();
    await doomed.getByRole("button", { name: "Confirm delete" }).click();

    await expect(tiles(page)).toHaveCount(1);
    await expect(page.getByRole("article", { name: "Remove.mkv" })).toHaveCount(0);
    expect((await storeItems(page)).map((i) => i.id)).toEqual(["item-1"]);
    expect((await catalog(page)).map((i) => i.id)).toEqual(["item-1"]);
});

test("a failed load shows an error and Try again recovers", async ({ page }) => {
    await openLibrary(page, [item(1)], { failList: true });
    await expect(page.getByRole("alert")).toContainText("database is locked");
    await page.evaluate(() => {
        (window as unknown as { __locast_library: { failList: boolean } }).__locast_library.failList = false;
    });
    await page.getByRole("button", { name: "Try again" }).click();
    await expect(tiles(page)).toHaveCount(1);
    await expect(page.getByRole("alert")).toHaveCount(0);
});

test("the grid is keyboard navigable", async ({ page }) => {
    const items = Array.from({ length: 9 }, (_, i) => item(i + 1, i === 1 ? { status: "temporary" } : {}));
    await openLibrary(page, items);
    await expect(tiles(page)).toHaveCount(9);

    // The grid is a single Tab stop (roving tabindex).
    await expect(page.locator('[data-testid="library-tile"][tabindex="0"]')).toHaveCount(1);

    const activeId = (): Promise<string | null> =>
        page.evaluate(() => document.activeElement?.getAttribute("data-media-id") ?? null);

    // Tab from the top of the page reaches the grid.
    await page.getByLabel("Search library").focus();
    let reached = false;
    for (let i = 0; i < 12 && !reached; i++) {
        await page.keyboard.press("Tab");
        reached = (await activeId()) !== null;
    }
    expect(reached, "Tab reaches a tile").toBe(true);
    expect(await activeId()).toBe("item-9"); // newest first

    // Arrow keys, Home and End move between tiles.
    await page.keyboard.press("Home");
    const first = await activeId();
    await page.keyboard.press("ArrowRight");
    const second = await activeId();
    expect(second).not.toBe(first);
    await page.keyboard.press("ArrowLeft");
    expect(await activeId()).toBe(first);
    await page.keyboard.press("End");
    expect(await activeId()).toBe("item-1");

    // ArrowDown moves one row (as many tiles as share the first row).
    await page.keyboard.press("Home");
    const columns = await page.evaluate(() => {
        const els = Array.from(document.querySelectorAll('[data-testid="library-tile"]'));
        const top = els[0]?.getBoundingClientRect().top ?? 0;
        return els.filter((e) => Math.abs(e.getBoundingClientRect().top - top) < 2).length;
    });
    expect(columns).toBeGreaterThan(1);
    await page.keyboard.press("ArrowDown");
    expect(await activeId()).toBe(`item-${9 - columns}`);

    // The tile's actions are reachable and operable from the keyboard.
    await page.locator('[data-testid="library-tile"][data-media-id="item-2"]').focus();
    await page.keyboard.press("Tab");
    await expect(page.getByRole("button", { name: "Make permanent" })).toBeFocused();
    await page.keyboard.press("Enter");
    await expect(
        page.getByRole("article", { name: "File 2.mkv" }).getByTestId("library-tile-status"),
    ).toHaveText("Permanent");
});

test("every control has an accessible name", async ({ page }) => {
    await openLibrary(page, [item(1, { status: "temporary" })]);
    await expect(page.getByRole("searchbox", { name: "Search library" })).toBeVisible();
    await expect(page.getByRole("button", { name: "Import files" })).toBeVisible();
    await expect(page.getByRole("navigation", { name: "Rooms" })).toBeVisible();
    await expect(page.getByRole("button", { name: "Make permanent" })).toBeVisible();
    await expect(page.getByRole("button", { name: "Delete File 1.mkv" })).toBeVisible();
    await expect(page.getByRole("heading", { name: "File 1.mkv" })).toBeVisible();
});
