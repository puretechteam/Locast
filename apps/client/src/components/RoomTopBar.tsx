import { useEffect, useRef, useState } from "react";
import type { RoomSummaryIpc } from "../services/room";
import { useHostGracePeriod } from "../hooks/useHostGracePeriod";
import { events } from "../services/ipc";

interface RoomTopBarProps {
    summary: RoomSummaryIpc | null;
}

function formatCountdown(ms: number): string {
    const totalSeconds = Math.max(0, Math.ceil(ms / 1000));
    const minutes = Math.floor(totalSeconds / 60);
    const seconds = totalSeconds % 60;
    if (minutes > 0) {
        return `${minutes}m ${seconds}s`;
    }
    return `${seconds}s`;
}

export function RoomTopBar({ summary }: RoomTopBarProps): JSX.Element | null {
    const { remainingMs, isExpired } = useHostGracePeriod(
        summary?.host_disconnect_deadline_ms ?? null,
    );
    const [roomEnded, setRoomEnded] = useState(false);
    // True while the user is in a room, whether the summary arrived through
    // this bar's own `room://state` listener or through the `summary` prop
    // (hydration on mount, or another listener updating the store).
    const inRoomRef = useRef(false);
    const hasRoom = summary !== null;

    useEffect(() => {
        if (hasRoom) {
            inRoomRef.current = true;
            setRoomEnded(false);
        }
    }, [hasRoom]);

    useEffect(() => {
        let cancelled = false;
        let unlisten: (() => void) | undefined;

        events
            .roomState((next: RoomSummaryIpc | null) => {
                if (cancelled) return;
                if (next !== null) {
                    inRoomRef.current = true;
                } else if (inRoomRef.current) {
                    inRoomRef.current = false;
                    setRoomEnded(true);
                }
            })
            .then((u) => {
                if (cancelled) {
                    u();
                } else {
                    unlisten = u;
                }
            })
            .catch((err: unknown) => {
                const detail = err instanceof Error ? err.message : String(err);
                console.error("RoomTopBar: failed to subscribe", detail);
            });

        return () => {
            cancelled = true;
            unlisten?.();
        };
    }, []);

    return (
        <div className="room-top-bar" aria-label="Room status">
            {summary?.host_disconnected && !isExpired && remainingMs !== null && (
                <div className="room-top-bar__grace-banner" role="status" aria-live="polite">
                    Host reconnecting… ({formatCountdown(remainingMs)} remaining)
                </div>
            )}
            {summary?.host_disconnected && isExpired && (
                <div className="room-top-bar__grace-banner room-top-bar__grace-banner--expired" role="status" aria-live="polite">
                    Host reconnecting… (time expired)
                </div>
            )}
            {roomEnded && (
                <div className="room-top-bar__toast" role="alert" aria-live="assertive">
                    Room ended
                </div>
            )}
        </div>
    );
}
