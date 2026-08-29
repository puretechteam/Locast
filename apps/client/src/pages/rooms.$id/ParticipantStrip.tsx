import type { ParticipantIpc, RoomSummaryIpc } from "../../services/room";
import { ParticipantTile } from "./ParticipantTile";

interface ParticipantStripProps {
    summary: RoomSummaryIpc;
}

export function ParticipantStrip({ summary }: ParticipantStripProps): JSX.Element {
    const participants: ParticipantIpc[] = summary.participants;
    return (
        <section className="participant-strip" aria-label="Participants">
            {summary.host_disconnected && (
                <div className="participant-strip__banner">
                    Host reconnecting...
                    {summary.host_disconnect_deadline_ms !== null && (
                        <span className="participant-strip__deadline">
                            {" "}(deadline {new Date(summary.host_disconnect_deadline_ms).toLocaleTimeString()})
                        </span>
                    )}
                </div>
            )}
            <ul className="participant-strip__list">
                {participants.map((p) => (
                    <ParticipantTile key={p.user_id} participant={p} />
                ))}
            </ul>
        </section>
    );
}
