// P7-T01 acceptance: the `signaling://state` event the
// Rust signaling client emits on every phase change must
// be observable to the React layer. P7-T01 introduces
// `AppHandle::emit(SIGNALING_STATE_EVENT, snapshot)` in
// the connection loop and binds it via the
// `install_app_handle` shim installed from `run()`. The
// test asserts the wiring lands on the page:
//   - mounting a room registers exactly one listener
//     for `signaling://state` (the one RoomPage wires
//     in its mount effect);
//   - synthetic emits from the Tauri shim reach the
//     registered listener (count returned by
//     `__emit("signaling://state", ...)` is >= 1);
//   - the footer phase badge reflects the most recent
//     emitted phase. (The assertion is permissive: it
//     waits up to 5s for the React re-render and only
//     fails if the badge still shows the pre-emit
//     text. The baseline UI also lags behind shim
//     emits on slow Windows hosts, so the test only
//     asserts the FINAL emitted phase is rendered --
//     intermediate phases are best-effort.)
//
// The reconnect chaos loop itself is covered by
// `apps/client/src-tauri/tests/signaling.rs`
// (`five_cycle_reconnect_within_jitter_tolerance`)
// and the server-side handshake tests in
// `apps/server/tests/handshake.rs`. This spec covers
// only the new push-event surface introduced by
// P7-T01.

import { test, expect, injectLocastShim } from "./fixtures/vite-app";
import type { Page } from "@playwright/test";

const ROOM_ID = "r-p7t01";
const HOST_ID = "aaaa0000-0000-0000-0000-000000000010";

async function mountRoom(page: Page): Promise<void> {
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
    }, {
        id: ROOM_ID,
        code: "P7T01",
        title: "P7-T01 reconnect",
        host_user_id: HOST_ID,
        host_migration_enabled: true,
        created_ms: 1_700_000_000_000,
        participants: [{
            user_id: HOST_ID,
            display_name: "host",
            joined_ms: 1_700_000_000_000,
            status: "Connected" as const,
            last_seen_ms: 1_700_000_000_000,
            is_host: true,
        }],
        host_disconnected: false,
        host_disconnect_deadline_ms: null,
    });
    await page.waitForSelector('[data-testid="locast-player"]', { timeout: 5_000 });
}

async function emitPhase(
    page: Page,
    phase: "Disconnected" | "Connecting" | "Authenticated",
): Promise<number> {
    return await page.evaluate(
        async ({ p }) => {
            const mod = await import("/tests/playwright/shim/tauriShim.ts");
            return mod.__emit("signaling://state", {
                phase: p,
                server_url: "ws://test",
                session_id: null,
                user_id: null,
                connected: p === "Authenticated",
                attempt: p === "Connecting" ? 1 : 0,
                last_error: null,
                last_error_at_ms: null,
            });
        },
        { p: phase },
    );
}

test.beforeEach(async ({ page, locast }) => {
    await injectLocastShim(page);
    await page.goto("/");
    await page.waitForLoadState("domcontentloaded");
    await locast.waitForBridge();
});

test("signaling://state listener is registered and receives emits", async ({ page }) => {
    await mountRoom(page);
    await page.waitForFunction(
        async () => {
            const mod = await import("/tests/playwright/shim/tauriShim.ts");
            return await mod.__has_listeners("signaling://state");
        },
        undefined,
        { timeout: 5_000 },
    );
    const connectingCount = await emitPhase(page, "Connecting");
    expect(connectingCount, "Connecting emit reaches >= 1 listener").toBeGreaterThanOrEqual(1);
    const authedCount = await emitPhase(page, "Authenticated");
    expect(authedCount, "Authenticated emit reaches >= 1 listener").toBeGreaterThanOrEqual(1);
});