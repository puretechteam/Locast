import type { ParticipantIpc, RoomSummaryIpc } from "../../services/room";
import { ParticipantTile } from "./ParticipantTile";
import { useConnectionQuality } from "../../hooks/useConnectionQuality";
import { useConnectionQualityStore } from "../../stores/useConnectionQualityStore";

interface ParticipantStripProps {
    summary: RoomSummaryIpc;
}

export function ParticipantStrip({ summary }: ParticipantStripProps): JSX.Element {
    useConnectionQuality();
    const quality = useConnectionQualityStore((s) => s.quality);
    const participants: ParticipantIpc[] = summary.participants;
    return (
        <section className="participant-strip" aria-label="Participants">
            <ul className="participant-strip__list">
                {participants.map((p) => (
                    <ParticipantTile key={p.user_id} participant={p} quality={quality} />
                ))}
            </ul>
        </section>
    );
}
