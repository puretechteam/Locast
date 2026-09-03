import { useCallback, useEffect, useMemo, useState } from "react";
import { Link, useParams } from "react-router-dom";
import { events } from "../../services/ipc";
import { getRoomState } from "../../services/room";
import { getSignalingState } from "../../services/signaling";
import type { ConnectionState, RoomSummaryIpc } from "../../services/room";
import { useRoomStore } from "../../stores/useRoomStore";
import { usePlaybackStore } from "../../stores/usePlaybackStore";
import { Player } from "../../components/Player";
import { PlaybackControls } from "../../components/PlaybackControls";
import { usePlaybackEventBridge } from "../../hooks/usePlaybackEventBridge";
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
            <Player localUserId={localUserId} isHost={isHost} />
            <ParticipantStrip summary={summary} />
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

