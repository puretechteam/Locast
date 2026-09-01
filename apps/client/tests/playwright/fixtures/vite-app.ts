import { test as base, expect } from "@playwright/test";
import type { Page } from "@playwright/test";

export { expect };

export type DownloadStateName =
    | "pending"
    | "connecting"
    | "transferring"
    | "verifying"
    | "complete"
    | "failed"
    | "paused"
    | "cancelled";

export type DownloadStateEvent = {
    id: string;
    media_id: string;
    state: DownloadStateName;
    error_message?: string | null;
};

export type DownloadProgressEvent = {
    id: string;
    state: DownloadStateName;
    transferred_bytes: number;
    total_bytes: number;
    bytes_per_sec_ema: number;
    eta_seconds: number | null;
};

type LocastApi = {
    emitDownloadState: (p: DownloadStateEvent) => Promise<void>;
    emitDownloadProgress: (p: DownloadProgressEvent) => Promise<void>;
    waitForBridge: () => Promise<void>;
};

declare global {
    interface Window {
        __locast: LocastApi;
    }
}

const SHIM_SOURCE = `
    (function() {
        var w = window;
        var api = {
            emitDownloadState: function(payload) {
                var p = payload;
                return import("/tests/playwright/shim/tauriShim.ts").then(function(mod) {
                    mod.__emit("download://state", {
                        v: 1,
                        id: p.id,
                        media_id: p.media_id,
                        state: p.state,
                        error_message: p.error_message == null ? null : p.error_message,
                    });
                });
            },
            emitDownloadProgress: function(payload) {
                var p = payload;
                return import("/tests/playwright/shim/tauriShim.ts").then(function(mod) {
                    mod.__emit("download://progress", {
                        v: 1,
                        id: p.id,
                        state: p.state,
                        transferred_bytes: p.transferred_bytes,
                        total_bytes: p.total_bytes,
                        bytes_per_sec_ema: p.bytes_per_sec_ema,
                        eta_seconds: p.eta_seconds,
                    });
                });
            },
        };
        w.__locast = api;
    })();
`;

export async function injectLocastShim(page: Page): Promise<void> {
    await page.addInitScript({ content: SHIM_SOURCE });
}

export const test = base.extend<{ locast: LocastApi }>({
    locast: async ({ page }, use) => {
        const api: LocastApi = {
            emitDownloadState: async (p) => {
                await page.evaluate((payload) => {
                    const w = window as unknown as { __locast?: { emitDownloadState: (p: unknown) => Promise<void> } };
                    if (!w.__locast) {
                        throw new Error("__locast not present on window");
                    }
                    return w.__locast.emitDownloadState(payload);
                }, p);
            },
            emitDownloadProgress: async (p) => {
                await page.evaluate((payload) => {
                    const w = window as unknown as { __locast?: { emitDownloadProgress: (p: unknown) => Promise<void> } };
                    if (!w.__locast) {
                        throw new Error("__locast not present on window");
                    }
                    return w.__locast.emitDownloadProgress(payload);
                }, p);
            },
            waitForBridge: async () => {
                await page.waitForFunction(
                    () => (window as { __locast_subscribed?: boolean }).__locast_subscribed === true,
                    undefined,
                    { timeout: 5000 },
                );
            },
        };
        await use(api);
    },
});
