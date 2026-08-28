// apps/client/src/services/room.ts
//
// Typed wrapper over the Rust room-lifecycle IPC surface.
// P2-T04: room create / join / leave / state queries over
// the existing signaling WebSocket.

import { commands } from "./ipc";
import type { RoomSummaryIpc } from "../bindings";

export type { RoomSummaryIpc, ParticipantIpc, ParticipantStatusIpc } from "../bindings";

/** Idempotent: ensure the signaling WS is open. */
export async function connectSignaling(): Promise<void> {
    await commands.roomConnectSignaling();
}

/** Create a new room. Returns the server's RoomSummary. */
export async function createRoom(
    title: string,
    migrationEnabled: boolean,
): Promise<RoomSummaryIpc> {
    return await commands.roomCreate(title, migrationEnabled);
}

/** Join a room by 6-char code and display name. */
export async function joinRoom(
    code: string,
    displayName: string,
): Promise<RoomSummaryIpc> {
    return await commands.roomJoin(code, displayName);
}

/** Leave the current room. */
export async function leaveRoom(): Promise<void> {
    await commands.roomLeave();
}

/** Get the cached room summary, if any. */
export async function getRoomState(): Promise<RoomSummaryIpc | null> {
    return await commands.roomGetState();
}
