import type { ParticipantIpc, ParticipantStatusIpc } from "../../services/room";

interface ParticipantTileProps {
    participant: ParticipantIpc;
}

const STATUS_COLOR: Record<ParticipantStatusIpc, string> = {
    Joining: "#f5a3a3",
    Connected: "#7bd88f",
    Reconnecting: "#f5c869",
    Disconnected: "#9aa3ad",
    Left: "#5b6168",
};

export function ParticipantTile({ participant }: ParticipantTileProps): JSX.Element {
    const initial = participant.display_name.trim().charAt(0).toUpperCase() || "?";
    return (
        <li className="participant-tile" title={participant.status}>
            <div className="participant-tile__avatar">{initial}</div>
            <div className="participant-tile__body">
                <div className="participant-tile__name">
                    {participant.display_name}
                    {participant.is_host && (
                        <span className="participant-tile__badge">Host</span>
                    )}
                </div>
            </div>
            <span
                className="participant-tile__dot"
                style={{ background: STATUS_COLOR[participant.status] }}
            />
        </li>
    );
}
