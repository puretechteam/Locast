import { useEffect, useState } from "react";
import { Link, useParams } from "react-router-dom";
import { events } from "../../services/ipc";
import { getRoomState } from "../../services/room";
import { getSignalingState } from "../../services/signaling";
import type { ConnectionState, RoomSummaryIpc } from "../../services/room";
import { useRoomStore } from "../../stores/useRoomStore";
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
            <section className="room-page__player" aria-label="Player">
                <p className="room-page__player-empty">No media loaded yet.</p>
            </section>
            <ParticipantStrip summary={summary} />
            <RoomFooter
                summary={summary}
                signaling={signaling}
                onLeft={clear}
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
