import { useEffect, useRef, useState, useCallback } from "react";
import { usePlaybackStore, type PlaybackKind } from "../stores/usePlaybackStore";
import { useRoomStore } from "../stores/useRoomStore";
import { sendPositionReport } from "../services/playback";
import { DrawingLayer } from "./DrawingLayer";

/**
 * P4-T02: the room media player.
 *
 * Renders a single `<video>` element inside the
 * `room-page__player` section. The player is driven by
 * the server-authoritative `playback://state` event
 * stream (P4-T01 + this task's Rust bridge).
 *
 * Authoritative ordering
 * ----------------------
 * The `usePlaybackStore` already enforces strict
 * `server_seq` ordering and a media-readiness buffer
 * (the most recent accepted event is parked in
 * `pending` until the `<video>` element fires
 * `canplay` or `loadedmetadata`). This component
 * reads `lastApplied` from the store and applies it
 * to the DOM via a `useEffect`.
 *
 * Media-readiness handling
 * ------------------------
 * The `<video>` element's `src` is whatever the parent
 * has written into `usePlaybackStore.mediaSrc` (a
 * `locast://...` URL or any other URL the parent can
 * resolve). When `src` changes, the element fires
 * `loadedmetadata` (or `canplay`) which flips
 * `mediaReady` to true. While `mediaReady` is false,
 * accepted events are parked; when it flips true, the
 * most recent parked event is applied.
 *
 * Feedback-loop prevention
 * ------------------------
 * The `<video>` element's `play` / `pause` / `seeked`
 * DOM event handlers MUST NOT bubble into a new
 * `PLAYBACK_CMD` envelope. This component does NOT
 * install any such handler. The host's UI controls
 * (see `PlaybackControls.tsx`) are the only legitimate
 * path that calls `sendPlaybackCommand`.
 *
 * Host echo suppression
 * ---------------------
 * When the host's client receives its own
 * `playback://state` event (the server rebroadcasts
 * to every participant), this component ignores the
 * event if the originating `sender_id` matches the
 * `localUserId` prop AND the local user is the host
 * (per the `isHost` prop). The host applied the
 * command locally before sending; the rebroadcast
 * is just the server's confirmation. This is purely
 * a no-op (we still record `lastAppliedServerSeq` so
 * stale events from a future host are dropped).
 */
export interface PlayerProps {
    /** The local user's user_id, used to detect host
     * echo. Pass `null` to disable host-echo
     * suppression. */
    localUserId?: string | null;
    /** Whether the local user is the current host. If
     * `false`, host-echo suppression is disabled. */
    isHost?: boolean;
    /** Optional shared ref that the parent (RoomPage) can
     *  read for drift-sampler telemetry. The parent
     *  supplies its own ref; Player's internal ref is
     *  used only if this is omitted. P4-T04. */
    videoRef?: React.MutableRefObject<HTMLVideoElement | null>;
}

export function Player({
    localUserId = null,
    isHost = false,
    videoRef,
}: PlayerProps): React.ReactNode {
    const localRef = useRef<HTMLVideoElement | null>(null);
    const ref = videoRef ?? localRef;
    const [errorMessage, setErrorMessage] = useState<string | null>(null);

    const lastApplied = usePlaybackStore((s) => s.lastApplied);
    const mediaReady = usePlaybackStore((s) => s.mediaReady);
    const mediaSrc = usePlaybackStore((s) => s.mediaSrc);
    const setMediaReady = usePlaybackStore((s) => s.setMediaReady);
    const markApplied = usePlaybackStore((s) => s.markApplied);

    // Wire the `<video>` element's `canplay` /
    // `loadedmetadata` events to `mediaReady = true`.
    const onCanPlay = useCallback(() => {
        setMediaReady(true);
    }, [setMediaReady]);
    const onLoadedMetadata = useCallback(() => {
        setMediaReady(true);
    }, [setMediaReady]);
    const onError = useCallback(() => {
        const v = ref.current;
        const msg = v?.error
            ? `media load failed (code ${v.error.code})`
            : "media load failed";
        setErrorMessage(msg);
    }, []);

    // Apply the latest accepted event to the
    // `<video>` element. The store already enforces
    // `server_seq` ordering, so the only work this
    // effect does is map the event into a DOM call.
    useEffect(() => {
        if (!lastApplied) return;
        const v = ref.current;
        if (!v) return;
        if (isHost && localUserId && lastApplied.sender_id === localUserId) {
            // Host echo: the host applied the change
            // locally before sending. Record the
            // server_seq and skip DOM mutation; the
            // <video> is already in the correct state.
            markApplied(lastApplied.server_seq);
            return;
        }
        // The wire unit is milliseconds; the DOM unit
        // is seconds. P4-T02 only deals with positions
        // in ms (the spec is ms end-to-end).
        const targetSec = lastApplied.media_position_ms / 1000;
        if (Math.abs(v.currentTime - targetSec) > 0.01) {
            v.currentTime = targetSec;
        }
        const kind: PlaybackKind = lastApplied.kind as PlaybackKind;
        if (kind === "play") {
            const res = v.play();
            if (res && typeof (res as Promise<void>).then === "function") {
                (res as Promise<void>).catch((err: unknown) => {
                    setErrorMessage(
                        err instanceof Error
                            ? `local play() rejected: ${err.message}`
                            : "local play() rejected",
                    );
                });
            }
        } else if (kind === "pause") {
            v.pause();
        }
        markApplied(lastApplied.server_seq);
    }, [lastApplied, localUserId, isHost, markApplied]);

    // Drain the `pending` slot when `mediaReady` flips
    // true.
    useEffect(() => {
        if (!mediaReady) return;
        const state = usePlaybackStore.getState();
        if (state.pending === null) return;
        if (state.lastApplied !== null) return;
        // Promote the parked event to `lastApplied`
        // AND clear the parked slot. Without the
        // clear, `pending` lingers forever in the
        // store (the drain guard `if
        // (state.lastApplied !== null) return` hides
        // the leak but does not free it).
        usePlaybackStore.setState({
            lastApplied: state.pending.event,
            pending: null,
        });
    }, [mediaReady]);

    // P4-T03: 1 Hz POSITION_REPORT reporter.
    // Lifecycle is bound to: mediaReady AND a stable
    // room id (summaryId). Room change / leave / unmount
    // / media-not-ready all stop the loop cleanly. The
    // timer source-of-truth is the live <video> element
    // (ref.current). Receiving a forwarded
    // POSITION_REPORT from another peer does NOT feed
    // back into this effect (it does not subscribe to
    // the position://report event); only the local
    // lifecycle + timer drive outbound reports.
    const summaryId = useRoomStore((s) => s.summary?.id ?? null);
    useEffect(() => {
        if (!mediaReady) return;
        if (summaryId === null) return;
        const v = ref.current;
        if (!v) return;
        const myRoom = summaryId;
        let stopped = false;
        const send = () => {
            if (stopped) return;
            // Bail out if the room changed since this
            // timer was scheduled.
            const cur = useRoomStore.getState().summary?.id ?? null;
            if (cur !== myRoom) {
                stopped = true;
                return;
            }
            const live = ref.current;
            if (!live) return;
            const media_position_ms = Math.max(0, Math.round(live.currentTime * 1000));
            const playing = !live.paused;
            sendPositionReport({ media_position_ms, playing }).catch((err: unknown) => {
                if (stopped) return;
                console.warn("position_report send failed", err);
            });
        };
        // First tick fires after 1 s. Approximate
        // cadence is acceptable for non-authoritative
        // telemetry.
        const handle = window.setInterval(send, 1000);
        return () => {
            stopped = true;
            window.clearInterval(handle);
        };
    }, [mediaReady, summaryId]);

return (
        <div className="room-page__player" data-testid="locast-player">
            {mediaSrc === null ? (
                <p className="room-page__player-empty">No media loaded yet.</p>
            ) : (
                <div className="room-page__player-stage" data-testid="locast-player-stage">
                    <video
                        ref={ref}
                        data-testid="locast-player-video"
                        src={mediaSrc}
                        controls
                        playsInline
                        onCanPlay={onCanPlay}
                        onLoadedMetadata={onLoadedMetadata}
                        onError={onError}
                        style={{ maxWidth: "100%", maxHeight: "100%" }}
                    />
                    {/* P5-T01: transparent drawing canvas overlaid
                       on the video. The canvas sits above the
                       video's pixel area (pointer-events: none
                       so the native <video controls> overlay
                       remains clickable). A future drawing-tool
                       task will switch this to pointer-events:
                       auto when the user enters pen mode. */}
                    <DrawingLayer videoRef={ref} userId={localUserId} />
                </div>
            )}
            {errorMessage !== null && (
                <p
                    className="room-page__player-empty"
                    role="status"
                    data-testid="locast-player-error"
                >
                    {errorMessage}
                </p>
            )}
        </div>
    );
}
