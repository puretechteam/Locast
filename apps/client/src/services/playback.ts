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
//
// P4-T03 additions:
// - `onPositionReport(handler)` subscribes to the
//   `position://report` Tauri event emitted whenever a
//   remote participant's POSITION_REPORT arrives at the
//   client (after the server forwards it).
// - `sendPositionReport(input)` calls the per-participant
//   `position_report` Tauri command; any room member may
//   call it (the cadence is owned by the React layer;
//   see `apps/client/src/components/Player.tsx`).

import { listenEvent } from "./_eventTransport";
import { commands } from "./ipc";
import type {
    PlaybackCommandInput,
    PlaybackSendResult,
    PlaybackStateEvent,
    PositionReportEvent,
    PositionReportInput,
    PositionReportResult,
} from "../bindings";

export type {
    PlaybackStateEvent,
    PlaybackCommandInput,
    PlaybackSendResult,
    PositionReportEvent,
    PositionReportInput,
    PositionReportResult,
};

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

/**
 * P4-T03: subscribe to `position://report`. The handler
 * receives every forwarded POSITION_REPORT from a
 * remote participant. The server stamps the originator's
 * `user_id` into the event's `sender_id` field so the
 * React layer can key positions by sender and keep
 * multiple viewers distinguishable. The local 1 Hz
 * reporter does NOT subscribe to this event (it only
 * emits); receiving a report never produces another
 * outbound report.
 *
 * Returns an unsubscribe function.
 */
export async function onPositionReport(
    handler: (e: PositionReportEvent) => void,
): Promise<() => void> {
    return await listenEvent<PositionReportEvent>("position://report", handler);
}

/**
 * P4-T03: send one POSITION_REPORT envelope. Any room
 * member (host or viewer) may call this; the server's
 * cap gate requires only that the caller is currently
 * a member of the named room (see
 * `apps/server/src/rooms/caps.rs::Command::PositionReport`).
 * The server forwards the report to every other
 * participant; the WS layer's originator filter
 * suppresses the echo to the sender so the local
 * client never sees its own report.
 *
 * The cadence is owned by the React layer (the 1 Hz
 * `useEffect` in `Player.tsx`); this is a single-shot
 * call. Errors from the Tauri command surface to the
 * caller via the rejected Promise; the caller may log
 * or swallow at its discretion.
 */
export async function sendPositionReport(
    input: PositionReportInput,
): Promise<PositionReportResult> {
    return await commands.positionReport(input);
}
