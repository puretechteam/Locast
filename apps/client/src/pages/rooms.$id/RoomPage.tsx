import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Link, useParams } from "react-router-dom";
import { events } from "../../services/ipc";
import { getRoomState } from "../../services/room";
import { getSignalingState } from "../../services/signaling";
import type { ConnectionState, RoomSummaryIpc } from "../../services/room";
import { useRoomStore } from "../../stores/useRoomStore";
import { usePlaybackStore } from "../../stores/usePlaybackStore";
import { Player } from "../../components/Player";
import { PlaybackControls } from "../../components/PlaybackControls";
import { DriftIndicator } from "../../components/DriftIndicator";
import { useDriftSmoother } from "../../drift/useDriftSmoother";
import { usePlaybackEventBridge } from "../../hooks/usePlaybackEventBridge";
import { usePositionReportBridge } from "../../hooks/usePositionReportBridge";
import { useViewerPositionStore } from "../../stores/useViewerPositionStore";
import { ParticipantStrip } from "./ParticipantStrip";
import { RoomFooter } from "./RoomFooter";

export function RoomPage(): JSX.Element {
    const params = useParams<{ id: string }>();
    const summary = useRoomStore((s) => s.summary);
    const signaling = useRoomStore((s) => s.signaling);
    const setSummary = useRoomStore((s) => s.setSummary);
    const setSignaling = useRoomStore((s) => s.setSignaling);
    const clear = useRoomStore((s) => s.clear);
    const [hydrated, setHydrated] = useState(false);

    // P4-T02: on leave, reset BOTH the room store's
    // `summary` AND the playback store's mediaSrc /
    // mediaReady / parked event / server_seq counter.
    // The bridge's `setRoomId` effect also clears the
    // server_seq / pending / lastApplied fields when
    // `roomId` changes to `null`, but the playback
    // store's `clear` additionally resets
    // `mediaReady` + `mediaSrc` + `suppressLocalEcho`
    // so a re-join starts from a clean slate.
const handleLeft = useCallback(() => {
        usePlaybackStore.getState().clear();
        // P4-T03: clear the per-viewer position cache
        // on leave so a re-join starts fresh.
        useViewerPositionStore.getState().clear();
        clear();
    }, [clear]);

    // P4-T02 test seam: in Vite's test mode, expose
    // `useRoomStore.setSummary` on `window.__locastRoomStore`
    // so the Playwright harness can mount the room
    // without going through the Tauri invoke surface.
    useEffect(() => {
        if (import.meta.env.MODE !== "test") return;
        const w = window as unknown as {
            __locastRoomStore?: { setSummary: (s: unknown) => void };
        };
        w.__locastRoomStore = {
            setSummary: (s) => setSummary(s as Parameters<typeof setSummary>[0]),
        };
        return () => {
            if (w.__locastRoomStore) delete w.__locastRoomStore;
        };
    }, [setSummary]);

    // P4-T02: bridge playback://state events into the
    // playback store and mirror the current room id
    // into it. The hook returns `null` (no JSX).
    usePlaybackEventBridge();

    // P4-T03: bridge position://report events into the
    // per-viewer position store. The hook returns
    // `null` (no JSX).
    usePositionReportBridge();

    // P4-T02: derive `isHost` and `localUserId` from
    // the cached room summary. The "local user" is
    // the participant whose `user_id` matches
    // `summary.host_user_id`; if no participant is
    // marked as host in the snapshot (race during
    // host migration), fall back to "not the host".
    const { isHost, localUserId, hostPositionMs } = useMemo(() => {
        const host = summary?.participants.find((p) => p.is_host);
        const isHostFlag = host !== undefined && summary !== null;
        const userId = isHostFlag ? (host?.user_id ?? null) : null;
        const pos = usePlaybackStore.getState().lastApplied?.media_position_ms ?? 0;
        return {
            isHost: isHostFlag,
            localUserId: userId,
            hostPositionMs: pos,
        };
    }, [summary]);
const lastApplied = usePlaybackStore((s) => s.lastApplied);
    const displayPositionMs = lastApplied?.media_position_ms ?? hostPositionMs;

    // P4-T03: per-viewer position snapshot for the
    // host's UI. Each row is keyed by the viewer's
    // user_id; the host can see all viewers in the
    // room (participants minus the host). We exclude
    // the host's own user_id so the host does not
    // appear in its own "viewers" list -- the host's
    // position is shown via the server-authoritative
    // `displayPositionMs` above.
    const viewerPositions = useViewerPositionStore((s) => s.byUserId);

    // P4-T04: the drift sampler. The smoother reads
    // the local media position from a shared ref
    // (`videoRef`) that the same `<video>` element
    // Player renders. The ref is owned here so the
    // smoother can read DOM state without coupling to
    // Player's internal hooks. The room id is the
    // gate: when it changes, the smoother resets its
    // EMA state so old samples cannot leak across
    // rooms.
    const videoRef = useRef<HTMLVideoElement | null>(null);
    const drift = useDriftSmoother({
        roomId: summary?.id ?? null,
        getLocalMs: () => {
            const v = videoRef.current;
            if (v === null) return null;
            // `currentTime` is a non-negative double;
            // we round to integer ms for the wire unit
            // and to keep the EMA inputs integer-valued.
            return Math.max(0, Math.round(v.currentTime * 1000));
        },
        getLocalPlaying: () => {
            const v = videoRef.current;
            if (v === null) return false;
            return !v.paused;
        },
        remoteParticipants: Object.values(viewerPositions).map((p) => ({
            userId: p.userId,
            mediaPositionMs: p.mediaPositionMs,
            playing: p.playing,
            receivedAtMs: p.receivedAtMs,
        })),
        localUserId,
    });

    // P4-T04: Resync button stub. The actual seek-to-host
    // behavior ships in P4-T05 (manual sync). The button
    // is wired so the UI surface matches architecture
    // §25.3.2 today; the underlying handler is a no-op
    // until P4-T05 lands. The stub is intentionally
    // side-effect-free so it can be removed without
    // migration.
    const onResync = useCallback(() => {
        // P4-T05 will replace this with a real seek
        // path. Until then, the click is a no-op.
    }, []);

    useEffect(() => {
        let cancelled = false;
        const unlistens: Array<() => void> = [];

        (async () => {
            try {
                const state = await getRoomState();
                if (cancelled) return;
                if (state !== null) {
                    setSummary(state);
                }

                const initialSignaling = await getSignalingState();
                if (cancelled) return;
                setSignaling(initialSignaling);

                const u1 = await events.signalingState((next: ConnectionState) => {
                    if (cancelled) return;
                    setSignaling(next);
                });
                if (cancelled) {
                    u1();
                    return;
                }
                unlistens.push(u1);

                const u2 = await events.roomState((next: RoomSummaryIpc | null) => {
                    if (cancelled) return;
                    setSummary(next);
                });
                if (cancelled) {
                    u2();
                    return;
                }
                unlistens.push(u2);

                const u3 = await events.roomEvent((next: RoomSummaryIpc) => {
                    if (cancelled) return;
                    setSummary(next);
                });
                if (cancelled) {
                    u3();
                    return;
                }
                unlistens.push(u3);
            } finally {
                if (!cancelled) {
                    setHydrated(true);
                }
            }
        })().catch((err: unknown) => {
            if (!cancelled) {
                const detail = err instanceof Error ? err.message : String(err);
                console.error("RoomPage: failed to subscribe", detail);
                setHydrated(true);
            }
        });

        return () => {
            cancelled = true;
            while (unlistens.length > 0) {
                const u = unlistens.pop();
                if (u) u();
            }
        };
    }, [setSummary, setSignaling]);

    if (!hydrated) {
        return (
            <div className="room-page room-page--loading">
                <p>Loading room...</p>
            </div>
        );
    }

    if (summary === null) {
        return (
            <div className="room-page room-page--empty" data-testid="room-empty">
                <p>Not in a room.</p>
                <p>
                    <Link to="/rooms/new">Create a room</Link> or{" "}
                    <Link to="/rooms/join">join one</Link>.
                </p>
                {params.id !== undefined && (
                    <p className="room-page__hint">
                        (URL id: <code>{params.id}</code>)
                    </p>
                )}
            </div>
        );
    }

    const expectedId = params.id;
    const idMismatch =
        expectedId !== undefined && expectedId.length > 0 && expectedId !== summary.id;

return (
        <div className="room-page">
            <Player localUserId={localUserId} isHost={isHost} videoRef={videoRef} />
            <ParticipantStrip summary={summary} />
            {/* P4-T04: drift indicator. Hidden by
             * default; only renders when the smoothed
             * offset exceeds 2.0 s. Non-blocking; the
             * user is notified but playback is NOT
             * auto-corrected. */}
            <DriftIndicator sample={drift} onResync={onResync} />
            {isHost && (
                <section
                    className="room-page__viewer-positions"
                    data-testid="viewer-positions"
                    aria-label="Viewer positions"
                >
                    <h3 className="room-page__viewer-positions-title">
                        Viewer positions
                    </h3>
                    {/* P4-T04: room median surface
                     * (architecture §25.3.4 "thin marker
                     * for the median participant
                     * position"). Until the full
                     * project-owned seek bar lands in a
                     * later task, the median is rendered
                     * here as a labeled line so the host
                     * can see the room's central
                     * position alongside each viewer's
                     * row. The label is hidden when no
                     * valid (playing + fresh) report is
                     * available. */}
                    {drift.roomMedianMs !== null && (
                        <div
                            className="room-page__viewer-positions-median"
                            data-testid="room-median"
                        >
                            <span className="room-page__viewer-positions-median-label">
                                Room median
                            </span>
                            <span className="room-page__viewer-positions-median-value">
                                {(drift.roomMedianMs / 1000).toFixed(1)}s
                            </span>
                            {drift.driftVsMedianMs !== null && (
                                <span
                                    className="room-page__viewer-positions-median-drift"
                                    data-testid="room-median-drift"
                                    data-direction={drift.direction}
                                >
                                    {drift.direction === "ahead"
                                        ? "ahead"
                                        : drift.direction === "behind"
                                          ? "behind"
                                          : "aligned"}
                                    {" "}
                                    {Math.abs(drift.driftVsMedianMs / 1000).toFixed(1)}s
                                </span>
                            )}
                        </div>
                    )}
                    <ul className="room-page__viewer-positions-list">
                        {Object.values(viewerPositions)
                            .filter((v) => v.userId !== localUserId)
                            .map((v) => {
                                const ageSec = Math.max(
                                    0,
                                    Math.round(
                                        (Date.now() - v.receivedAtMs) / 1000,
                                    ),
                                );
                                const posSec = (v.mediaPositionMs / 1000).toFixed(
                                    1,
                                );
                                return (
                                    <li
                                        key={v.userId}
                                        className="room-page__viewer-position-row"
                                        data-testid="viewer-position-row"
                                        data-sender-id={v.userId}
                                    >
                                        <span className="room-page__viewer-position-user">
                                            {v.userId.slice(0, 8)}
                                        </span>
                                        <span className="room-page__viewer-position-time">
                                            {posSec}s
                                        </span>
                                        <span className="room-page__viewer-position-state">
                                            {v.playing ? "playing" : "paused"}
                                        </span>
                                        <span className="room-page__viewer-position-age">
                                            {ageSec}s ago
                                        </span>
                                    </li>
                                );
                            })}
                        {Object.values(viewerPositions).filter(
                            (v) => v.userId !== localUserId,
                        ).length === 0 && (
                            <li
                                className="room-page__viewer-position-empty"
                                data-testid="viewer-positions-empty"
                            >
                                No viewer position reports yet.
                            </li>
                        )}
                    </ul>
                </section>
            )}
            <PlaybackControls
                isHost={isHost}
                positionMs={displayPositionMs}
            />
            <RoomFooter
                summary={summary}
                signaling={signaling}
                onLeft={handleLeft}
            />
            {idMismatch && (
                <p className="room-page__hint">
                    Note: URL id <code>{expectedId}</code> differs from the
                    active room <code>{summary.id}</code>; showing the active
                    room.
                </p>
            )}
        </div>
    );
}

