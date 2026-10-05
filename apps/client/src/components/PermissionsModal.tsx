import { useCallback, useState } from "react";
import type { MouseEvent } from "react";
import { useRoomStore } from "../stores/useRoomStore";
import { setCapabilities, CAP } from "../services/permissions";
import type { Cap } from "../services/permissions";
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

    // A click on the backdrop itself does nothing (the modal
    // closes only through X / Apply). Clicks inside the panel
    // bubble here too and must keep their default action, or
    // the preset radios can never change.
    const handleBackdropClick = useCallback(
        (e: MouseEvent) => {
            if (e.target !== e.currentTarget) return;
            e.preventDefault();
            e.stopPropagation();
        },
        [],
    );

    const handleClose = useCallback(() => {
        onClose();
    }, [onClose]);

    const handleApply = useCallback(async () => {
        if (!summary) return;
        setApplying(true);
        try {
            for (const p of summary.participants) {
                if (p.is_host) continue;
                const newPreset = selections[p.user_id] ?? "viewer";
                if (newPreset === initialSelections[p.user_id]) continue;
                // Replace, not add: moving someone down to Viewer
                // must take the higher preset's caps away.
                await setCapabilities(p.user_id, presetToCaps(newPreset));
            }
        } finally {
            setApplying(false);
            onClose();
        }
    }, [summary, selections, initialSelections, onClose]);

    const handlePresetChange = useCallback((userId: string, preset: Preset) => {
        setSelections((prev) => ({ ...prev, [userId]: preset }));
    }, []);

    return (
        <div
            className="permissions-modal__backdrop"
            onClick={handleBackdropClick}
            role="presentation"
        >
            <dialog
                className="permissions-modal__panel"
                open
                aria-modal="true"
                aria-labelledby="permissions-modal-title"
            >
                <header className="permissions-modal__header">
                    <h2 id="permissions-modal-title" className="permissions-modal__title">
                        Permissions
                    </h2>
                    <button
                        className="permissions-modal__close"
                        onClick={handleClose}
                        aria-label="Close"
                    >
                        X
                    </button>
                </header>

                <ul className="permissions-modal__list">
                    {summary?.participants.map((p) => {
                        const preset = selections[p.user_id] ?? "viewer";
                        return (
                            <li key={p.user_id} className="permissions-modal__participant">
                                <div className="permissions-modal__participant-info">
                                    <span className="permissions-modal__participant-name">
                                        {p.display_name}
                                    </span>
                                    {p.is_host && (
                                        <span className="permissions-modal__badge">Host</span>
                                    )}
                                </div>
                                {!p.is_host && (
                                    <div className="permissions-modal__preset-group">
                                        <label className="permissions-modal__preset">
                                            <input
                                                type="radio"
                                                name={`preset-${p.user_id}`}
                                                value="viewer"
                                                checked={preset === "viewer"}
                                                onChange={() =>
                                                    handlePresetChange(p.user_id, "viewer")
                                                }
                                            />
                                            <span>Viewer</span>
                                        </label>
                                        <label className="permissions-modal__preset">
                                            <input
                                                type="radio"
                                                name={`preset-${p.user_id}`}
                                                value="editor"
                                                checked={preset === "editor"}
                                                onChange={() =>
                                                    handlePresetChange(p.user_id, "editor")
                                                }
                                            />
                                            <span>Editor</span>
                                        </label>
                                        <label className="permissions-modal__preset">
                                            <input
                                                type="radio"
                                                name={`preset-${p.user_id}`}
                                                value="co-host"
                                                checked={preset === "co-host"}
                                                onChange={() =>
                                                    handlePresetChange(p.user_id, "co-host")
                                                }
                                            />
                                            <span>Co-host</span>
                                        </label>
                                    </div>
                                )}
                            </li>
                        );
                    })}
                </ul>

                <footer className="permissions-modal__footer">
                    <button
                        className="permissions-modal__apply"
                        onClick={handleApply}
                        disabled={applying}
                    >
                        {applying ? "Applying..." : "Apply"}
                    </button>
                </footer>
            </dialog>
        </div>
    );
}
