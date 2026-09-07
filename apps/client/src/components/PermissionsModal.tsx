import { useCallback, useState } from "react";
import type { MouseEvent } from "react";
import { useRoomStore } from "../stores/useRoomStore";
import { grantCapability, CAP } from "../services/permissions";
import type { Cap } from "../services/permissions";
import "../styles/permissions-modal.css";

type Preset = "viewer" | "editor" | "co-host";

const PRESET_CAPS: Record<Preset, Cap[]> = {
    viewer: [],
    editor: [CAP.PLAYBACK_CONTROL, CAP.DRAW, CAP.LASER, CAP.CHAT],
    "co-host": [CAP.PLAYBACK_CONTROL, CAP.DRAW, CAP.LASER, CAP.MANAGE_ROOM, CAP.KICK, CAP.PUBLISH_MANIFEST, CAP.INVITE, CAP.CHAT],
};

function presetToCaps(preset: Preset): number {
    return PRESET_CAPS[preset].reduce((acc, cap) => acc | cap, 0);
}

interface PermissionsModalProps {
    onClose: () => void;
}

export function PermissionsModal({ onClose }: PermissionsModalProps): JSX.Element {
    const summary = useRoomStore((s) => s.summary);
    const [selections, setSelections] = useState<Record<string, Preset>>(() => {
        const initial: Record<string, Preset> = {};
        for (const p of summary?.participants ?? []) {
            initial[p.user_id] = "viewer";
        }
        return initial;
    });
    const [applying, setApplying] = useState(false);

    const handleBackdropClick = useCallback(
        (e: MouseEvent) => {
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
                const newCaps = presetToCaps(newPreset);
                const oldCaps = 0;
                if (newCaps !== oldCaps) {
                    if (newCaps !== 0) {
                        await grantCapability(p.user_id, newCaps as Cap);
                    }
                }
            }
        } finally {
            setApplying(false);
            onClose();
        }
    }, [summary, selections, onClose]);

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
