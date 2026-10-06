// The Rooms page records the room the user is in as a "recent". The role it
// stores used to be wrong for every host: it compared the identity id with
// `host_user_id`, which is the server-assigned id, so hosts were saved as guests.

import { test, expect, injectLocastShim } from "./fixtures/vite-app";

const HOST_ID = "aaaa0000-0000-0000-0000-00000000000a";
const VIEWER_ID = "bbbb0000-0000-0000-0000-000000000001";

function summary(you: string) {
    const p = (id: string, name: string, isHost: boolean) => ({
        user_id: id,
        display_name: name,
        joined_ms: 1_700_000_000_000,
        status: "Connected" as const,
        last_seen_ms: 1_700_000_000_000,
        is_host: isHost,
    });
    return {
        id: "r-recents-role",
        code: "RECNT1",
        title: "recents",
        host_user_id: HOST_ID,
        host_migration_enabled: true,
        created_ms: 1_700_000_000_000,
        participants: [p(HOST_ID, "host", true), p(VIEWER_ID, "viewer", false)],
        host_disconnected: false,
        host_disconnect_deadline_ms: null,
        you_user_id: you,
    };
}

for (const [you, role] of [
    [HOST_ID, "host"],
    [VIEWER_ID, "guest"],
] as const) {
    test(`a room entered as ${role} is recorded with role ${role}`, async ({ page, locast }) => {
        await injectLocastShim(page);
        await page.addInitScript(() => {
            const w = window as unknown as {
                __TAURI_INTERNALS__: {
                    invoke: (name: string, args?: unknown, options?: unknown) => Promise<unknown>;
                };
                __recent_upserts: Array<{ entry: { role: string } }>;
            };
            w.__recent_upserts = [];
            const original = w.__TAURI_INTERNALS__.invoke;
            w.__TAURI_INTERNALS__.invoke = (name, args, options) => {
                if (name === "recent_room_upsert") {
                    w.__recent_upserts.push(args as { entry: { role: string } });
                    return Promise.resolve(null);
                }
                if (name === "recent_rooms_list") return Promise.resolve([]);
                if (name === "identity_get") {
                    // The identity id is NOT the server id the room uses.
                    return Promise.resolve({ user_id: "identity-pubkey-hash", display_name: "guest" });
                }
                return original(name, args, options);
            };
        });
        await page.goto("/rooms");
        await expect
            .poll(
                async () => {
                    // The page subscribes asynchronously; keep announcing the
                    // room until it has.
                    await locast.emitRoomState(summary(you));
                    return await page.evaluate(
                        () =>
                            (window as unknown as { __recent_upserts: Array<{ entry: { role: string } }> })
                                .__recent_upserts[0]?.entry.role ?? null,
                    );
                },
                { timeout: 12_000, intervals: [1_500] },
            )
            .toBe(role);
    });
}
