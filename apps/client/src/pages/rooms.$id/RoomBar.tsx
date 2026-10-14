import { useState } from "react";
import { useNavigate } from "react-router-dom";
import type { ConnectionState, RoomSummaryIpc } from "../../services/room";
import { errorText } from "../../services/errors";
import { leaveRoom } from "../../services/room";
import { LeaveRoomModal } from "../../components/LeaveRoomModal";
import { Button, Icon, StatusPill } from "../../components/ui";
import type { PillTone } from "../../components/ui";

interface RoomBarProps {
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

interface Connection {
    label: string;
    tone: PillTone;
    live: boolean;
}

/** What the signaling session is doing, in words a viewer understands. */
function describeConnection(signaling: ConnectionState | null): Connection {
    switch (signaling?.phase) {
        case "Authenticated":
            return signaling.connected
                ? { label: "Connected", tone: "success", live: false }
                : { label: "Connecting", tone: "warning", live: true };
        case "Connecting":
        case "Handshaking":
            return { label: "Connecting", tone: "warning", live: true };
        case "Reconnecting":
            return { label: "Reconnecting", tone: "warning", live: true };
        case "ShuttingDown":
            return { label: "Closing", tone: "neutral", live: false };
        case "Disconnected":
            return { label: "Disconnected", tone: "danger", live: false };
        default:
            return { label: "Connecting", tone: "warning", live: true };
    }
}

/**
 * The room's header bar: what room this is, who you are in it, how the
 * connection is doing, and the way out.
 */
export function RoomBar({
    summary,
    signaling,
    onLeft,
    isHost,
    lastKnownHostPositionMs,
}: RoomBarProps): JSX.Element {
    const navigate = useNavigate();
    const [leaving, setLeaving] = useState(false);
    const [error, setError] = useState<string | null>(null);
    const [showModal, setShowModal] = useState(false);
    const [copied, setCopied] = useState(false);

    // Leave the room. A failure rejects, so the leave dialog (which called this)
    // shows it: the bar's own error text sits behind the dialog's backdrop.
    async function onLeave(): Promise<void> {
        if (leaving) return;
        setLeaving(true);
        setError(null);
        try {
            await leaveRoom();
        } catch (err) {
            setLeaving(false);
            setError(errorText(err));
            throw err;
        }
        onLeft();
        navigate("/rooms");
    }

    async function copyCode(): Promise<void> {
        try {
            await navigator.clipboard.writeText(summary.code);
            setCopied(true);
            window.setTimeout(() => setCopied(false), 2000);
        } catch {
            setCopied(false);
        }
    }

    const connection = describeConnection(signaling);
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
            <header className="room-bar">
                <div className="room-bar__main">
                    <h1 className="room-bar__title" title={summary.title}>
                        {summary.title}
                    </h1>
                    <div className="room-bar__meta">
                        <span className="room-bar__code">
                            <code>{summary.code}</code>
                            <Button
                                variant="ghost"
                                size="sm"
                                iconOnly
                                icon={copied ? "check" : "copy"}
                                aria-label={copied ? "Room code copied" : "Copy room code"}
                                title={copied ? "Copied" : "Copy room code"}
                                onClick={() => void copyCode()}
                            />
                        </span>
                        <StatusPill tone={isHost ? "accent" : "neutral"}>{isHost ? "You are the host" : "Viewer"}</StatusPill>
                        <StatusPill
                            tone={connection.tone}
                            dot
                            pulse={connection.live}
                            title={`Connection: ${signaling?.phase ?? "Disconnected"}`}
                        >
                            {connection.label}
                        </StatusPill>
                        {isOffline && (
                            <StatusPill tone="danger" testId="host-offline-badge">
                                Host offline
                            </StatusPill>
                        )}
                        {isOffline && lastKnownHostPositionMs !== null && (
                            <span className="room-bar__last-position" data-testid="last-known-position">
                                <Icon name="clock" />
                                Last known host position: {formatPosition(lastKnownHostPositionMs)}
                            </span>
                        )}
                    </div>
                </div>
                <Button
                    className="room-bar__leave"
                    variant="danger"
                    icon="leave"
                    onClick={() => setShowModal(true)}
                    disabled={leaving}
                >
                    {leaving ? "Leaving..." : "Leave"}
                </Button>
                {error !== null && (
                    <p className="room-bar__error" role="alert">
                        {error}
                    </p>
                )}
            </header>
        </>
    );
}
