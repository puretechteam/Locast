import React from "react";
import type { ParticipantIpc, ParticipantStatusIpc } from "../../services/room";
import type { ConnectionQuality } from "../../stores/useConnectionQualityStore";

interface ParticipantTileProps {
    participant: ParticipantIpc;
    quality?: ConnectionQuality;
}

const STATUS_COLOR: Record<ParticipantStatusIpc, string> = {
    Joining: "#f5a3a3",
    Connected: "#7bd88f",
    Reconnecting: "#f5c869",
    Disconnected: "#9aa3ad",
    Left: "#5b6168",
};

export const ParticipantTile = React.memo(
    function ParticipantTile({ participant, quality }: ParticipantTileProps): JSX.Element {
        const initial = participant.display_name.trim().charAt(0).toUpperCase() || "?";
        const qualityLevel = quality ?? "good";
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
                {qualityLevel !== "good" && (
                    <span
                        className={`participant-tile__quality participant-tile__quality--${qualityLevel}`}
                        aria-label={`Connection quality: ${qualityLevel}`}
                    >
                        <span className="participant-tile__quality-bar" />
                        <span className="participant-tile__quality-bar" />
                        <span className="participant-tile__quality-bar" />
                    </span>
                )}
                <span
                    className="participant-tile__dot"
                    style={{ background: STATUS_COLOR[participant.status] }}
                    aria-label={`Status: ${participant.status}`}
                />
            </li>
        );
    },
    (prevProps, nextProps) => prevProps.participant === nextProps.participant
);
