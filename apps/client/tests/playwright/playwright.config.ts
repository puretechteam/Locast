import { defineConfig } from "@playwright/test";

export default defineConfig({
    testDir: ".",
    fullyParallel: false,
    retries: 0,
    workers: 1,
    use: {
        baseURL: "http://localhost:1420",
        headless: true,
        viewport: { width: 1280, height: 800 },
        trace: "off",
    },
    webServer: {
        command: "pnpm dev:test",
        url: "http://localhost:1420",
        reuseExistingServer: true,
        timeout: 60_000,
    },
    reporter: [["list"]],
});
