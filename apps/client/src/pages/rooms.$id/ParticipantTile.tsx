import React from "react";
import type { ParticipantIpc, ParticipantStatusIpc } from "../../services/room";
import type { ConnectionQuality } from "../../stores/useConnectionQualityStore";

interface ParticipantTileProps {
    participant: ParticipantIpc;
    quality?: ConnectionQuality;
}

/** Statuses that need words next to the dot, so color is never the only signal. */
const STATUS_TEXT: Partial<Record<ParticipantStatusIpc, string>> = {
    Joining: "Joining",
    Reconnecting: "Reconnecting",
    Disconnected: "Disconnected",
    Left: "Left",
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
                        <span className="participant-tile__text" title={participant.display_name}>
                            {participant.display_name.trim() || "Unnamed"}
                        </span>
                        {participant.is_host && (
                            <span className="participant-tile__badge">Host</span>
                        )}
                    </div>
                    {STATUS_TEXT[participant.status] !== undefined && (
                        <div className="participant-tile__status">{STATUS_TEXT[participant.status]}</div>
                    )}
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
                    className={`participant-tile__dot participant-tile__dot--${participant.status.toLowerCase()}`}
                    role="img"
                    aria-label={`Status: ${participant.status}`}
                />
            </li>
        );
    },
    (prevProps, nextProps) =>
        prevProps.participant === nextProps.participant &&
        prevProps.quality === nextProps.quality
);
