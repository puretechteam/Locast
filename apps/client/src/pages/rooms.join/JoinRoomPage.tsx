import { useState } from "react";
import { useNavigate, useSearchParams } from "react-router-dom";
import { errorText } from "../../services/errors";
import { connectSignaling, joinRoom } from "../../services/room";
import { resetRoomScopedStores } from "../../stores/resetRoomScopedStores";
import { useRoomStore } from "../../stores/useRoomStore";
import { Button, Field, LinkButton, Notice, PageHeader, Spinner } from "../../components/ui";
import { FormError } from "../../components/ui/FormError";

const CODE_ALPHABET = "ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

function isValidCodeChar(ch: string): boolean {
    return CODE_ALPHABET.indexOf(ch) !== -1;
}

function cleanCode(raw: string): string {
    return raw.toUpperCase().split("").filter(isValidCodeChar).join("").slice(0, 6);
}

export function JoinRoomPage(): JSX.Element {
    const navigate = useNavigate();
    const [params] = useSearchParams();
    const currentRoom = useRoomStore((s) => s.summary);
    const [code, setCode] = useState(() => cleanCode(params.get("code") ?? ""));
    const [invite, setInvite] = useState("");
    const [displayName, setDisplayName] = useState("");
    const [submitting, setSubmitting] = useState(false);
    const [error, setError] = useState<string | null>(null);

    const codeUpper = code.toUpperCase();
    const codeValid = codeUpper.length === 6 && [...codeUpper].every(isValidCodeChar);
    const nameValid = displayName.trim().length > 0 && displayName.trim().length <= 32;

    // The invite link carries the host's key (the trust anchor for
    // shared media). Pasting one also fills the room code; Rust parses
    // and checks the link strictly on join.
    function onInviteChange(e: React.ChangeEvent<HTMLInputElement>): void {
        const value = e.target.value;
        setInvite(value);
        const m = /\/join\/([A-Za-z0-9]{6})(?:[?#]|$)/.exec(value.trim());
        if (m !== null && m[1] !== undefined) setCode(cleanCode(m[1]));
    }

    function onCodeChange(e: React.ChangeEvent<HTMLInputElement>): void {
        setCode(cleanCode(e.target.value));
    }

    async function onSubmit(e: React.FormEvent): Promise<void> {
        e.preventDefault();
        if (!codeValid || !nameValid || submitting) return;
        setSubmitting(true);
        setError(null);
        try {
            // Not in a room: drop leftovers (e.g. a file played locally from
            // the library) so they do not open inside the new room.
            if (useRoomStore.getState().summary === null) resetRoomScopedStores();
            await connectSignaling();
            const inviteUrl = invite.trim();
            const summary = await joinRoom(
                codeUpper,
                displayName.trim(),
                inviteUrl.length > 0 ? inviteUrl : null,
            );
            navigate(`/rooms/${summary.id}`);
        } catch (err) {
            setError(errorText(err));
            setSubmitting(false);
        }
    }

    return (
        <div className="page page--narrow">
            <PageHeader
                title="Join a room"
                description="Paste the invite link your host sent you, or type the six-character room code."
                back={{ to: "/rooms", label: "Back to rooms" }}
            />

            {currentRoom !== null && (
                <Notice kind="warning">
                    You are in &ldquo;{currentRoom.title}&rdquo;. Joining another room leaves it.
                </Notice>
            )}

            <form className="panel form-panel form-stack" onSubmit={onSubmit}>
                <Field
                    label="Invite link"
                    optional
                    hint="Needed to receive the media the host shares. Pasting it fills in the room code."
                >
                    {({ id, describedBy }) => (
                        <input
                            id={id}
                            className="input input--mono"
                            type="text"
                            value={invite}
                            onChange={onInviteChange}
                            placeholder="locast://join/ABCDEF?h=...&v=1"
                            autoComplete="off"
                            spellCheck={false}
                            disabled={submitting}
                            data-testid="join-invite"
                            autoFocus={code === ""}
                            {...(describedBy !== undefined ? { "aria-describedby": describedBy } : {})}
                        />
                    )}
                </Field>
                <Field label="Room code" hint="Letters and numbers, six characters.">
                    {({ id, describedBy }) => (
                        <input
                            id={id}
                            className="input input--code"
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
                            {...(describedBy !== undefined ? { "aria-describedby": describedBy } : {})}
                        />
                    )}
                </Field>
                <Field label="Display name" hint="How you appear to everyone in the room. Up to 32 characters.">
                    {({ id, describedBy }) => (
                        <input
                            id={id}
                            className="input"
                            type="text"
                            value={displayName}
                            onChange={(e) => setDisplayName(e.target.value)}
                            placeholder="Your name"
                            maxLength={32}
                            disabled={submitting}
                            required
                            autoFocus={code !== ""}
                            {...(describedBy !== undefined ? { "aria-describedby": describedBy } : {})}
                        />
                    )}
                </Field>
                {error !== null && <FormError>{error}</FormError>}
                <div className="form-actions">
                    <LinkButton to="/rooms" variant="ghost">
                        Cancel
                    </LinkButton>
                    <Button type="submit" variant="primary" disabled={!codeValid || !nameValid || submitting}>
                        {submitting && <Spinner />}
                        {submitting ? "Joining..." : "Join room"}
                    </Button>
                </div>
            </form>
        </div>
    );
}
