// P8-T07 acceptance (roadmap):
//   "a Playwright test traces a 60 s playback session with 5-minute drawing
//    activity; the trace is uploaded; a script flags any long task > 50 ms."
//
// The roadmap sentence nests a 5-minute drawing activity inside a 60 s session,
// which cannot both hold at once, so the scenario is two phases on one trace:
//
//   phase 1 (PERF_PLAYBACK_SECONDS, default 60):  <video> playing AND drawing
//   phase 2 (remainder of PERF_DRAWING_SECONDS, default 300):  drawing only
//
// i.e. the video plays for 60 s and drawing is sustained for 5 minutes, the
// first 60 s of it concurrent with playback. Both durations are overridable so
// the scenario can be smoke-run locally in seconds.
//
// What is captured (all under apps/client/perf-results/):
//   playwright-trace.zip  Playwright trace of the whole scenario
//   chrome-trace.json     Chromium DevTools timeline trace (open in the
//                         Performance panel / ui.perfetto.dev)
//   long-tasks.json       every Long Task (> 50 ms) seen during the scenario
//   summary.json          scenario parameters, sanity metrics, and the achieved
//                         pointer rate per 30 s window (a falling rate means the
//                         page could not keep up with sustained input)
// scripts: `pnpm perf:flag` (tests/perf/flag-long-tasks.mjs) reads long-tasks.json.
//
// The harness is the same Vite dev-server + Tauri shim as tests/playwright, so
// the numbers are for an unminified React development build: use them to
// compare traces against each other, not as absolute production figures.

import { mkdirSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { test, expect, injectLocastShim } from "../playwright/fixtures/vite-app";
import type { Page } from "@playwright/test";

const OUT_DIR = fileURLToPath(new URL("../../perf-results/", import.meta.url));
const PLAYBACK_SECONDS = Number(process.env["PERF_PLAYBACK_SECONDS"] ?? 60);
const DRAWING_SECONDS = Number(process.env["PERF_DRAWING_SECONDS"] ?? 300);
const POINTER_HZ = 120;
const WINDOW_SECONDS = 30;
const CAP_ALL = 0xff;

const ROOM = {
    id: "r-p8t07-room",
    code: "P807AB",
    title: "P8-T07",
    host_user_id: "11111111-1111-1111-1111-111111111111",
    host_migration_enabled: true,
    created_ms: 1_700_000_000_000,
    participants: [
        {
            user_id: "11111111-1111-1111-1111-111111111111",
            display_name: "host",
            joined_ms: 1_700_000_000_000,
            status: "Connected" as const,
            last_seen_ms: 1_700_000_000_000,
            is_host: true,
        },
    ],
    host_disconnected: false,
    host_disconnect_deadline_ms: null,
    you_cap_set: CAP_ALL,
};

type LongTask = { startTime: number; duration: number };

async function spaNavigate(page: Page, path: string): Promise<void> {
    await page.evaluate((to) => {
        window.history.pushState({}, "", to);
        window.dispatchEvent(new PopStateEvent("popstate"));
    }, path);
}

test.describe.configure({ mode: "serial" });

test("trace a 60 s playback session with 5-minute drawing activity", async ({
    page,
    context,
    browser,
    browserName,
    locast,
}) => {
    test.skip(browserName !== "chromium", "Chromium DevTools tracing is Chromium-only");
    mkdirSync(OUT_DIR, { recursive: true });

    // Long Task observer, installed before any app code runs.
    await page.addInitScript(() => {
        const w = window as unknown as { __longTasks: Array<{ startTime: number; duration: number }> };
        w.__longTasks = [];
        new PerformanceObserver((list) => {
            for (const e of list.getEntries()) {
                w.__longTasks.push({ startTime: e.startTime, duration: e.duration });
            }
        }).observe({ type: "longtask", buffered: true });
    });
    await injectLocastShim(page);
    await page.goto("/");
    await page.waitForLoadState("domcontentloaded");
    await locast.waitForBridge();

    // Room + player, as in the functional drawing/playback specs.
    await spaNavigate(page, `/rooms/${ROOM.id}`);
    await page.waitForSelector('[data-testid="room-empty"]', { timeout: 10_000 });
    await page.waitForFunction(
        () => (window as { __locastRoomStore?: unknown }).__locastRoomStore !== undefined,
        undefined,
        { timeout: 10_000 },
    );
    await page.evaluate((summary) => {
        const w = window as unknown as {
            __locastRoomStore: { setSummary: (s: unknown) => void };
        };
        w.__locastRoomStore.setSummary(summary);
    }, ROOM);
    await page.waitForSelector('[data-testid="locast-player"]', { timeout: 10_000 });
    await page.waitForFunction(
        () => (window as { __locastStore?: unknown }).__locastStore !== undefined,
        undefined,
        { timeout: 10_000 },
    );

    // The Vite harness has no media file, so synthesize a short WebM in the page
    // (a canvas animation recorded with MediaRecorder) and loop it. This is real
    // decoded video, unlike the stubbed `/test/asset.mp4` of the functional specs.
    const clipUrl = await page.evaluate(async () => {
        const c = document.createElement("canvas");
        c.width = 640;
        c.height = 360;
        const ctx = c.getContext("2d");
        if (!ctx) throw new Error("2d context unavailable");
        const stream = c.captureStream(30);
        const rec = new MediaRecorder(stream, { mimeType: "video/webm;codecs=vp8" });
        const chunks: Blob[] = [];
        rec.ondataavailable = (e) => chunks.push(e.data);
        const done = new Promise<void>((r) => {
            rec.onstop = () => r();
        });
        rec.start();
        const t0 = performance.now();
        await new Promise<void>((resolve) => {
            const frame = (): void => {
                const t = (performance.now() - t0) / 1000;
                ctx.fillStyle = `hsl(${(t * 90) % 360}, 60%, 40%)`;
                ctx.fillRect(0, 0, c.width, c.height);
                ctx.fillStyle = "#fff";
                ctx.fillRect(40 + ((t * 120) % 500), 150, 60, 60);
                if (t < 3) requestAnimationFrame(frame);
                else resolve();
            };
            frame();
        });
        rec.stop();
        await done;
        return URL.createObjectURL(new Blob(chunks, { type: "video/webm" }));
    });
    await page.evaluate((src) => {
        const w = window as unknown as {
            __locastStore: { setMediaSrc: (s: string) => void; setMediaReady: (r: boolean) => void };
        };
        w.__locastStore.setMediaSrc(src);
        w.__locastStore.setMediaReady(true);
    }, clipUrl);
    const video = page.locator('[data-testid="locast-player-video"]');
    await video.waitFor({ state: "attached", timeout: 10_000 });
    await page.waitForFunction(
        () => {
            const v = document.querySelector<HTMLVideoElement>('[data-testid="locast-player-video"]');
            return v !== null && v.readyState >= 2;
        },
        undefined,
        { timeout: 15_000 },
    );

    // Enter pen mode through the real toolbar: `d` opens it, then pick the pen.
    await page.keyboard.press("d");
    await page.locator('[data-testid="drawing-toolbar-tool-pen"]').click();
    await page.waitForFunction(
        () => {
            const c = document.querySelector<HTMLCanvasElement>('[data-testid="locast-drawing-layer"]');
            return c !== null && c.style.pointerEvents === "auto";
        },
        undefined,
        { timeout: 5_000 },
    );

    // Prove the Long Task observer works in this browser before relying on its
    // silence: block the main thread for 120 ms inside a timer task (work run
    // directly by the CDP evaluate is not a page task) and expect it recorded.
    const selfTestTasks = await page.evaluate(async () => {
        const w = window as unknown as { __longTasks: Array<{ duration: number }> };
        const before = w.__longTasks.length;
        await new Promise<void>((resolve) =>
            setTimeout(() => {
                const end = performance.now() + 120;
                while (performance.now() < end) {
                    /* busy wait */
                }
                resolve();
            }, 10),
        );
        await new Promise((r) => setTimeout(r, 300));
        return w.__longTasks.length - before;
    });
    expect(selfTestTasks, "long task observer must see a 120 ms block").toBeGreaterThan(0);

    // --- traced scenario ---------------------------------------------------
    await context.tracing.start({ screenshots: false, snapshots: false, sources: false });
    await browser.startTracing(page, {
        path: `${OUT_DIR}chrome-trace.json`,
        screenshots: false,
        categories: [
            "-*",
            "devtools.timeline",
            "v8.execute",
            "blink.user_timing",
        ],
    });

    const metrics = await page.evaluate(
        async ({ playbackSeconds, drawingSeconds, hz, windowSeconds }) => {
            type LongTaskEntry = { startTime: number; duration: number };
            const w = window as unknown as { __longTasks: LongTaskEntry[] };
            const v = document.querySelector<HTMLVideoElement>('[data-testid="locast-player-video"]');
            const canvas = document.querySelector<HTMLCanvasElement>('[data-testid="locast-drawing-layer"]');
            if (!v || !canvas) throw new Error("player or drawing layer missing");
            v.muted = true;
            v.loop = true;

            const scenarioStart = performance.now();
            await v.play();

            let pointsDispatched = 0;
            let strokes = 0;
            const pointsPerWindow: number[] = [];
            const rect = canvas.getBoundingClientRect();
            const sleepUntil = (t: number): Promise<void> =>
                new Promise((r) => setTimeout(r, Math.max(0, t - performance.now())));
            const fire = (type: string, x: number, y: number): void => {
                canvas.dispatchEvent(
                    new PointerEvent(type, {
                        bubbles: true,
                        clientX: rect.left + x * rect.width,
                        clientY: rect.top + y * rect.height,
                        pressure: 0.5,
                        pointerId: 1,
                        pointerType: "pen",
                        isPrimary: true,
                    }),
                );
            };

            // Sustained input: 1-second strokes sampled at `hz`, back to back.
            const interval = 1000 / hz;
            const pointsPerStroke = hz;
            const drawEnd = scenarioStart + drawingSeconds * 1000;
            const playbackEnd = scenarioStart + playbackSeconds * 1000;
            let videoPaused = false;
            let next = performance.now();
            while (performance.now() < drawEnd) {
                if (!videoPaused && performance.now() >= playbackEnd) {
                    v.pause();
                    videoPaused = true;
                }
                const phase = strokes * 0.7;
                const pos = (i: number): [number, number] => {
                    const a = (i / pointsPerStroke) * Math.PI * 2;
                    return [0.5 + 0.35 * Math.sin(a * 2 + phase), 0.5 + 0.35 * Math.cos(a * 3 + phase)];
                };
                const [sx, sy] = pos(0);
                fire("pointerdown", sx, sy);
                for (let i = 1; i < pointsPerStroke; i++) {
                    next += interval;
                    await sleepUntil(next);
                    const [x, y] = pos(i);
                    fire("pointermove", x, y);
                    pointsDispatched++;
                    const win = Math.floor((performance.now() - scenarioStart) / (windowSeconds * 1000));
                    pointsPerWindow[win] = (pointsPerWindow[win] ?? 0) + 1;
                }
                const [ex, ey] = pos(pointsPerStroke);
                fire("pointerup", ex, ey);
                strokes++;
                next += interval;
                await sleepUntil(next);
            }
            if (!videoPaused) v.pause();

            const quality = v.getVideoPlaybackQuality();
            return {
                scenarioStart,
                scenarioEnd: performance.now(),
                droppedVideoFrames: quality.droppedVideoFrames,
                totalVideoFrames: quality.totalVideoFrames,
                strokes,
                pointsDispatched,
                pointsPerWindow,
                longTasks: w.__longTasks.filter((t) => t.startTime >= scenarioStart),
            };
        },
        {
            playbackSeconds: PLAYBACK_SECONDS,
            drawingSeconds: DRAWING_SECONDS,
            hz: POINTER_HZ,
            windowSeconds: WINDOW_SECONDS,
        },
    );

    await browser.stopTracing();
    await context.tracing.stop({ path: `${OUT_DIR}playwright-trace.zip` });
    // -----------------------------------------------------------------------

    // The pointer pipeline draws locally (the wire path is not wired into the
    // component yet), so the stroke store is what proves strokes were produced.
    const strokesInStore = await page.evaluate(() => {
        const w = window as unknown as { __locastDrawing?: { getStrokes: () => unknown[] } };
        return w.__locastDrawing?.getStrokes().length ?? 0;
    });
    const longTasks: LongTask[] = metrics.longTasks;
    writeFileSync(
        `${OUT_DIR}long-tasks.json`,
        JSON.stringify({ thresholdMs: 50, tasks: longTasks }, null, 2) + "\n",
    );
    const durationSeconds = (metrics.scenarioEnd - metrics.scenarioStart) / 1000;
    writeFileSync(
        `${OUT_DIR}summary.json`,
        JSON.stringify(
            {
                playbackSeconds: PLAYBACK_SECONDS,
                drawingSeconds: DRAWING_SECONDS,
                pointerHz: POINTER_HZ,
                scenarioDurationSeconds: durationSeconds,
                achievedPointerHz: metrics.pointsDispatched / durationSeconds,
                strokes: metrics.strokes,
                pointsDispatched: metrics.pointsDispatched,
                strokesInStore,
                achievedPointerHzByWindow: metrics.pointsPerWindow.map(
                    (n, i) => n / Math.min(WINDOW_SECONDS, durationSeconds - i * WINDOW_SECONDS),
                ),
                droppedVideoFrames: metrics.droppedVideoFrames,
                totalVideoFrames: metrics.totalVideoFrames,
                longTaskObserverSelfTest: selfTestTasks,
                longTaskCount: longTasks.length,
                longestTaskMs: longTasks.reduce((m, t) => Math.max(m, t.duration), 0),
            },
            null,
            2,
        ) + "\n",
    );

    // Sanity: the scenario really exercised playback and drawing. Long tasks
    // themselves are reported by the flag script, not asserted here.
    expect(metrics.strokes).toBeGreaterThan(0);
    expect(strokesInStore).toBeGreaterThanOrEqual(Math.floor(metrics.strokes * 0.9));
    // The 30 fps source loops, so currentTime wraps; decoded frame count is the
    // honest measure that video kept playing for the playback phase.
    expect(metrics.totalVideoFrames).toBeGreaterThan(PLAYBACK_SECONDS * 10);
});
