import { useState } from "react";
import { useNavigate } from "react-router-dom";
import { errorText } from "../../services/errors";
import { connectSignaling, createRoom } from "../../services/room";
import { resetRoomScopedStores } from "../../stores/resetRoomScopedStores";
import { useRoomStore } from "../../stores/useRoomStore";
import { Button, Field, LinkButton, Notice, PageHeader, Spinner } from "../../components/ui";
import { FormError } from "../../components/ui/FormError";

export function NewRoomPage(): JSX.Element {
    const navigate = useNavigate();
    const currentRoom = useRoomStore((s) => s.summary);
    const [title, setTitle] = useState("");
    const [migrationEnabled, setMigrationEnabled] = useState(true);
    const [submitting, setSubmitting] = useState(false);
    const [error, setError] = useState<string | null>(null);

    const titleValid = title.trim().length > 0 && title.trim().length <= 80;

    async function onSubmit(e: React.FormEvent): Promise<void> {
        e.preventDefault();
        if (!titleValid || submitting) return;
        setSubmitting(true);
        setError(null);
        try {
            // Not in a room: drop leftovers (e.g. a file played locally from
            // the library) so they do not open inside the new room.
            if (useRoomStore.getState().summary === null) resetRoomScopedStores();
            await connectSignaling();
            const summary = await createRoom(title.trim(), migrationEnabled);
            navigate(`/rooms/${summary.id}`);
        } catch (err) {
            setError(errorText(err));
            setSubmitting(false);
        }
    }

    return (
        <div className="page page--narrow">
            <PageHeader
                title="Create a room"
                description="You host the room, pick what to watch and share an invite link with your friends."
                back={{ to: "/rooms", label: "Back to rooms" }}
            />

            {currentRoom !== null && (
                <Notice kind="warning">
                    You are in &ldquo;{currentRoom.title}&rdquo;. Creating a new room leaves it.
                </Notice>
            )}

            <form className="panel form-panel form-stack" onSubmit={onSubmit}>
                <Field label="Room title" hint="Your friends see this when they join. Up to 80 characters.">
                    {({ id, describedBy }) => (
                        <input
                            id={id}
                            className="input"
                            type="text"
                            value={title}
                            onChange={(e) => setTitle(e.target.value)}
                            placeholder="Movie night"
                            maxLength={80}
                            disabled={submitting}
                            required
                            autoFocus
                            {...(describedBy !== undefined ? { "aria-describedby": describedBy } : {})}
                        />
                    )}
                </Field>
                <label className="check">
                    <input
                        type="checkbox"
                        checked={migrationEnabled}
                        onChange={(e) => setMigrationEnabled(e.target.checked)}
                        disabled={submitting}
                    />
                    <span className="check__body">
                        <span className="check__label">Enable host migration</span>
                        <span className="field__hint">
                            If you lose your connection, another person in the room can take over as host.
                        </span>
                    </span>
                </label>
                {error !== null && <FormError>{error}</FormError>}
                <div className="form-actions">
                    <LinkButton to="/rooms" variant="ghost">
                        Cancel
                    </LinkButton>
                    <Button type="submit" variant="primary" disabled={!titleValid || submitting}>
                        {submitting && <Spinner />}
                        {submitting ? "Creating..." : "Create room"}
                    </Button>
                </div>
            </form>
        </div>
    );
}
