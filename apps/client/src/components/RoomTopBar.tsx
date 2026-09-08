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
    const prevSummaryRef = useRef<RoomSummaryIpc | null>(null);

    useEffect(() => {
        let cancelled = false;

        async function subscribe(): Promise<void> {
            const unlisten = await events.roomState((next: RoomSummaryIpc | null) => {
                if (cancelled) return;
                if (next === null && prevSummaryRef.current !== null) {
                    setRoomEnded(true);
                }
                prevSummaryRef.current = next;
            });

            if (cancelled) {
                unlisten();
                return;
            }
        }

        void subscribe();
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
