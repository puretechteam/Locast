// Settings page: the server address the signaling client connects to.
//
// The page is driven through the Vite test shim. The two Settings commands are
// stubbed here by wrapping the shim's invoke (registered after
// `injectLocastShim`, which installs the invoke it wraps). The stub follows the
// contract of the Rust commands: it returns the saved, active and next-launch
// addresses (the env var wins, else the saved address, else the default), and
// rejects an address that is not `wss://`. The validator itself is covered by
// the Rust tests (`net::config`, `tests/server_settings.rs`), so these specs
// prove the page's rendering and the shape of its calls.

import { test, expect, injectLocastShim } from "./fixtures/vite-app";
import type { Page } from "@playwright/test";

const DEFAULT_URL = "ws://127.0.0.1:8787/ws";
const GOOD_URL = "wss://good.example.com/ws";

interface StubOptions {
    configured?: string | null;
    /** The address the running client is using. */
    active?: string;
    envOverride?: boolean;
    failLoad?: boolean;
}

async function stubSettings(page: Page, opts: StubOptions = {}): Promise<void> {
    await page.addInitScript(
        (o: {
            configured: string | null;
            active: string;
            envOverride: boolean;
            failLoad: boolean;
            defaultUrl: string;
        }) => {
            const w = window as unknown as {
                __TAURI_INTERNALS__: {
                    invoke: (name: string, args?: unknown, options?: unknown) => Promise<unknown>;
                };
                __settings_calls: unknown[];
            };
            w.__settings_calls = [];
            let configured = o.configured;
            const view = () => ({
                configured_url: configured,
                active_url: o.active,
                next_url: o.envOverride ? o.active : (configured ?? o.defaultUrl),
                env_override: o.envOverride,
            });
            const original = w.__TAURI_INTERNALS__.invoke;
            w.__TAURI_INTERNALS__.invoke = (name, args, options) => {
                if (name === "settings_get_server") {
                    if (o.failLoad) {
                        return Promise.reject({ kind: "Other", message: "settings: database is locked" });
                    }
                    return Promise.resolve(view());
                }
                if (name === "settings_set_server_url") {
                    const url = (args as { url: string | null }).url;
                    w.__settings_calls.push(url);
                    if (url !== null && !url.startsWith("wss://")) {
                        return Promise.reject({
                            kind: "Other",
                            message: "the address must start with wss:// (or ws:// for this computer only)",
                        });
                    }
                    configured = url;
                    return Promise.resolve(view());
                }
                return original(name, args, options);
            };
        },
        {
            configured: opts.configured ?? null,
            active: opts.active ?? DEFAULT_URL,
            envOverride: opts.envOverride ?? false,
            failLoad: opts.failLoad ?? false,
            defaultUrl: DEFAULT_URL,
        },
    );
}

async function openSettings(page: Page, opts: StubOptions = {}): Promise<void> {
    await injectLocastShim(page);
    await stubSettings(page, opts);
    await page.goto("/settings");
    await expect(page.locator('[data-testid="settings-form"]')).toBeVisible();
}

async function calls(page: Page): Promise<unknown[]> {
    return await page.evaluate(
        () => (window as unknown as { __settings_calls: unknown[] }).__settings_calls,
    );
}

const restartNote = '[data-testid="settings-restart"]';
const savedNote = '[data-testid="settings-saved"]';

test("shows the address in use and an empty field when nothing is saved", async ({ page }) => {
    await openSettings(page);
    await expect(page.locator('[data-testid="settings-active-url"]')).toHaveText(DEFAULT_URL);
    await expect(page.locator('[data-testid="settings-url"]')).toHaveValue("");
    await expect(page.locator('[data-testid="settings-env-note"]')).toHaveCount(0);
    await expect(page.locator(restartNote)).toHaveCount(0);
    await expect(page.locator('[data-testid="settings-reset"]')).toBeDisabled();
});

test("saving an address stores it trimmed and asks for a restart", async ({ page }) => {
    await openSettings(page);
    await page.locator('[data-testid="settings-url"]').fill("  wss://locast.example.com/ws ");
    await page.getByRole("button", { name: "Save" }).click();

    await expect(page.locator(savedNote)).toBeVisible();
    await expect(page.locator(restartNote)).toContainText("wss://locast.example.com/ws");
    expect(await calls(page)).toEqual(["wss://locast.example.com/ws"]);
    await expect(page.locator('[data-testid="settings-url"]')).toHaveValue(
        "wss://locast.example.com/ws",
    );
    // The running client is unchanged until the next launch.
    await expect(page.locator('[data-testid="settings-active-url"]')).toHaveText(DEFAULT_URL);
});

test("a pending saved address is called out on load", async ({ page }) => {
    await openSettings(page, { configured: GOOD_URL });
    await expect(page.locator('[data-testid="settings-url"]')).toHaveValue(GOOD_URL);
    await expect(page.locator(restartNote)).toContainText(GOOD_URL);
    await expect(page.locator(savedNote)).toHaveCount(0);
});

test("no restart is called for when the saved address is the one in use", async ({ page }) => {
    await openSettings(page, { configured: GOOD_URL, active: GOOD_URL });
    await expect(page.locator(restartNote)).toHaveCount(0);
});

test("a rejected address shows the reason and does not claim to be saved", async ({ page }) => {
    await openSettings(page, { configured: GOOD_URL });

    await page.locator('[data-testid="settings-url"]').fill("http://remote.example.com/ws");
    await page.getByRole("button", { name: "Save" }).click();

    await expect(page.locator('[data-testid="settings-error"]')).toContainText("wss://");
    await expect(page.locator('[data-testid="settings-url"]')).toHaveAttribute(
        "aria-invalid",
        "true",
    );
    await expect(page.locator(savedNote)).toHaveCount(0);
    // The previously saved address is still the one waiting for a restart.
    await expect(page.locator(restartNote)).toContainText(GOOD_URL);

    // Editing the field dismisses the stale error.
    await page.locator('[data-testid="settings-url"]').fill("wss://");
    await expect(page.locator('[data-testid="settings-error"]')).toHaveCount(0);
});

test("Use the default clears the saved address", async ({ page }) => {
    await openSettings(page, { configured: GOOD_URL });
    await page.locator('[data-testid="settings-reset"]').click();

    await expect(page.locator(savedNote)).toBeVisible();
    await expect(page.locator(restartNote)).toHaveCount(0);
    await expect(page.locator('[data-testid="settings-url"]')).toHaveValue("");
    expect(await calls(page)).toEqual([null]);
    await expect(page.locator('[data-testid="settings-reset"]')).toBeDisabled();
});

test("clearing the address the app is running on asks for a restart", async ({ page }) => {
    // Running on the saved address: the next launch will use the default.
    await openSettings(page, { configured: GOOD_URL, active: GOOD_URL });
    await page.locator('[data-testid="settings-reset"]').click();

    await expect(page.locator(savedNote)).toBeVisible();
    await expect(page.locator(restartNote)).toContainText(DEFAULT_URL);
});

test("tells the user when the environment variable overrides the saved address", async ({ page }) => {
    await openSettings(page, { configured: GOOD_URL, envOverride: true });
    await expect(page.locator('[data-testid="settings-env-note"]')).toContainText(
        "LOCAST_SIGNALING_URL",
    );
    // The env var decides the address for the next launch too, so no restart.
    await expect(page.locator(restartNote)).toHaveCount(0);
});

test("a failed load shows the error and a way back", async ({ page }) => {
    await injectLocastShim(page);
    await stubSettings(page, { failLoad: true });
    await page.goto("/settings");

    await expect(page.locator('[data-testid="settings-load-error"]')).toContainText(
        "database is locked",
    );
    await expect(page.getByRole("link", { name: "Back to library" })).toBeVisible();
});

test("the library links to Settings", async ({ page }) => {
    await injectLocastShim(page);
    await stubSettings(page);
    await page.goto("/library");
    await page.getByRole("link", { name: "Settings" }).click();
    await expect(page).toHaveURL(/\/settings$/);
    await expect(page.locator('[data-testid="settings-form"]')).toBeVisible();
});
