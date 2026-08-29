import { useState } from "react";
import { useNavigate } from "react-router-dom";
import { connectSignaling, joinRoom } from "../../services/room";

const CODE_ALPHABET = "ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

function isValidCodeChar(ch: string): boolean {
    return CODE_ALPHABET.indexOf(ch) !== -1;
}

export function JoinRoomPage(): JSX.Element {
    const navigate = useNavigate();
    const [code, setCode] = useState("");
    const [displayName, setDisplayName] = useState("");
    const [submitting, setSubmitting] = useState(false);
    const [error, setError] = useState<string | null>(null);

    const codeUpper = code.toUpperCase();
    const codeValid = codeUpper.length === 6 && [...codeUpper].every(isValidCodeChar);
    const nameValid = displayName.trim().length > 0 && displayName.trim().length <= 32;

    function onCodeChange(e: React.ChangeEvent<HTMLInputElement>): void {
        const filtered = e.target.value
            .toUpperCase()
            .split("")
            .filter(isValidCodeChar)
            .join("")
            .slice(0, 6);
        setCode(filtered);
    }

    async function onSubmit(e: React.FormEvent): Promise<void> {
        e.preventDefault();
        if (!codeValid || !nameValid || submitting) return;
        setSubmitting(true);
        setError(null);
        try {
            await connectSignaling();
            const summary = await joinRoom(codeUpper, displayName.trim());
            navigate(`/rooms/${summary.id}`);
        } catch (err) {
            setError(err instanceof Error ? err.message : String(err));
            setSubmitting(false);
        }
    }

    return (
        <form className="form" onSubmit={onSubmit}>
            <label className="form__label">
                <span>Room code</span>
                <input
                    className="form__input form__input--code"
                    type="text"
                    value={codeUpper}
                    onChange={onCodeChange}
                    placeholder="ABCDEF"
                    maxLength={6}
                    autoCapitalize="characters"
                    autoComplete="off"
                    spellCheck={false}
                    disabled={submitting}
                    required
                />
            </label>
            <label className="form__label">
                <span>Display name</span>
                <input
                    className="form__input"
                    type="text"
                    value={displayName}
                    onChange={(e) => setDisplayName(e.target.value)}
                    placeholder="Your name"
                    maxLength={32}
                    disabled={submitting}
                    required
                />
            </label>
            {error !== null && <p className="form__error">{error}</p>}
            <button
                className="form__submit"
                type="submit"
                disabled={!codeValid || !nameValid || submitting}
            >
                {submitting ? "Joining..." : "Join"}
            </button>
        </form>
    );
}
