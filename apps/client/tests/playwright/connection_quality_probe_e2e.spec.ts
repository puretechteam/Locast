// The connection-quality indicator probes the server once a second. Only the
// START of a probe was throttled, so on a slow server a new probe began every
// second while earlier ones were still waiting, stacking requests on a link
// that was already struggling.
//
// `clock_skew_probe` is stubbed by wrapping the shim's invoke (registered after
// `injectLocastShim`, which installs the invoke it wraps). Each probe takes
// 2.5 s to answer. The room page also measures the clock skew through the same
// command (a burst on entry), so the stub counts only the probes made by the
// connection-quality hook, identified by the caller in the call stack (the dev
// server serves unminified modules, so the file name is there).

import { test, expect, injectLocastShim } from "./fixtures/vite-app";
import type { Page } from "@playwright/test";

const ROOM_ID = "r-quality-probe";
const HOST_ID = "aaaa0000-0000-0000-0000-000000000011";

async function openRoomWithSlowProbe(page: Page): Promise<void> {
    await injectLocastShim(page);
    await page.addInitScript(() => {
        const w = window as unknown as {
            __TAURI_INTERNALS__: {
                invoke: (name: string, args?: unknown, options?: unknown) => Promise<unknown>;
            };
            __probes_started: number;
            __probes_finished: number;
        };
        w.__probes_started = 0;
        w.__probes_finished = 0;
        const fromQualityHook = () => (new Error().stack ?? "").includes("useConnectionQuality");
        const original = w.__TAURI_INTERNALS__.invoke;
        w.__TAURI_INTERNALS__.invoke = (name, args, options) => {
            if (name !== "clock_skew_probe") return original(name, args, options);
            const counted = fromQualityHook();
            if (counted) w.__probes_started += 1;
            const t0 = Date.now();
            return new Promise((resolve) => {
                setTimeout(() => {
                    if (counted) w.__probes_finished += 1;
                    resolve({
                        t0_local_ms: t0,
                        t3_local_ms: t0 + 2_500,
                        server_ts_ms: t0 + 1_250,
                        client_send_ms_echo: t0,
                    });
                }, 2_500);
            });
        };
    });
    await page.goto("/");
    await page.evaluate((to) => {
        window.history.pushState({}, "", to);
        window.dispatchEvent(new PopStateEvent("popstate"));
    }, `/rooms/${ROOM_ID}`);
    await page.waitForSelector('[data-testid="room-empty"]', { timeout: 5_000 });
    await page.waitForFunction(
        () => (window as { __locastRoomStore?: unknown }).__locastRoomStore !== undefined,
        undefined,
        { timeout: 5_000 },
    );
    await page.evaluate((s) => {
        (window as unknown as { __locastRoomStore: { setSummary: (s: unknown) => void } })
            .__locastRoomStore.setSummary(s);
    }, {
        id: ROOM_ID,
        code: "QUAL01",
        title: "quality",
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
    });
    await page.waitForSelector(".participant-tile", { timeout: 5_000 });
}

async function counts(page: Page): Promise<{ started: number; finished: number }> {
    return await page.evaluate(() => {
        const w = window as unknown as { __probes_started: number; __probes_finished: number };
        return { started: w.__probes_started, finished: w.__probes_finished };
    });
}

test("a slow probe is not joined by new ones while it is still waiting", async ({ page }) => {
    await openRoomWithSlowProbe(page);

    // Wait for the first probe to finish (about 2.5 s after it started). By
    // then an unguarded hook would have started a probe every second: three.
    await expect
        .poll(async () => (await counts(page)).finished, { timeout: 15_000 })
        .toBeGreaterThanOrEqual(1);
    const atFirstFinish = await counts(page);
    expect(atFirstFinish.started).toBe(1);
});
