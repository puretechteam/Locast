import { useCallback, useRef } from "react";
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
    const nextSeq = useRef<number>(1);

    const send = useCallback(
        async (
            action: "play" | "pause" | "seek",
            media_position_ms: number,
        ): Promise<void> => {
            if (!isHost) return;
            const seq = nextSeq.current;
            try {
                await sendPlaybackCommand({
                    action,
                    monotonic_seq: seq,
                    media_position_ms,
                });
                // Only advance the sequence on
                // server-accepted commands. The Tauri
                // command throws on cap-gate rejection
                // or monotonic_seq validation failure,
                // so reaching this line means the
                // command was accepted.
                nextSeq.current = seq + 1;
            } catch (err) {
                // Surface to the host's console. The
                // host can retry; the server's
                // last_acked_seq is unchanged so a
                // retry with the same `seq` is valid
                // and not a duplicate.
                // eslint-disable-next-line no-console
                console.warn("playback_send failed", err);
            }
        },
        [isHost],
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
