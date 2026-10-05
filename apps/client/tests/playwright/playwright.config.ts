import { defineConfig } from "@playwright/test";

const isCi = Boolean(process.env["CI"]);

export default defineConfig({
    testDir: ".",
    fullyParallel: false,
    // No retries: a retry would turn a flaky test into a silent pass.
    retries: 0,
    workers: 1,
    forbidOnly: isCi,
    outputDir: "../../test-results",
    use: {
        baseURL: "http://127.0.0.1:1420/",
        headless: true,
        viewport: { width: 1280, height: 800 },
        trace: "retain-on-failure",
        video: "retain-on-failure",
        screenshot: "only-on-failure",
    },
    webServer: {
        command: "pnpm dev:test",
        url: "http://127.0.0.1:1420/",
        // CI must always start its own server so a stale one can never be
        // reused. Locally a running dev server on :1420 is reused.
        reuseExistingServer: !isCi,
        timeout: 60_000,
    },
    reporter: isCi
        ? [
              ["list"],
              ["html", { open: "never", outputFolder: "../../playwright-report" }],
              ["json", { outputFile: "../../playwright-report/results.json" }],
          ]
        : [["list"]],
});
