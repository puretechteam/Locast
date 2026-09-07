// P6-T02 acceptance (roadmap):
//   "PERMISSION_SET / CAPABILITY_UPDATE wire protocol"
//
// This spec verifies that:
// 1. The drawing toolbar is hidden when the viewer has no DRAW cap
// 2. The drawing toolbar becomes visible after DRAW cap is granted
// 3. The drawing toolbar hides again after DRAW cap is revoked
//
// The test uses the two-client pattern from drawing_e2e.spec.ts.
// The Vite harness drives the capability store through
// `__locastRoomStore.setSummary()` and the `emitCapabilityUpdate`
// shim that fires `room://event` to trigger the store sync.

import { test, expect, injectLocastShim } from "./fixtures/vite-app";
import type { Page } from "@playwright/test";

const CAP_DRAW = 0x02;

const HOST_ID = "11111111-1111-1111-1111-111111111111";
const VIEWER_ID = "22222222-2222-2222-2222-222222222222";

const ROOM_ID = "r-p6t02-capability";

function makeRoomSummary(youCapSet: number | undefined) {
    return {
        id: ROOM_ID,
        code: "P6T02",
        title: "P6-T02",
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
            {
                user_id: VIEWER_ID,
                display_name: "viewer",
                joined_ms: 1_700_000_000_500,
                status: "Connected" as const,
                last_seen_ms: 1_700_000_000_500,
                is_host: false,
            },
        ],
        host_disconnected: false,
        host_disconnect_deadline_ms: null,
        you_cap_set: youCapSet,
    };
}

async function spaNavigate(page: Page, path: string): Promise<void> {
    await page.evaluate((to) => {
        window.history.pushState({}, "", to);
        window.dispatchEvent(new PopStateEvent("popstate"));
    }, path);
}

async function setupViewerClient(p: Page): Promise<void> {
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
}

test("toolbar hidden when viewer has no DRAW cap", async ({ page: viewerPage }) => {
    await setupViewerClient(viewerPage);

    const initialSummary = makeRoomSummary(0);
    await viewerPage.evaluate((s) => {
        const w = window as unknown as {
            __locastRoomStore?: { setSummary: (s: unknown) => void };
        };
        w.__locastRoomStore!.setSummary(s);
    }, initialSummary);

    await viewerPage.waitForSelector('[data-testid="room-empty"]', {
        state: "detached",
        timeout: 5_000,
    });
    await viewerPage.waitForSelector('[data-testid="locast-player"]', {
        timeout: 5_000,
    });
    await viewerPage.waitForFunction(
        () => (window as { __locastStore?: unknown }).__locastStore !==
            undefined,
        undefined,
        { timeout: 5_000 },
    );
    await viewerPage.evaluate(() => {
        const w = window as unknown as {
            __locastStore?: {
                setMediaSrc: (s: string) => void;
                setMediaReady: (r: boolean) => void;
            };
        };
        w.__locastStore!.setMediaSrc("/test/asset.mp4");
        w.__locastStore!.setMediaReady(true);
    });

    await viewerPage.waitForFunction(
        () =>
            (window as { __locastDrawingStore?: { setRoomId: (id: string | null) => void } })
                .__locastDrawingStore !== undefined,
        undefined,
        { timeout: 5_000 },
    );
    await viewerPage.evaluate((roomId: string) => {
        const w = window as unknown as {
            __locastDrawingStore?: { setRoomId: (id: string | null) => void };
        };
        w.__locastDrawingStore!.setRoomId(roomId);
    }, ROOM_ID);

    await viewerPage.waitForFunction(
        () =>
            (window as { __locastKeyboardScope?: { canDraw: boolean } })
                .__locastKeyboardScope !== undefined,
        undefined,
        { timeout: 5_000 },
    );

    const canDraw = await viewerPage.evaluate(() => {
        const w = window as unknown as {
            __locastKeyboardScope?: { canDraw: boolean };
        };
        return w.__locastKeyboardScope?.canDraw ?? false;
    });
    expect(canDraw).toBe(false);

    const toolbarElement = await viewerPage.locator('[data-testid="drawing-toolbar"]');
    await expect(toolbarElement).toHaveCount(0);
});

test("toolbar visible after DRAW cap is granted", async ({ page: viewerPage, locast }) => {
    await setupViewerClient(viewerPage);

    const initialSummary = makeRoomSummary(0);
    await viewerPage.evaluate((s) => {
        const w = window as unknown as {
            __locastRoomStore?: { setSummary: (s: unknown) => void };
        };
        w.__locastRoomStore!.setSummary(s);
    }, initialSummary);

    await viewerPage.waitForSelector('[data-testid="room-empty"]', {
        state: "detached",
        timeout: 5_000,
    });
    await viewerPage.waitForSelector('[data-testid="locast-player"]', {
        timeout: 5_000,
    });
    await viewerPage.waitForFunction(
        () => (window as { __locastStore?: unknown }).__locastStore !==
            undefined,
        undefined,
        { timeout: 5_000 },
    );
    await viewerPage.evaluate(() => {
        const w = window as unknown as {
            __locastStore?: {
                setMediaSrc: (s: string) => void;
                setMediaReady: (r: boolean) => void;
            };
        };
        w.__locastStore!.setMediaSrc("/test/asset.mp4");
        w.__locastStore!.setMediaReady(true);
    });

    await viewerPage.waitForFunction(
        () =>
            (window as { __locastDrawingStore?: { setRoomId: (id: string | null) => void } })
                .__locastDrawingStore !== undefined,
        undefined,
        { timeout: 5_000 },
    );
    await viewerPage.evaluate((roomId: string) => {
        const w = window as unknown as {
            __locastDrawingStore?: { setRoomId: (id: string | null) => void };
        };
        w.__locastDrawingStore!.setRoomId(roomId);
    }, ROOM_ID);

    const updatedSummary = makeRoomSummary(CAP_DRAW);
    await locast.emitCapabilityUpdate(updatedSummary);

    await viewerPage.waitForFunction(
        () => {
            const w = window as unknown as {
                __locastKeyboardScope?: { canDraw: boolean };
            };
            return w.__locastKeyboardScope?.canDraw === true;
        },
        undefined,
        { timeout: 5_000 },
    );

    await viewerPage.evaluate(() => {
        const w = window as unknown as {
            __locastKeyboardScope?: { toggleToolbar: () => void };
        };
        w.__locastKeyboardScope?.toggleToolbar();
    });

    await viewerPage.waitForFunction(
        () => {
            const w = window as unknown as {
                __locastKeyboardScope?: { toolbarVisible: boolean };
            };
            return w.__locastKeyboardScope?.toolbarVisible === true;
        },
        undefined,
        { timeout: 5_000 },
    );

    const toolbarElement = await viewerPage.locator('[data-testid="drawing-toolbar"]');
    await expect(toolbarElement).toBeVisible();
});

test("toolbar hidden after DRAW cap is revoked", async ({ page: viewerPage, locast }) => {
    await setupViewerClient(viewerPage);

    const initialSummary = makeRoomSummary(CAP_DRAW);
    await viewerPage.evaluate((s) => {
        const w = window as unknown as {
            __locastRoomStore?: { setSummary: (s: unknown) => void };
        };
        w.__locastRoomStore!.setSummary(s);
    }, initialSummary);

    await viewerPage.waitForSelector('[data-testid="room-empty"]', {
        state: "detached",
        timeout: 5_000,
    });
    await viewerPage.waitForSelector('[data-testid="locast-player"]', {
        timeout: 5_000,
    });
    await viewerPage.waitForFunction(
        () => (window as { __locastStore?: unknown }).__locastStore !==
            undefined,
        undefined,
        { timeout: 5_000 },
    );
    await viewerPage.evaluate(() => {
        const w = window as unknown as {
            __locastStore?: {
                setMediaSrc: (s: string) => void;
                setMediaReady: (r: boolean) => void;
            };
        };
        w.__locastStore!.setMediaSrc("/test/asset.mp4");
        w.__locastStore!.setMediaReady(true);
    });

    await viewerPage.waitForFunction(
        () =>
            (window as { __locastDrawingStore?: { setRoomId: (id: string | null) => void } })
                .__locastDrawingStore !== undefined,
        undefined,
        { timeout: 5_000 },
    );
    await viewerPage.evaluate((roomId: string) => {
        const w = window as unknown as {
            __locastDrawingStore?: { setRoomId: (id: string | null) => void };
        };
        w.__locastDrawingStore!.setRoomId(roomId);
    }, ROOM_ID);

    await viewerPage.waitForFunction(
        () => {
            const w = window as unknown as {
                __locastKeyboardScope?: { canDraw: boolean };
            };
            return w.__locastKeyboardScope?.canDraw === true;
        },
        undefined,
        { timeout: 5_000 },
    );

    await viewerPage.evaluate(() => {
        const w = window as unknown as {
            __locastKeyboardScope?: { toggleToolbar: () => void };
        };
        w.__locastKeyboardScope?.toggleToolbar();
    });

    await viewerPage.waitForFunction(
        () => {
            const w = window as unknown as {
                __locastKeyboardScope?: { toolbarVisible: boolean };
            };
            return w.__locastKeyboardScope?.toolbarVisible === true;
        },
        undefined,
        { timeout: 5_000 },
    );

    const revokedSummary = makeRoomSummary(0);
    await locast.emitCapabilityUpdate(revokedSummary);

    await viewerPage.waitForFunction(
        () => {
            const w = window as unknown as {
                __locastKeyboardScope?: { canDraw: boolean };
            };
            return w.__locastKeyboardScope?.canDraw === false;
        },
        undefined,
        { timeout: 5_000 },
    );

    const toolbarElement = await viewerPage.locator('[data-testid="drawing-toolbar"]');
    await expect(toolbarElement).toHaveCount(0);
});

const CAP_PLAYBACK_CONTROL = 0x01;

test("co-host can playback after applying Co-host preset", async ({ page: viewerPage, locast }) => {
    await setupViewerClient(viewerPage);

    const initialSummary = makeRoomSummary(0);
    await viewerPage.evaluate((s) => {
        const w = window as unknown as {
            __locastRoomStore?: { setSummary: (s: unknown) => void };
        };
        w.__locastRoomStore!.setSummary(s);
    }, initialSummary);

    await viewerPage.waitForSelector('[data-testid="room-empty"]', {
        state: "detached",
        timeout: 5_000,
    });
    await viewerPage.waitForSelector('[data-testid="locast-player"]', {
        timeout: 5_000,
    });
    await viewerPage.waitForFunction(
        () => (window as { __locastStore?: unknown }).__locastStore !==
            undefined,
        undefined,
        { timeout: 5_000 },
    );
    await viewerPage.evaluate(() => {
        const w = window as unknown as {
            __locastStore?: {
                setMediaSrc: (s: string) => void;
                setMediaReady: (r: boolean) => void;
            };
        };
        w.__locastStore!.setMediaSrc("/test/asset.mp4");
        w.__locastStore!.setMediaReady(true);
    });

    await viewerPage.waitForFunction(
        () =>
            (window as { __locastKeyboardScope?: { canPlayback: boolean } })
                .__locastKeyboardScope !== undefined,
        undefined,
        { timeout: 5_000 },
    );

    const canPlaybackBefore = await viewerPage.evaluate(() => {
        const w = window as unknown as {
            __locastKeyboardScope?: { canPlayback: boolean };
        };
        return w.__locastKeyboardScope?.canPlayback ?? false;
    });
    expect(canPlaybackBefore).toBe(false);

    const cohostSummary = makeRoomSummary(CAP_PLAYBACK_CONTROL);
    await locast.emitCapabilityUpdate(cohostSummary);

    await viewerPage.waitForFunction(
        () => {
            const w = window as unknown as {
                __locastKeyboardScope?: { canPlayback: boolean };
            };
            return w.__locastKeyboardScope?.canPlayback === true;
        },
        undefined,
        { timeout: 5_000 },
    );

    const canPlaybackAfter = await viewerPage.evaluate(() => {
        const w = window as unknown as {
            __locastKeyboardScope?: { canPlayback: boolean };
        };
        return w.__locastKeyboardScope?.canPlayback ?? false;
    });
    expect(canPlaybackAfter).toBe(true);
});
