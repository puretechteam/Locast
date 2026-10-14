import type { ReactNode } from "react";
import type { ParticipantIpc, RoomSummaryIpc } from "../../services/room";
import { ParticipantTile } from "./ParticipantTile";
import { useConnectionQuality } from "../../hooks/useConnectionQuality";
import { useConnectionQualityStore } from "../../stores/useConnectionQualityStore";

interface ParticipantStripProps {
    summary: RoomSummaryIpc;
    /** Shown in the panel header, e.g. the host's Permissions button. */
    action?: ReactNode;
}

export function ParticipantStrip({ summary, action }: ParticipantStripProps): JSX.Element {
    useConnectionQuality();
    const quality = useConnectionQualityStore((s) => s.quality);
    const participants: ParticipantIpc[] = summary.participants;
    return (
        <section className="participant-strip" aria-label="Participants">
            <div className="participant-strip__head">
                <h2 className="section-title">People &middot; {participants.length}</h2>
                {action}
            </div>
            <ul className="participant-strip__list">
                {participants.map((p) => (
                    <ParticipantTile key={p.user_id} participant={p} quality={quality} />
                ))}
            </ul>
        </section>
    );
}
