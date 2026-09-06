// apps/client/src/services/permissions.ts
//
// P6-T02: client-side service for granting and revoking capabilities.
//
// The host calls `grantCapability(userId, cap)` to grant a capability
// to a participant, or `revokeCapability(userId, cap)` to revoke it.
// The server validates the caller is the host, applies the mutation,
// and broadcasts a `CAPABILITY_UPDATE` to all participants.

import { commands } from "./ipc";

export const CAP = {
    PLAYBACK_CONTROL: 0x01,
    DRAW: 0x02,
    LASER: 0x04,
    MANAGE_ROOM: 0x08,
    KICK: 0x10,
    PUBLISH_MANIFEST: 0x20,
    INVITE: 0x40,
    CHAT: 0x80,
} as const;

export type Cap = (typeof CAP)[keyof typeof CAP];

export async function grantCapability(targetUserId: string, cap: Cap): Promise<void> {
    await commands.roomPermissionSet(targetUserId, cap, 0);
}

export async function revokeCapability(targetUserId: string, cap: Cap): Promise<void> {
    await commands.roomPermissionSet(targetUserId, 0, cap);
}
