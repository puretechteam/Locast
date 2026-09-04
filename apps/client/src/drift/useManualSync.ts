/**
 * P4-T05: manual sync ("Sync to Host") target calculation
 * + DOM seek.
 *
 * Two-branch model, per architecture §12.5 and the
 * roadmap P4-T05 goal:
 *
 *  - VIEWER branch (the user is NOT the room's current
 *    host): a local-only DOM action. The user's
 *    `<video>.currentTime` is set to the host's
 *    expected position; no server round-trip, no
 *    `PLAYBACK_CMD` envelope emitted. The
 *    authoritative room playback state is unchanged.
 *  - HOST branch (the user IS the current host): the
 *    host's local `<video>.currentTime` is set to the
 *    host's expected position AND a `PLAYBACK_CMD`
 *    envelope is emitted (action = "seek") so that
 *    other peers converge to the same position. The
 *    host's monotonic sequence is taken from
 *    `usePlaybackStore.bumpHostSeq` to keep a single
 *    shared counter across the host's Play/Pause/Seek
 *    buttons and the Sync button.
 *
 * Capability model (architecture §14, P4-T01):
 * `playback.issue_commands` is a forward-looking
 * capability name; the implementation has only
 * `cap::PLAYBACK_CONTROL` (a u32 bitfield) on the
 * server side, not surfaced to the React layer. P4-T05
 * therefore gates the host branch on
 * `isHost` derived from the `RoomSummaryIpc` that the
 * server already populates -- this matches the
 * server-side cap gate (P4-T01) which also keys off
 * `is_host` and would reject any non-host `PLAYBACK_CMD`
 * with a single-caller `ROOM_ERROR(NotHost)`. A future
 * "capability task" can replace the gate with a true
 * `cap_set` check; until then, the React and Rust
 * checks are aligned.
 *
 * Target calculation: the host target is
 * `expectedPositionMs(hostCommand, nowMs, skewMs)` from
 * `drift.ts`. The `hostCommand` is read from
 * `usePlaybackStore.lastApplied` (the server-authoritative
 * `playback://state` event mirror) -- NOT from
 * `useViewerPositionStore.byUserId[hostUserId]`,
 * which is empty for the host (the WS forwarder's
 * originator filter prevents self-echo per P4-T03).
 * `skewMs` is 0 in v1; P4-T06 will populate it.
 *
 * No automatic correction: this hook is only called
 * from an explicit user action (SyncButton click or
 * DriftIndicator Resync click). It NEVER mutates the
 * authoritative playback state (`usePlaybackStore`),
 * and the local seek does not trigger any
 * `playback://state` rebroadcast because it does not
 * touch the server.
 *
 * Media bounds: the local seek targets
 * `host_target_ms / 1000` seconds. The browser clamps
 * out-of-range seeks (`currentTime` is silently
 * pinned to `[0, duration]`). A seek to a position
 * past `duration` will appear as "media ended" in
 * the `<video>` element; the host branch's `seek`
 * action will be rejected by the server's
 * lifecycle check if the room is not in
 * `Ready`/`Playing`/`Paused` (architecture §13).
 *
 * Re-entrancy: the hook's `localSeek` and
 * `authoritativeSeek` functions are async-safe but
 * the underlying DOM write is synchronous. A second
 * click during an in-flight host emit will bump
 * `bumpHostSeq` again (so each click gets a fresh
 * monotonic seq); the server processes them in order
 * and the last `seek` wins.
 */

import { useCallback } from "react";
import { usePlaybackStore, type PlaybackStateEvent } from "../stores/usePlaybackStore";
import { sendPlaybackCommand } from "../services/playback";
import {
    computeSyncTarget,
    expectedPositionMs,
    type ManualSyncTarget,
} from "./drift";
import { bumpLocalSeekTick, getForcedHostCommand } from "./testSeams";

export type { ManualSyncTarget } from "./drift";

/** P4-T05: shared helper that resolves the "current
 *  host command" for both `localSeek` and
 *  `authoritativeSeek`. In test mode the seam's
 *  `getForcedHostCommand` override takes precedence so
 *  tests can drive the hook without emitting a real
 *  `playback://state` event. In production the override
 *  is null and the function falls through to the
 *  authoritative `usePlaybackStore.lastApplied` event. */
function resolveHostCommand(
    storeLast: PlaybackStateEvent | null,
    roomId: string | null,
): {
    last: PlaybackStateEvent | null;
    kind: "play" | "pause" | "seek" | null;
} {
    const forced = getForcedHostCommand();
    if (forced !== null) {
        if (roomId === null) return { last: null, kind: null };
        // Synthesize a PlaybackStateEvent-like record
        // from the override so the rest of the hook
        // does not need to know the override exists.
        return {
            last: {
                room_id: roomId,
                server_seq: 0,
                server_ts_ms: forced.serverTsMs,
                sender_id: "test-seam",
                monotonic_seq: 0,
                kind: "play",
                media_position_ms: forced.mediaPositionMs,
            },
            kind: "play",
        };
    }
    if (storeLast === null) return { last: null, kind: null };
    if (roomId !== null && storeLast.room_id !== roomId) {
        return { last: null, kind: null };
    }
    return { last: storeLast, kind: storeLast.kind };
}

export interface ManualSyncResult extends ManualSyncTarget {
    /** Viewer branch: set the local `<video>.currentTime`
     *  to the host's expected position. Pure DOM, no
     *  server round-trip, no `usePlaybackStore`
     *  mutation. Resolves to `true` if the seek was
     *  attempted (the browser may still clamp to
     *  `[0, duration]`), `false` if the action was
     *  skipped (no host command, no video, no media,
     *  etc.). */
    localSeek: () => Promise<boolean>;
    /** Host branch: local seek + emit a
     *  `PLAYBACK_CMD{action:"seek"}` envelope through
     *  the existing `sendPlaybackCommand` IPC path.
     *  Resolves to `true` on the server's acceptance,
     *  `false` on cap-gate rejection or any other
     *  IPC error. The local seek is always attempted
     *  (matches the existing host-echo behavior: the
     *  host's local `<video>` element is moved
     *  immediately; the rebroadcast is suppressed by
     *  Player.tsx's host-echo check). */
    authoritativeSeek: () => Promise<boolean>;
}

/** The hook. Pulls the live state from the stores and
 *  returns a stable `ManualSyncResult`. */
export function useManualSync(args: {
    roomId: string | null;
    isHost: boolean;
    /** Function that returns the live `<video>` element
     *  (or null when no element is mounted yet). The
     *  caller owns the ref. */
    getVideo: () => HTMLVideoElement | null;
    /** Skew offset (server - local), ms. Defaults to 0
     *  in v1; P4-T06 will populate. */
    skewMs?: number;
}): ManualSyncResult {
    const { roomId, isHost, getVideo, skewMs = 0 } = args;

    const target: ManualSyncTarget = (() => {
        // Re-derive on every render. The call is cheap
        // (one subtraction + one Math.max) and the
        // stores only update on real changes.
        const state = usePlaybackStore.getState();
        // P4-T05 test-mode override: when a test has
        // forced a synthetic host command via the seam,
        // honor it. This is the test-only path; in
        // production `getForcedHostCommand` returns null.
        const forced = getForcedHostCommand();
        const lastApplied =
            forced !== null
                ? {
                      room_id: roomId ?? "",
                      media_position_ms: forced.mediaPositionMs,
                      server_ts_ms: forced.serverTsMs,
                  }
                : state.lastApplied;
        return computeSyncTarget({
            roomId,
            isHost,
            lastApplied,
            mediaReady: state.mediaReady,
            nowMs: Date.now(),
            skewMs,
        });
    })();

    const localSeek = useCallback(async (): Promise<boolean> => {
        const state = usePlaybackStore.getState();
        if (state.mediaReady === false) return false;
        if (roomId === null) return false;
        const { last, kind } = resolveHostCommand(state.lastApplied, roomId);
        if (last === null) return false;
        const targetMs = expectedPositionMs(
            {
                mediaPositionMs: last.media_position_ms,
                serverTsMs: last.server_ts_ms,
            },
            Date.now(),
            skewMs,
        );
        if (targetMs === null) return false;
        const v = getVideo();
        if (v === null) return false;
        // Convert ms -> seconds. The browser's seek is
        // a no-op when target == currentTime within
        // the platform's tolerance, which is exactly
        // what we want (no spurious `seeked` event).
        // The browser clamps out-of-range values; we
        // do not pre-validate against `duration` so a
        // zero-duration asset (rare) does not block
        // the user's manual sync.
        const targetSec = targetMs / 1000;
        if (Math.abs(v.currentTime - targetSec) > 0.001) {
            v.currentTime = targetSec;
        }
        // P4-T05 test seam: the Vite harness cannot observe
        // the actual `<video>.currentTime` write (Chromium
        // ignores `currentTime` assignments on a video whose
        // source has not loaded). Bump a module-level
        // counter so the e2e tests can assert "local seek
        // ran" without reading the DOM. The helper is a
        // no-op in production (gated on `MODE === "test"`).
        bumpLocalSeekTick();
        // If the host is currently PLAYING and the
        // local media was paused, restore the playing
        // state so the user resumes in sync. This is
        // the local-equivalent of the architecture's
        // §13 SEEK semantics: "resume previous
        // play/pause state". We only call play() when
        // the host's last event says play (a host
        // authoritative SEEK after PAUSE is a "seek
        // while paused" in the host's UI, so the
        // viewer should stay paused after sync).
        if (kind === "play" && v.paused) {
            const res = v.play();
            if (res && typeof (res as Promise<void>).then === "function") {
                (res as Promise<void>).catch((err: unknown) => {
                    // eslint-disable-next-line no-console
                    console.warn("local play() after sync rejected", err);
                });
            }
        }
        return true;
    }, [roomId, getVideo, skewMs]);

    const authoritativeSeek = useCallback(async (): Promise<boolean> => {
        // Step 1: local seek (same as the viewer branch;
        // matches the existing host-echo behavior where
        // the host's local <video> is moved immediately
        // and the server's rebroadcast is suppressed).
        const localOk = await localSeek();
        if (localOk === false) return false;
        // Step 2: emit PLAYBACK_CMD{action:"seek"} through
        // the existing IPC path. The host's monotonic
        // seq is read+advanced atomically in
        // `bumpHostSeq` so a click during an in-flight
        // emit gets a fresh seq.
        const state = usePlaybackStore.getState();
        const { last } = resolveHostCommand(state.lastApplied, roomId);
        if (last === null) return false;
        const seq = state.bumpHostSeq();
        const targetMs = expectedPositionMs(
            {
                mediaPositionMs: last.media_position_ms,
                serverTsMs: last.server_ts_ms,
            },
            Date.now(),
            skewMs,
        );
        if (targetMs === null) return false;
        try {
            await sendPlaybackCommand({
                action: "seek",
                monotonic_seq: seq,
                media_position_ms: targetMs,
            });
            return true;
        } catch (err) {
            // eslint-disable-next-line no-console
            console.warn("manual sync seek failed", err);
            return false;
        }
    }, [localSeek, roomId, skewMs]);

    return {
        ...target,
        localSeek,
        authoritativeSeek,
    };
}
