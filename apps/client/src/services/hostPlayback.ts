// apps/client/src/services/hostPlayback.ts
//
// The host side of PLAYBACK_CMD, shared by every host-
// authoritative emit path (the PlaybackControls buttons and
// the manual-sync host branch).
//
// The server never echoes a PLAYBACK_CMD to the host that
// sent it (originator suppression in the room broadcast), so
// the host cannot wait for its own `playback://state` event
// the way a viewer does. Instead the host:
//   1. applies the command to its own <video> first,
//   2. sends it with the next shared `monotonic_seq`,
//   3. records it in the playback store as `lastApplied`, so
//      the host has the same "last host command" a viewer
//      gets from the server (manual sync, drift and the
//      controls' position all read it). The Player's host-
//      echo check keeps that record from touching the DOM.

import { usePlaybackStore, type PlaybackKind } from "../stores/usePlaybackStore";
import { useClockSkewStore } from "../stores/useClockSkewStore";
import { sendPlaybackCommand } from "./playback";

/** Apply one playback command to a <video> element: move to
 *  `mediaPositionMs` when it is more than 10 ms away, then
 *  play or pause (a SEEK keeps the current play state). The
 *  `play()` promise rejection (autoplay policy, no source) is
 *  handed to `onPlayRejected`. */
export function applyPlaybackToVideo(
    v: HTMLVideoElement,
    kind: PlaybackKind,
    mediaPositionMs: number,
    onPlayRejected?: (err: unknown) => void,
): void {
    // The wire unit is milliseconds; the DOM unit is seconds.
    const targetSec = mediaPositionMs / 1000;
    if (Math.abs(v.currentTime - targetSec) > 0.01) {
        v.currentTime = targetSec;
    }
    if (kind === "play") {
        const res = v.play();
        if (res && typeof (res as Promise<void>).then === "function") {
            (res as Promise<void>).catch((err: unknown) => {
                onPlayRejected?.(err);
            });
        }
    } else if (kind === "pause") {
        v.pause();
    }
}

/** Whether the server can accept `kind` from the host right
 *  now. A room starts `Open` and the server rejects PAUSE and
 *  SEEK until the first PLAY (architecture 11.1). Any accepted
 *  command (the host's own, or one from a previous host)
 *  means the room has left `Open`. Without this check a
 *  premature PAUSE / SEEK would consume a `monotonic_seq`
 *  the server never acknowledges, and every later command
 *  would be rejected as a gap. */
export function hostCommandAllowed(kind: PlaybackKind, roomId: string | null): boolean {
    if (roomId === null) return false;
    if (kind === "play") return true;
    const last = usePlaybackStore.getState().lastApplied;
    return last !== null && last.room_id === roomId;
}

/** Send one host PLAYBACK_CMD. `video`, when given, gets the
 *  command applied before the send (pass null when the caller
 *  already moved it). Resolves `true` once the envelope is
 *  handed to the signaling socket; `false` when it could not
 *  be sent, in which case its `monotonic_seq` is given back.
 *  The server's verdict is asynchronous and does not reach
 *  this function. */
export async function sendHostPlaybackCommand(args: {
    roomId: string;
    localUserId: string | null;
    kind: PlaybackKind;
    mediaPositionMs: number;
    video: HTMLVideoElement | null;
}): Promise<boolean> {
    const { roomId, localUserId, kind, video } = args;
    const mediaPositionMs = Math.max(0, Math.round(args.mediaPositionMs));
    if (video !== null) {
        applyPlaybackToVideo(video, kind, mediaPositionMs, (err) => {
            // eslint-disable-next-line no-console
            console.warn("host play() rejected", err);
        });
    }
    const store = usePlaybackStore.getState();
    const seq = store.bumpHostSeq();
    try {
        await sendPlaybackCommand({
            action: kind,
            monotonic_seq: seq,
            media_position_ms: mediaPositionMs,
        });
    } catch (err) {
        usePlaybackStore.getState().rollbackHostSeq(seq);
        // eslint-disable-next-line no-console
        console.warn("playback_send failed", err);
        return false;
    }
    // Stamp the record with the host's estimate of server
    // time (skew = server - local) so expected-position math
    // treats it like a server-stamped event.
    const skewMs = useClockSkewStore.getState().skewMs ?? 0;
    usePlaybackStore.getState().recordHostCommand({
        room_id: roomId,
        server_seq: 0,
        server_ts_ms: Date.now() + skewMs,
        sender_id: localUserId ?? "",
        monotonic_seq: seq,
        kind,
        media_position_ms: mediaPositionMs,
    });
    return true;
}
