// apps/client/src/services/playback.ts
//
// Typed wrapper over the Rust playback IPC surface (P4-T02).
// Mirrors the shape of `services/downloads.ts`:
// - `onPlaybackState(handler)` subscribes to the
//   `playback://state` Tauri event emitted by the
//   server-authoritative playback stream.
// - `sendPlaybackCommand(cmd)` calls the host-only
//   `playback_send` Tauri command. Only the room's
//   current host should call this. The server's
//   cap gate (P4-T01) rejects non-host callers.

import { listenEvent } from "./_eventTransport";
import { commands } from "./ipc";
import type {
    PlaybackCommandInput,
    PlaybackSendResult,
    PlaybackStateEvent,
} from "../bindings";

export type { PlaybackStateEvent, PlaybackCommandInput, PlaybackSendResult };

/**
 * Subscribe to `playback://state`. The handler receives
 * every accepted server playback event in arrival order.
 * The handler is responsible for `server_seq` ordering
 * (the `usePlaybackStore` enforces this).
 *
 * Returns an unsubscribe function.
 */
export async function onPlaybackState(
    handler: (e: PlaybackStateEvent) => void,
): Promise<() => void> {
    return await listenEvent<PlaybackStateEvent>("playback://state", handler);
}

/**
 * Host-only: send a `PLAYBACK_CMD` envelope through the
 * signaling WebSocket. The server (P4-T01) validates
 * the host authority, the room lifecycle, and the
 * per-sender `monotonic_seq`; on success it rebroadcasts
 * the accepted event to every room participant.
 *
 * The caller is responsible for tracking its own
 * `monotonic_seq` (the React store does not do this for
 * the host because the host is the originator).
 *
 * Throws on cap-gate rejection (non-host) or
 * `monotonic_seq` validation failure: the Tauri command
 * returns an `AppError` and the React caller surfaces
 * it. The caller should NOT swallow the error silently
 * because that would let the host's monotonic sequence
 * drift out of sync with the server's `last_acked_seq`.
 */
export async function sendPlaybackCommand(
    cmd: PlaybackCommandInput,
): Promise<PlaybackSendResult> {
    return await commands.playbackSend(cmd);
}
