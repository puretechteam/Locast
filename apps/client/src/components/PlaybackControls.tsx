import { useCallback } from "react";
import { usePlaybackStore } from "../stores/usePlaybackStore";
import { sendPlaybackCommand } from "../services/playback";

/**
 * P4-T02: host-only playback control buttons.
 *
 * Renders Play / Pause / Seek-to-60s buttons. The host
 * (only) is allowed to send `PLAYBACK_CMD` envelopes;
 * the server's cap gate (P4-T01) rejects non-host
 * callers with a single-caller `ROOM_ERROR(NotHost)`.
 *
 * The host's `monotonic_seq` is tracked locally in a
 * `useRef`. The host is the only sender that needs a
 * monotonic sequence; the server's per-sender table
 * (`last_acked_seq`) keys on `user_id` (not pubkey),
 * so the sequence survives the host's own reconnect
 * within a single session but resets to 0 across
 * host migrations (the new host's first command is
 * always `monotonic_seq = 1`).
 *
 * The host's local `<video>` element applies the
 * command locally before `sendPlaybackCommand`
 * resolves; the server's rebroadcast is ignored by the
 * Player's host-echo suppression (see `Player.tsx`).
 */
export interface PlaybackControlsProps {
    /** Whether the local user is the current host. If
     * `false`, the controls render as disabled and
     * the `send` calls are blocked. */
    isHost: boolean;
    /** The current media position in milliseconds, for
     * display + a sensible default SEEK value. The
     * server is the authority; the host's UI shows
     * whatever the most recently applied event
     * carried. */
    positionMs: number;
}

export function PlaybackControls({
    isHost,
    positionMs,
}: PlaybackControlsProps): React.ReactNode {
    // P4-T05: the host's monotonic sequence is now
    // shared across all host-authoritative emit paths
    // (the existing Play/Pause/Seek buttons here AND
    // the Sync button's host branch) via
    // `usePlaybackStore.bumpHostSeq`. We read+advance
    // atomically in `send` and on server rejection the
    // counter is NOT rolled back, so a retry must NOT
    // call `bumpHostSeq` again -- it must reuse the
    // same `seq`. The local `useRef` counter that lived
    // here previously is removed; the store is the
    // single source of truth.
    const bumpHostSeq = usePlaybackStore((s) => s.bumpHostSeq);

    const send = useCallback(
        async (
            action: "play" | "pause" | "seek",
            media_position_ms: number,
        ): Promise<void> => {
            if (!isHost) return;
            const seq = bumpHostSeq();
            try {
                await sendPlaybackCommand({
                    action,
                    monotonic_seq: seq,
                    media_position_ms,
                });
                // Reaching this line means the server
                // accepted the command. The counter has
                // already been advanced by `bumpHostSeq`
                // above; on rejection it is left
                // advanced so the next emit does NOT
                // reuse this seq (the server already
                // rejected it as out-of-order or
                // invalid). The caller can recover by
                // resetting the store on room change.
            } catch (err) {
                // Surface to the host's console. The
                // host can recover by leaving/rejoining
                // the room which resets `hostNextSeq` via
                // `setRoomId` -> `clear()`.
                // eslint-disable-next-line no-console
                console.warn("playback_send failed", err);
            }
        },
        [isHost, bumpHostSeq],
    );

    const onPlay = useCallback(() => {
        void send("play", positionMs);
    }, [send, positionMs]);
    const onPause = useCallback(() => {
        void send("pause", positionMs);
    }, [send, positionMs]);
    // SEEK to 60 s is the literal P4-T02 acceptance
    // test target. The host can also use the
    // <video> element's native scrubber to seek; the
    // scrubber is NOT wired to send PLAYBACK_CMD in
    // P4-T02 (that is a P4-T05 / "manual sync" task).
    const onSeek60 = useCallback(() => {
        void send("seek", 60_000);
    }, [send]);

    return (
        <div className="playback-controls" data-testid="locast-playback-controls">
            <button
                type="button"
                onClick={onPlay}
                disabled={!isHost}
                data-testid="locast-playback-play"
            >
                Play
            </button>
            <button
                type="button"
                onClick={onPause}
                disabled={!isHost}
                data-testid="locast-playback-pause"
            >
                Pause
            </button>
            <button
                type="button"
                onClick={onSeek60}
                disabled={!isHost}
                data-testid="locast-playback-seek60"
            >
                Seek 60s
            </button>
            {!isHost && (
                <span className="playback-controls__hint">
                    (host only)
                </span>
            )}
        </div>
    );
}
