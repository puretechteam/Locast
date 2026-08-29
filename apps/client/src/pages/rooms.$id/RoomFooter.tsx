import { useState } from "react";
import { useNavigate } from "react-router-dom";
import type { ConnectionState, RoomSummaryIpc } from "../../services/room";
import { leaveRoom } from "../../services/room";

interface RoomFooterProps {
    summary: RoomSummaryIpc;
    signaling: ConnectionState | null;
    onLeft: () => void;
}

export function RoomFooter({ summary, signaling, onLeft }: RoomFooterProps): JSX.Element {
    const navigate = useNavigate();
    const [leaving, setLeaving] = useState(false);
    const [error, setError] = useState<string | null>(null);

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

    return (
        <footer className="room-footer">
            <div className="room-footer__meta">
                <span className="room-footer__code">{summary.code}</span>
                <span className="room-footer__title">{summary.title}</span>
                <span className="room-footer__phase">signaling: {phase}</span>
            </div>
            {error !== null && <p className="room-footer__error">{error}</p>}
            <button
                className="room-footer__leave"
                type="button"
                onClick={onLeave}
                disabled={leaving}
            >
                {leaving ? "Leaving..." : "Leave"}
            </button>
        </footer>
    );
}
