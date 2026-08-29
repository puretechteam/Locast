import { useState } from "react";
import { useNavigate } from "react-router-dom";
import { connectSignaling, createRoom } from "../../services/room";

export function NewRoomPage(): JSX.Element {
    const navigate = useNavigate();
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
            await connectSignaling();
            const summary = await createRoom(title.trim(), migrationEnabled);
            navigate(`/rooms/${summary.id}`);
        } catch (err) {
            setError(err instanceof Error ? err.message : String(err));
            setSubmitting(false);
        }
    }

    return (
        <form className="form" onSubmit={onSubmit}>
            <label className="form__label">
                <span>Title</span>
                <input
                    className="form__input"
                    type="text"
                    value={title}
                    onChange={(e) => setTitle(e.target.value)}
                    placeholder="Movie night"
                    maxLength={80}
                    disabled={submitting}
                    required
                />
            </label>
            <label className="form__label form__label--checkbox">
                <input
                    type="checkbox"
                    checked={migrationEnabled}
                    onChange={(e) => setMigrationEnabled(e.target.checked)}
                    disabled={submitting}
                />
                <span>Enable host migration</span>
            </label>
            {error !== null && <p className="form__error">{error}</p>}
            <button
                className="form__submit"
                type="submit"
                disabled={!titleValid || submitting}
            >
                {submitting ? "Creating..." : "Create"}
            </button>
        </form>
    );
}
