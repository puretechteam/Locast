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
    /** P4-T05: read all Tauri invoke() calls recorded
     *  by the shim since the last reset. Tests assert
     *  on this to verify that the local-only sync
     *  branch does NOT emit a `playback_send` and the
     *  host-authoritative branch DOES. */
    readInvokeLog: () => Promise<
        Array<{ name: string; args: unknown }>
    >;
    /** P4-T05: clear the invoke log. Tests call this at
     *  the start of each scenario. */
    resetInvokeLog: () => Promise<void>;
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
            // P4-T05: record every playback_send invocation
            // so the manual-sync e2e tests can assert that a
            // viewer click did NOT emit a command while a
            // host click DID. The recorded list is a
            // per-test throwaway; each Playwright test
            // resets it via resetInvokeLog() before the
            // scenario under test. The default return is
            // shaped to match the Rust command success
            // type (an envelope id + the monotonic seq the
            // server would have assigned) so the host
            // branch can call sendPlaybackCommand without
            // a TypeError.
            if (name === "playback_send") {
                w.__locast_invoke_log.push({ name: name, args: args });
                return Promise.resolve({
                    envelope_id: "envelope-" + (w.__locast_invoke_log.length),
                    monotonic_seq:
                        args !== null &&
                        args !== undefined &&
                        typeof args.cmd === "object" &&
                        args.cmd !== null &&
                        typeof args.cmd.monotonic_seq === "number"
                            ? args.cmd.monotonic_seq
                            : 0,
                });
            }
            return Promise.resolve(null);
        };
        // P4-T05: the @tauri-apps/api/core.js package calls
        // window.__TAURI_INTERNALS__.invoke (not
        // window.__TAURI_INVOKE -- that property name is
        // the older Tauri 1 convention). Wire BOTH
        // shims so the bindings reach the harness
        // recording surface.
        w.__TAURI_INTERNALS__ = w.__TAURI_INTERNALS__ || {};
        w.__TAURI_INTERNALS__.invoke = w.__TAURI_INVOKE;
        // P4-T05: per-test invoke log (FIFO). resetInvokeLog
        // empties it between scenarios.
        w.__locast_invoke_log = [];
        w.__locast_resetInvokeLog = function() {
            w.__locast_invoke_log = [];
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
            readInvokeLog: async () => {
                return await page.evaluate(() => {
                    const w = window as unknown as {
                        __locast_invoke_log?: Array<{
                            name: string;
                            args: unknown;
                        }>;
                    };
                    return w.__locast_invoke_log ?? [];
                });
            },
            resetInvokeLog: async () => {
                await page.evaluate(() => {
                    const w = window as unknown as {
                        __locast_resetInvokeLog?: () => void;
                    };
                    if (w.__locast_resetInvokeLog) {
                        w.__locast_resetInvokeLog();
                    }
                });
            },
        };
        await use(api);
    },
});
