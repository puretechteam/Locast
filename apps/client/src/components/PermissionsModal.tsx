import { useCallback, useState } from "react";
import { useRoomStore } from "../stores/useRoomStore";
import { errorText } from "../services/errors";
import { setCapabilities, CAP } from "../services/permissions";
import type { Cap } from "../services/permissions";
import { Button, Dialog, Spinner } from "./ui";
import "../styles/permissions-modal.css";

type Preset = "viewer" | "editor" | "co-host";

const PRESET_CAPS: Record<Preset, Cap[]> = {
    // The server's default for a joining participant.
    viewer: [CAP.CHAT],
    // P5-T03: an Editor can undo their own strokes and a Co-host can also
    // clear the canvas (architecture 14.7). UNDO_ANY is not part of any
    // preset; it needs the per-user capability editor.
    editor: [CAP.PLAYBACK_CONTROL, CAP.DRAW, CAP.LASER, CAP.CHAT, CAP.UNDO_OWN],
    "co-host": [CAP.PLAYBACK_CONTROL, CAP.DRAW, CAP.LASER, CAP.MANAGE_ROOM, CAP.KICK, CAP.PUBLISH_MANIFEST, CAP.INVITE, CAP.CHAT, CAP.UNDO_OWN, CAP.CLEAR_ALL],
};

function presetToCaps(preset: Preset): number {
    return PRESET_CAPS[preset].reduce((acc, cap) => acc | cap, 0);
}

/** The preset a participant's known cap set matches exactly,
 *  or "viewer". A participant's `cap_set` is only known after
 *  a CAPABILITY_UPDATE (room snapshots carry 0), which is why
 *  Apply sends only the rows the host changed. */
function capsToPreset(caps: number): Preset {
    if (caps === presetToCaps("co-host")) return "co-host";
    if (caps === presetToCaps("editor")) return "editor";
    return "viewer";
}

interface PermissionsModalProps {
    onClose: () => void;
}

export function PermissionsModal({ onClose }: PermissionsModalProps): JSX.Element {
    const summary = useRoomStore((s) => s.summary);
    const [selections, setSelections] = useState<Record<string, Preset>>(() => {
        const initial: Record<string, Preset> = {};
        for (const p of summary?.participants ?? []) {
            initial[p.user_id] = capsToPreset(p.cap_set);
        }
        return initial;
    });
    const [initialSelections] = useState(selections);
    const [applying, setApplying] = useState(false);
    const [error, setError] = useState<string | null>(null);

    const handleClose = useCallback(() => {
        onClose();
    }, [onClose]);

    const handleApply = useCallback(async () => {
        if (!summary || applying) return;
        setApplying(true);
        setError(null);
        try {
            for (const p of summary.participants) {
                if (p.is_host) continue;
                // Someone who joined after the modal opened was never shown
                // here; defaulting them to Viewer would overwrite their caps.
                if (!(p.user_id in initialSelections)) continue;
                const newPreset = selections[p.user_id] ?? "viewer";
                if (newPreset === initialSelections[p.user_id]) continue;
                // Replace, not add: moving someone down to Viewer
                // must take the higher preset's caps away.
                await setCapabilities(p.user_id, presetToCaps(newPreset));
            }
            onClose();
        } catch (err) {
            // Stay open and say so: closing here made a failure look like
            // success, with only some of the changes applied. Retrying is
            // safe because each call replaces a participant's caps.
            setError(`Could not apply all changes: ${errorText(err)}`);
        } finally {
            setApplying(false);
        }
    }, [summary, applying, selections, initialSelections, onClose]);

    const handlePresetChange = useCallback((userId: string, preset: Preset) => {
        setSelections((prev) => ({ ...prev, [userId]: preset }));
    }, []);

    const PRESETS: Array<{ value: Preset; label: string }> = [
        { value: "viewer", label: "Viewer" },
        { value: "editor", label: "Editor" },
        { value: "co-host", label: "Co-host" },
    ];

    return (
        <Dialog
            title="Permissions"
            wide
            className="permissions-modal__panel"
            backdropClassName="permissions-modal__backdrop"
            onEscape={applying ? undefined : handleClose}
            onClose={handleClose}
            closeDisabled={applying}
            footer={
                <>
                    <Button onClick={handleClose} disabled={applying}>
                        Cancel
                    </Button>
                    <Button
                        variant="primary"
                        className="permissions-modal__apply"
                        onClick={handleApply}
                        disabled={applying}
                    >
                        {applying && <Spinner />}
                        {applying ? "Applying..." : "Apply"}
                    </Button>
                </>
            }
        >
            <p className="permissions-modal__help">
                <strong>Viewer</strong> can chat. <strong>Editor</strong> can also control playback, draw and use
                the laser pointer. <strong>Co-host</strong> can also manage the room.
            </p>

            <ul className="permissions-modal__list">
                {summary?.participants.map((p) => {
                    const preset = selections[p.user_id] ?? "viewer";
                    return (
                        <li key={p.user_id} className="permissions-modal__participant">
                            <div className="permissions-modal__participant-info">
                                <span className="permissions-modal__participant-name" title={p.display_name}>
                                    {p.display_name.trim() || "Unnamed"}
                                </span>
                                {p.is_host && <span className="permissions-modal__badge">Host</span>}
                            </div>
                            {!p.is_host && (
                                <div
                                    className="permissions-modal__preset-group"
                                    role="radiogroup"
                                    aria-label={`Role for ${p.display_name.trim() || "Unnamed"}`}
                                >
                                    {PRESETS.map((opt) => (
                                        <label
                                            key={opt.value}
                                            className={`permissions-modal__preset${preset === opt.value ? " permissions-modal__preset--on" : ""}`}
                                        >
                                            <input
                                                type="radio"
                                                name={`preset-${p.user_id}`}
                                                value={opt.value}
                                                checked={preset === opt.value}
                                                onChange={() => handlePresetChange(p.user_id, opt.value)}
                                            />
                                            <span>{opt.label}</span>
                                        </label>
                                    ))}
                                </div>
                            )}
                        </li>
                    );
                })}
            </ul>

            {error !== null && (
                <p className="dialog__error" role="alert" data-testid="permissions-error">
                    {error}
                </p>
            )}
        </Dialog>
    );
}
