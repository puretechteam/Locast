import { useState } from "react";
import { useNavigate } from "react-router-dom";
import type { ConnectionState, RoomSummaryIpc } from "../../services/room";
import { leaveRoom } from "../../services/room";
import { LeaveRoomModal } from "../../components/LeaveRoomModal";

interface RoomFooterProps {
    summary: RoomSummaryIpc;
    signaling: ConnectionState | null;
    onLeft: () => void;
    isHost: boolean;
    lastKnownHostPositionMs: number | null;
}

function formatPosition(ms: number): string {
    const totalSec = Math.max(0, Math.floor(ms / 1000));
    const h = Math.floor(totalSec / 3600);
    const m = Math.floor((totalSec % 3600) / 60);
    const s = totalSec % 60;
    if (h > 0) {
        return `${h}:${m.toString().padStart(2, "0")}:${s.toString().padStart(2, "0")}`;
    }
    return `${m}:${s.toString().padStart(2, "0")}`;
}

export function RoomFooter({
    summary,
    signaling,
    onLeft,
    isHost,
    lastKnownHostPositionMs,
}: RoomFooterProps): JSX.Element {
    const navigate = useNavigate();
    const [leaving, setLeaving] = useState(false);
    const [error, setError] = useState<string | null>(null);
    const [showModal, setShowModal] = useState(false);

    async function onLeave(): Promise<void> {
        if (leaving) return;
        setLeaving(true);
        setError(null);
        try {
            await leaveRoom();
            onLeft();
            navigate("/rooms");
        } catch (err) {
            setError(err instanceof Error ? err.message : String(err));
            setLeaving(false);
        }
    }

    const phase = signaling?.phase ?? "Disconnected";
    const isConnected = signaling?.connected ?? false;
    const isOffline = !isHost && !isConnected;

    return (
        <>
            {showModal && (
                <LeaveRoomModal
                    roomId={summary.id}
                    onClose={() => setShowModal(false)}
                    onConfirm={onLeave}
                />
            )}
            <footer className="room-footer">
                <div className="room-footer__meta">
                    <span className="room-footer__code">{summary.code}</span>
                    <span className="room-footer__title">{summary.title}</span>
                    <span className="room-footer__phase">signaling: {phase}</span>
                    {isOffline && (
                        <span className="room-footer__host-offline" data-testid="host-offline-badge">
                            Host offline
                        </span>
                    )}
                    {isOffline && lastKnownHostPositionMs !== null && (
                        <span className="room-footer__last-position" data-testid="last-known-position">
                            Last known host position: {formatPosition(lastKnownHostPositionMs)}
                        </span>
                    )}
                </div>
                {error !== null && <p className="room-footer__error">{error}</p>}
                <button
                    className="room-footer__leave"
                    type="button"
                    onClick={() => setShowModal(true)}
                    disabled={leaving}
                >
                    {leaving ? "Leaving..." : "Leave"}
                </button>
            </footer>
        </>
    );
}
