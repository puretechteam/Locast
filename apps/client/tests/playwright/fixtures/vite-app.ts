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

export type RoomSummaryIpc = {
    id: string;
    code: string;
    title: string;
    host_user_id: string;
    host_migration_enabled: boolean;
    created_ms: number;
    participants: Array<{
        user_id: string;
        display_name: string;
        joined_ms: number;
        status:
            | "Joining"
            | "Connected"
            | "Reconnecting"
            | "Disconnected"
            | "Left";
        last_seen_ms: number;
        is_host: boolean;
    }>;
    host_disconnected: boolean;
    host_disconnect_deadline_ms: number | null;
};

export type PlaybackStateEvent = {
    room_id: string;
    server_seq: number;
    server_ts_ms: number;
    sender_id: string;
    monotonic_seq: number;
    kind: "play" | "pause" | "seek";
    media_position_ms: number;
};

export type PositionReportEvent = {
    room_id: string;
    sender_id: string;
    media_position_ms: number;
    playing: boolean;
    client_ts_ms: number;
};

type LocastApi = {
    emitDownloadState: (p: DownloadStateEvent) => Promise<void>;
    emitDownloadProgress: (p: DownloadProgressEvent) => Promise<void>;
    emitRoomState: (p: RoomSummaryIpc | null) => Promise<void>;
    emitPlaybackState: (p: PlaybackStateEvent) => Promise<void>;
    emitPositionReport: (p: PositionReportEvent) => Promise<void>;
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
        /*
         * Stub the Tauri invoke() surface so the React
         * app's getRoomState() / getSignalingState()
         * calls resolve with null instead of throwing
         * in the Vite-only harness. This is a test-only
         * shim; production code is invoked through the
         * real Tauri runtime.
         */
        w.__TAURI_INVOKE = function(name, args) {
            if (name === "room_get_state") return Promise.resolve(null);
            if (name === "signaling_get_state") return Promise.resolve({
                phase: "Disconnected",
                server_url: "",
                session_id: null,
                user_id: null,
                connected: false,
                attempt: 0,
                last_error: null,
                last_error_at_ms: null,
            });
            return Promise.resolve(null);
        };
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
            emitRoomState: function(payload) {
                return import("/tests/playwright/shim/tauriShim.ts").then(function(mod) {
                    mod.__emit("room://state", payload);
                });
            },
            emitPlaybackState: function(payload) {
                return import("/tests/playwright/shim/tauriShim.ts").then(function(mod) {
                    mod.__emit("playback://state", payload);
                });
            },
            emitPositionReport: function(payload) {
                return import("/tests/playwright/shim/tauriShim.ts").then(function(mod) {
                    mod.__emit("position://report", payload);
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
            emitRoomState: async (p) => {
                await page.evaluate((payload) => {
                    const w = window as unknown as { __locast?: { emitRoomState: (p: unknown) => Promise<void> } };
                    if (!w.__locast) {
                        throw new Error("__locast not present on window");
                    }
                    return w.__locast.emitRoomState(payload);
                }, p);
            },
            emitPlaybackState: async (p) => {
                await page.evaluate((payload) => {
                    const w = window as unknown as { __locast?: { emitPlaybackState: (p: unknown) => Promise<void> } };
                    if (!w.__locast) {
                        throw new Error("__locast not present on window");
                    }
                    return w.__locast.emitPlaybackState(payload);
                }, p);
            },
            emitPositionReport: async (p) => {
                await page.evaluate((payload) => {
                    const w = window as unknown as { __locast?: { emitPositionReport: (p: unknown) => Promise<void> } };
                    if (!w.__locast) {
                        throw new Error("__locast not present on window");
                    }
                    return w.__locast.emitPositionReport(payload);
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
