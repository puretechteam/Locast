// The room page measures the clock skew against the server. The probe used to
// be an inline `async () => null`, so the measurement never ran and every
// drift and sync calculation assumed the viewer's clock matched the server's.
//
// `clock_skew_probe` is stubbed by wrapping the shim's invoke (registered after
// `injectLocastShim`, which installs the invoke it wraps). The stub answers
// like the real command: the local send and receive times and the server's
// clock, here running a fixed amount ahead of the local one.

import { test, expect, injectLocastShim } from "./fixtures/vite-app";
import type { Page } from "@playwright/test";

const ROOM_ID = "r-skew-probe";
const HOST_ID = "aaaa0000-0000-0000-0000-000000000001";

function summary() {
    return {
        id: ROOM_ID,
        code: "SKEW01",
        title: "skew",
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
}

async function stubProbe(page: Page, answer: "ahead" | "unavailable", aheadMs: number): Promise<void> {
    await page.addInitScript(
        (o: { answer: string; aheadMs: number }) => {
            const w = window as unknown as {
                __TAURI_INTERNALS__: {
                    invoke: (name: string, args?: unknown, options?: unknown) => Promise<unknown>;
                };
                __probe_calls: number;
            };
            w.__probe_calls = 0;
            const original = w.__TAURI_INTERNALS__.invoke;
            w.__TAURI_INTERNALS__.invoke = (name, args, options) => {
                if (name !== "clock_skew_probe") return original(name, args, options);
                w.__probe_calls += 1;
                if (o.answer === "unavailable") return Promise.resolve(null);
                // 20 ms round trip, the server read at the midpoint.
                const t0 = Date.now();
                return Promise.resolve({
                    t0_local_ms: t0,
                    t3_local_ms: t0 + 20,
                    server_ts_ms: t0 + 10 + o.aheadMs,
                    client_send_ms_echo: t0,
                });
            };
        },
        { answer, aheadMs },
    );
}

async function openRoom(page: Page): Promise<void> {
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
    }, summary());
    await page.waitForSelector('[data-testid="locast-player"]', { timeout: 5_000 });
}

async function readSkew(page: Page): Promise<number | null> {
    return await page.evaluate(
        () =>
            (window as unknown as { __locastClockSkew: { getSkew: () => number | null } })
                .__locastClockSkew.getSkew(),
    );
}

async function probeCalls(page: Page): Promise<number> {
    return await page.evaluate(() => (window as unknown as { __probe_calls: number }).__probe_calls);
}

test("entering a room measures the skew against the server", async ({ page }) => {
    await injectLocastShim(page);
    await stubProbe(page, "ahead", 5_000);
    await openRoom(page);

    // The first burst of four samples runs as soon as the room opens. Every
    // sample says the server is exactly 5000 ms ahead, so the median is 5000.
    await expect.poll(() => readSkew(page), { timeout: 15_000 }).toBe(5_000);
    expect(await probeCalls(page)).toBeGreaterThanOrEqual(4);
});

test("a re-rendering room page does not re-run the burst", async ({ page }) => {
    await injectLocastShim(page);
    await stubProbe(page, "ahead", 1_000);
    await openRoom(page);
    await expect.poll(() => readSkew(page), { timeout: 15_000 }).toBe(1_000);
    const afterFirstBurst = await probeCalls(page);

    // Updates to the room summary re-render the page. With an unstable probe
    // function each one restarted the cadence and fired four more probes.
    for (let i = 0; i < 5; i++) {
        await page.evaluate(
            ([s, n]) => {
                (window as unknown as { __locastRoomStore: { setSummary: (s: unknown) => void } })
                    .__locastRoomStore.setSummary({ ...s, title: `skew ${n}` });
            },
            [summary(), i] as const,
        );
    }
    // Let React render the updates and run any effects they trigger: two
    // animation frames, not a guessed delay.
    await page.evaluate(
        () =>
            new Promise<void>((resolve) =>
                requestAnimationFrame(() => requestAnimationFrame(() => resolve())),
            ),
    );

    expect(await probeCalls(page)).toBe(afterFirstBurst);
});

test("when the probe has no answer the skew stays unset and the page works", async ({ page }) => {
    await injectLocastShim(page);
    await stubProbe(page, "unavailable", 0);
    await openRoom(page);
    await expect.poll(() => probeCalls(page), { timeout: 15_000 }).toBeGreaterThanOrEqual(4);

    expect(await readSkew(page)).toBeNull();
    await expect(page.locator('[data-testid="locast-player"]')).toBeVisible();
});
