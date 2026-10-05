import { useCallback } from "react";
import { usePlaybackStore, type PlaybackKind } from "../stores/usePlaybackStore";
import { useRoomStore } from "../stores/useRoomStore";
import {
    hostCommandAllowed,
    sendHostPlaybackCommand,
} from "../services/hostPlayback";

/**
 * P4-T02: host-only playback control buttons.
 *
 * Renders Play / Pause / Seek-to-60s buttons. The host
 * (only) is allowed to send `PLAYBACK_CMD` envelopes;
 * the server's cap gate (P4-T01) rejects non-host
 * callers with a single-caller `ROOM_ERROR(NotHost)`.
 *
 * Every send goes through `sendHostPlaybackCommand`
 * (`services/hostPlayback.ts`): the command is applied to
 * the host's own `<video>` first (the server never echoes
 * a PLAYBACK_CMD back to its sender), then sent with the
 * shared host `monotonic_seq` (`usePlaybackStore.bumpHostSeq`,
 * also used by the Sync button's host branch), then
 * recorded as the host's `lastApplied`.
 *
 * PLAY and PAUSE carry the host's actual `<video>`
 * position, so viewers land where the host is. Pause and
 * Seek stay disabled until the room has a known command:
 * the server rejects them while the room is still `Open`.
 */
export interface PlaybackControlsProps {
    /** Whether the local user is the current host. If
     * `false`, the controls render as disabled and
     * the `send` calls are blocked. */
    isHost: boolean;
    /** The last known room position in milliseconds, used
     * for PLAY / PAUSE only when the host's `<video>` is not
     * mounted. */
    positionMs: number;
    /** The local user's id, stamped on the host's recorded
     * command so the Player's host-echo check recognizes it. */
    localUserId?: string | null;
    /** The host's live `<video>` element (null while no
     * media is mounted). */
    getVideo?: () => HTMLVideoElement | null;
}

export function PlaybackControls({
    isHost,
    positionMs,
    localUserId = null,
    getVideo,
}: PlaybackControlsProps): React.ReactNode {
    const roomId = useRoomStore((s) => s.summary?.id ?? null);
    // Re-render when a command lands so Pause / Seek enable.
    const hasRoomCommand = usePlaybackStore(
        (s) => s.lastApplied !== null && s.lastApplied.room_id === roomId,
    );

    const send = useCallback(
        (kind: PlaybackKind, fixedPositionMs?: number): void => {
            if (!isHost || roomId === null) return;
            if (!hostCommandAllowed(kind, roomId)) return;
            const video = getVideo?.() ?? null;
            const mediaPositionMs =
                fixedPositionMs ??
                (video !== null ? video.currentTime * 1000 : positionMs);
            void sendHostPlaybackCommand({
                roomId,
                localUserId,
                kind,
                mediaPositionMs,
                video,
            });
        },
        [isHost, roomId, localUserId, getVideo, positionMs],
    );

    const onPlay = useCallback(() => {
        send("play");
    }, [send]);
    const onPause = useCallback(() => {
        send("pause");
    }, [send]);
    // SEEK to 60 s is the literal P4-T02 acceptance
    // test target. The host can also use the
    // <video> element's native scrubber to seek; the
    // scrubber is NOT wired to send PLAYBACK_CMD (only
    // these buttons and the Sync button send).
    const onSeek60 = useCallback(() => {
        send("seek", 60_000);
    }, [send]);
    const needsPlayFirst = isHost && !hasRoomCommand;
    const playFirstHint = needsPlayFirst ? "Press Play first" : undefined;

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
                disabled={!isHost || needsPlayFirst}
                title={playFirstHint}
                data-testid="locast-playback-pause"
            >
                Pause
            </button>
            <button
                type="button"
                onClick={onSeek60}
                disabled={!isHost || needsPlayFirst}
                title={playFirstHint}
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
