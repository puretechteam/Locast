import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useParams } from "react-router-dom";
import { Button, EmptyState, LinkButton, Spinner, StatusPill } from "../../components/ui";
import { events } from "../../services/ipc";
import { getRoomState } from "../../services/room";
import { getSignalingState } from "../../services/signaling";
import type { ConnectionState, RoomSummaryIpc } from "../../services/room";
import { useChatStore } from "../../stores/useChatStore";
import type { PositionReportEvent } from "../../bindings";
import { useRoomStore } from "../../stores/useRoomStore";
import { resetRoomScopedStores } from "../../stores/resetRoomScopedStores";
import { usePlaybackStore } from "../../stores/usePlaybackStore";
import { Player } from "../../components/Player";
import { PlaybackControls } from "../../components/PlaybackControls";
import { DriftIndicator } from "../../components/DriftIndicator";
import { SyncButton } from "../../components/SyncButton";
import { ChatPanel } from "../../components/ChatPanel";
import { PermissionsModal } from "../../components/PermissionsModal";
import { useDriftSmoother } from "../../drift/useDriftSmoother";
import { useManualSync } from "../../drift/useManualSync";
import { useClockSkew } from "../../drift/useClockSkew";
import { probeClockSkewOnce } from "../../drift/clockSkewProbe";
import { usePositionReportBridge } from "../../hooks/usePositionReportBridge";
import { useViewerPositionStore } from "../../stores/useViewerPositionStore";
import { useClockSkewStore } from "../../stores/useClockSkewStore";
import { useCapabilityStore } from "../../stores/useCapabilityStore";
import { ParticipantStrip } from "./ParticipantStrip";
import { RoomMediaPanel } from "./RoomMediaPanel";
import { RoomBar } from "./RoomBar";
import { RoomTopBar } from "../../components/RoomTopBar";

export function RoomPage(): JSX.Element {
    const params = useParams<{ id: string }>();
    const summary = useRoomStore((s) => s.summary);
    const signaling = useRoomStore((s) => s.signaling);
    const setSummary = useRoomStore((s) => s.setSummary);
    const setSignaling = useRoomStore((s) => s.setSignaling);
    const setLastKnownHostPositionMs = useRoomStore((s) => s.setLastKnownHostPositionMs);
    const [hydrated, setHydrated] = useState(false);
    const messages = useChatStore((s) => s.messages);
    const [showPermissions, setShowPermissions] = useState(false);
    // P1-T10: media chosen from the library, played locally.
    const localMediaSrc = usePlaybackStore((s) => s.mediaSrc);
    const localMediaTitle = usePlaybackStore((s) => s.mediaTitle);

    // On leave, reset every room-scoped store (summary, playback media and
    // sequence counters, per-viewer positions, capabilities, connection
    // quality, shared media, downloads) so a re-join starts from a clean slate.
    // A room that ENDS is handled by `RoomEndBridge`; leaving on purpose emits
    // no event, so this runs from the leave paths.
    const handleLeft = useCallback(() => {
        resetRoomScopedStores();
    }, []);

    // P4-T02 test seam: in Vite's test mode, expose
    // `useRoomStore.setSummary` on `window.__locastRoomStore`
    // so the Playwright harness can mount the room
    // without going through the Tauri invoke surface.
    // P4-T05: also expose `setLocalUserId` /
    // `getLocalUserId` so the harness can tell the
    // page "I am participant X" -- the room
    // summary alone does not disambiguate host
    // from viewer in a multi-participant room.
    useEffect(() => {
        if (import.meta.env.MODE !== "test") return;
        let localUserIdValue: string | null = null;
        const w = window as unknown as {
            __locastRoomStore?: {
                setSummary: (s: unknown) => void;
                setLocalUserId: (id: string | null) => void;
                getLocalUserId: () => string | null;
            };
        };
        w.__locastRoomStore = {
            setSummary: (s) => setSummary(s as Parameters<typeof setSummary>[0]),
            setLocalUserId: (id) => {
                localUserIdValue = id;
            },
            getLocalUserId: () => localUserIdValue,
        };
        return () => {
            if (w.__locastRoomStore) delete w.__locastRoomStore;
        };
    }, [setSummary]);

    // The playback://state bridge is mounted at the app level
    // (`PlaybackBridge` in main.tsx), not here: this page is
    // unmounted while a download runs and would miss events.

    // P4-T03: bridge position://report events into the
    // per-viewer position store. The hook returns
    // `null` (no JSX).
    usePositionReportBridge();

    // P4-T02: derive `isHost` and `localUserId` from
    // the cached room summary.
    //
    // P4-T05 fix: `isHost` means "the LOCAL user is the
    // current host", not "the room has a host".
    //
    // P5-T04: `localUserId` is the local user's canonical
    // server-assigned id in production too: the Rust room
    // client fills `summary.you_user_id` from ROOM_CREATED /
    // ROOM_JOINED `you.user_id`; before that is known the
    // signaling session's AUTH_OK `user_id` is the fallback.
    // (`identityGet` is NOT used: its `user_id` is a pubkey
    // hash, not the server id.) It is set for viewers as well
    // as hosts. In Vite test mode the Playwright harness may
    // still override the id through
    // `__locastRoomStore.setLocalUserId` (see
    // `tests/playwright/fixtures/vite-app.ts`); production
    // never reads that seam.
    //
    // `isHost` is `host_user_id === localUserId`, so it follows
    // HOST_MIGRATED / HOST_RECONNECTED summaries. It gates
    // host-only UI (playback controls, permissions, viewer
    // positions, the authoritative manual-sync branch); the
    // server still enforces every host-only action. Capability
    // checks (laser, drawing) use `you_cap_set`.
    const signalingUserId = signaling?.user_id ?? null;
    const { isHost, localUserId, hostPositionMs } = useMemo(() => {
        let overrideId: string | null = null;
        if (import.meta.env.MODE === "test") {
            const w = window as unknown as {
                __locastRoomStore?: {
                    getLocalUserId?: () => string | null;
                };
            };
            overrideId = w.__locastRoomStore?.getLocalUserId?.() ?? null;
        }
        const userId =
            summary == null
                ? null
                : overrideId ?? summary.you_user_id ?? signalingUserId ?? null;
        const localUserIsHost =
            summary != null && userId !== null && summary.host_user_id === userId;
        const pos = usePlaybackStore.getState().lastApplied?.media_position_ms ?? 0;
        return {
            isHost: localUserIsHost,
            localUserId: userId,
            hostPositionMs: pos,
        };
    }, [summary, signalingUserId]);
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

    // Forget a viewer's position when they leave. `removeViewer` existed for
    // this but nothing called it, so a participant who left stayed in the
    // host's list with an age counter that grew forever, and still counted
    // toward the room median. Only someone seen LEAVING the participant list
    // is removed, not everyone absent from it, so a report that arrives before
    // its sender's first summary is kept.
    const previousParticipantIdsRef = useRef<Set<string>>(new Set());
    useEffect(() => {
        const current = new Set((summary?.participants ?? []).map((p) => p.user_id));
        for (const id of previousParticipantIdsRef.current) {
            if (!current.has(id)) {
                useViewerPositionStore.getState().removeViewer(id);
            }
        }
        previousParticipantIdsRef.current = current;
    }, [summary]);

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
    const getVideo = useCallback(() => videoRef.current, []);
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

    // P4-T05: the manual sync hook. The hook owns the
    // target-position computation and the DOM seek;
    // the UI surfaces (DriftIndicator Resync, standalone
    // SyncButton) both call into the same hook so there
    // is one sync implementation. The branch is chosen
    // by the hook based on `isHost` -- viewers get a
    // local-only DOM seek; hosts get a local seek + a
    // PLAYBACK_CMD emit through the existing
    // `sendPlaybackCommand` path.
    // P4-T06: thread the measured skew into the manual-sync
    // hook. Until the first NTP measurement is available,
    // `skewMs` is null; the hook falls back to 0 in that
    // case so P4-T05's existing behavior is preserved.
    const measuredSkewMs = useClockSkewStore((s) => s.skewMs);
    const sync = useManualSync({
        roomId: summary?.id ?? null,
        isHost,
        getVideo: () => videoRef.current,
        skewMs: measuredSkewMs ?? 0,
        localUserId,
    });

    // P4-T06: mount the 60s NTP measurement driver. The probe is the Tauri
    // `clock_skew_probe` command (a stable module-level function: the hook
    // restarts its cadence whenever `probeOnce` changes). Until the first
    // burst completes `skewMs` stays null and the consumers use 0. The test
    // seam (`__locastClockSkew.setSkewJitter`) still drives the store
    // directly in Playwright.
    useClockSkew({
        roomId: summary?.id ?? null,
        probeOnce: probeClockSkewOnce,
    });

    // P4-T05: real Resync handler. The DriftIndicator's
    // Resync button (P4-T04 stub) now calls into the
    // same shared `sync` hook as the standalone
    // SyncButton. Selecting the branch is the hook's
    // job; the UI surface just calls whichever entry
    // point matches the user's role expectation
    // (`authoritativeSeek` is also a no-op for
    // non-hosts, so it is safe to wire unconditionally).
    const onResync = useCallback(() => {
        if (isHost) {
            void sync.authoritativeSeek();
        } else {
            void sync.localSeek();
        }
    }, [isHost, sync]);

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

                // Chat messages are collected by the app-level `ChatBridge`
                // into `useChatStore`, so they are not lost while this page
                // is unmounted.

                // P7-T06: listen for position://report events to track
                // the host's last known position. The host is the
                // participant with is_host === true in the summary.
                const u5 = await events.positionReport((next: PositionReportEvent) => {
                    if (cancelled) return;
                    // Read the live summary: this listener is
                    // registered once, when `summary` was still null.
                    const current = useRoomStore.getState().summary;
                    if (current === null) return;
                    const host = current.participants.find((p) => p.is_host);
                    if (host && next.sender_id === host.user_id) {
                        setLastKnownHostPositionMs(next.media_position_ms);
                    }
                });
                if (cancelled) {
                    u5();
                    return;
                }
                unlistens.push(u5);
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
    // eslint-disable-next-line react-hooks/exhaustive-deps -- `setLastKnownHostPositionMs` is a stable zustand action selected from the store.
    }, [setSummary, setSignaling]);

    // P6-T02: sync you_cap_set from the room summary to the
    // capability store so DrawingLayer can gate toolbar visibility.
    useEffect(() => {
        if (summary?.you_cap_set !== undefined) {
            useCapabilityStore.getState().setYouCapSet(summary.you_cap_set);
        }
    }, [summary]);

    if (!hydrated) {
        return (
            <div className="room-page room-page--loading">
                <h1 className="sr-only">Room</h1>
                <p className="loading" role="status" aria-busy="true">
                    <Spinner />
                    Loading room...
                </p>
            </div>
        );
    }

    if (summary === null && localMediaSrc !== null) {
        // Local playback (P1-T10): a library item opened without a room. The
        // same Player as in a room, minus everything that needs the network.
        return (
            <div className="room-page room-page--local" data-testid="room-local">
                <RoomTopBar summary={null} />
                <div className="room-local">
                    <div className="room-local__bar">
                        <LinkButton to="/library" variant="ghost" size="sm" icon="arrow-left">
                            Back to library
                        </LinkButton>
                        <h1 className="sr-only">Local playback</h1>
                        <p className="room-page__local-note">
                            Playing locally: <strong>{localMediaTitle ?? "media"}</strong>
                        </p>
                        <StatusPill>Only on this computer</StatusPill>
                    </div>
                    <Player videoRef={videoRef} />
                </div>
            </div>
        );
    }

    if (summary === null) {
        return (
            <div className="room-page room-page--empty">
                <RoomTopBar summary={null} />
                <div className="page page--narrow" data-testid="room-empty">
                    <EmptyState
                        icon="users"
                        title="You are not in a room"
                        headingLevel={1}
                        actions={
                            <>
                                <LinkButton to="/rooms/join" icon="join">
                                    Join a room
                                </LinkButton>
                                <LinkButton to="/rooms/new" variant="primary" icon="plus">
                                    Create a room
                                </LinkButton>
                            </>
                        }
                    >
                        This room is not active. Create a new one, or join with a code.
                        {params.id !== undefined && (
                            <span className="room-page__hint">
                                {" "}
                                (URL id: <code>{params.id}</code>)
                            </span>
                        )}
                    </EmptyState>
                </div>
            </div>
        );
    }

    const expectedId = params.id;
    const idMismatch =
        expectedId !== undefined && expectedId.length > 0 && expectedId !== summary.id;

    const otherViewers = Object.values(viewerPositions).filter((v) => v.userId !== localUserId);

    return (
        <div className="room-page">
            <RoomTopBar summary={summary} />
            <RoomBar
                summary={summary}
                signaling={signaling}
                onLeft={handleLeft}
                isHost={isHost}
                lastKnownHostPositionMs={useRoomStore.getState().lastKnownHostPositionMs}
            />
            <div className="room-layout">
                <div className="room-main">
                    <Player localUserId={localUserId} isHost={isHost} videoRef={videoRef} />
                    <div className="room-controls">
                        <PlaybackControls
                            isHost={isHost}
                            positionMs={displayPositionMs}
                            localUserId={localUserId}
                            getVideo={getVideo}
                        />
                        {/* P4-T05: the Sync button is always visible so the
                         * user can converge on the host at any time; the
                         * drift indicator's Resync below is the same action
                         * (one shared `useManualSync` hook). */}
                        <div className="room-controls__sync">
                            <SyncButton sync={sync} />
                        </div>
                    </div>
                    {/* P4-T04: hidden until the smoothed offset exceeds 2.0 s.
                     * Non-blocking; playback is never auto-corrected. */}
                    <DriftIndicator sample={drift} onResync={onResync} />
                    <RoomMediaPanel />
                    {isHost && (
                        <section
                            className="room-page__viewer-positions panel"
                            data-testid="viewer-positions"
                            aria-label="Viewer positions"
                        >
                            <div className="panel__header">
                                <h2 className="panel__title">Viewer positions</h2>
                                {drift.roomMedianMs !== null && (
                                    <span
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
                                    </span>
                                )}
                            </div>
                            <ul className="room-page__viewer-positions-list">
                                {otherViewers.map((v) => {
                                    const ageSec = Math.max(
                                        0,
                                        Math.round((Date.now() - v.receivedAtMs) / 1000),
                                    );
                                    const posSec = (v.mediaPositionMs / 1000).toFixed(1);
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
                                {otherViewers.length === 0 && (
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
                    {idMismatch && (
                        <p className="room-page__hint">
                            Note: URL id <code>{expectedId}</code> differs from the
                            active room <code>{summary.id}</code>; showing the active
                            room.
                        </p>
                    )}
                </div>
                <aside className="room-side" aria-label="Room sidebar">
                    <ParticipantStrip
                        summary={summary}
                        action={
                            isHost ? (
                                <Button
                                    size="sm"
                                    icon="shield"
                                    className="room-page__permissions-btn"
                                    onClick={() => setShowPermissions(true)}
                                >
                                    Permissions
                                </Button>
                            ) : undefined
                        }
                    />
                    <ChatPanel messages={messages} />
                </aside>
            </div>
            {showPermissions && (
                <PermissionsModal onClose={() => setShowPermissions(false)} />
            )}
        </div>
    );
}
