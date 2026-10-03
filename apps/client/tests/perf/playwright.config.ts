import { defineConfig } from "@playwright/test";

// P8-T07: webview performance trace. Kept separate from the functional e2e
// config (tests/playwright) so the multi-minute scenario never runs as part of
// `pnpm test:e2e`. Run with `pnpm test:perf`.
export default defineConfig({
    testDir: ".",
    testMatch: /.*\.spec\.ts/,
    fullyParallel: false,
    retries: 0,
    workers: 1,
    // 60 s playback + 5 min drawing, plus setup and trace flush.
    timeout: 12 * 60_000,
    use: {
        baseURL: "http://127.0.0.1:1420/",
        headless: true,
        viewport: { width: 1280, height: 800 },
        // The scenario records its own traces (see the spec); the default
        // per-test trace would double the capture overhead.
        trace: "off",
        launchOptions: {
            // A looping, muted <video> must start without a user gesture.
            args: ["--autoplay-policy=no-user-gesture-required"],
        },
    },
    webServer: {
        command: "pnpm dev:test",
        url: "http://127.0.0.1:1420/",
        reuseExistingServer: true,
        timeout: 60_000,
    },
    reporter: [["list"]],
});
