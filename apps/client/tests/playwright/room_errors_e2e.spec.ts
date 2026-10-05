// A failed join or create must show why. Rust's `AppError` reaches the webview
// as an object tagged with `kind` (not an `Error`), and these pages used to
// render `String(err)`, which is "[object Object]".
//
// The commands are stubbed by wrapping the shim's invoke (registered after
// `injectLocastShim`, which installs the invoke it wraps), rejecting with the
// same `{ kind, message }` shape.

import { test, expect, injectLocastShim } from "./fixtures/vite-app";
import type { Page } from "@playwright/test";

async function rejectWith(page: Page, command: string, error: unknown): Promise<void> {
    await injectLocastShim(page);
    await page.addInitScript(
        (o: { command: string; error: unknown }) => {
            const w = window as unknown as {
                __TAURI_INTERNALS__: {
                    invoke: (name: string, args?: unknown, options?: unknown) => Promise<unknown>;
                };
            };
            const original = w.__TAURI_INTERNALS__.invoke;
            w.__TAURI_INTERNALS__.invoke = (name, args, options) =>
                name === o.command ? Promise.reject(o.error) : original(name, args, options);
        },
        { command, error },
    );
}

test("a failed join shows the server's reason", async ({ page }) => {
    await rejectWith(page, "room_join", { kind: "Other", message: "room not found" });
    await page.goto("/rooms/join");

    await page.getByPlaceholder("ABCDEF", { exact: true }).fill("ABCDEF");
    await page.getByPlaceholder("Your name").fill("viewer");
    await page.getByRole("button", { name: "Join" }).click();

    const error = page.locator(".form__error");
    await expect(error).toHaveText("room not found");
    await expect(error).not.toContainText("[object Object]");
});

test("a failed create shows the server's reason", async ({ page }) => {
    await rejectWith(page, "room_create", { kind: "Other", message: "the server is full" });
    await page.goto("/rooms/new");

    await page.getByRole("textbox").first().fill("Movie night");
    await page.getByRole("button", { name: /create/i }).click();

    const error = page.locator(".form__error");
    await expect(error).toHaveText("the server is full");
    await expect(error).not.toContainText("[object Object]");
});

test("an error with no message shows its kind rather than an object", async ({ page }) => {
    await rejectWith(page, "room_join", { kind: "SourceMissing", path: "C:\\x" });
    await page.goto("/rooms/join");

    await page.getByPlaceholder("ABCDEF", { exact: true }).fill("ABCDEF");
    await page.getByPlaceholder("Your name").fill("viewer");
    await page.getByRole("button", { name: "Join" }).click();

    await expect(page.locator(".form__error")).toHaveText("SourceMissing");
});
