// apps/client/src/services/recentRooms.ts
//
// Typed wrapper over the Rust recent-rooms IPC surface (P2-T08).
// The recents list is a client-side SQLite table; "Recent" rooms
// survive a restart of the desktop client. The `/rooms` page is
// the only consumer in this phase.

import { commands } from "./ipc";
import type { RecentRoomEntry } from "../bindings";

export type { RecentRoomEntry, RecentRoomRole } from "../bindings";

/** Read the recents list (newest activity first, capped at 100). */
export async function listRecentRooms(): Promise<RecentRoomEntry[]> {
    return await commands.recentRoomsList();
}

/** Upsert a single recents row. */
export async function upsertRecentRoom(entry: RecentRoomEntry): Promise<void> {
    await commands.recentRoomUpsert(entry);
}
