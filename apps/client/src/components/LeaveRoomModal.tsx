import { useCallback, useEffect, useState } from "react";
import type { MouseEvent } from "react";
import { errorText } from "../services/errors";
import { getTempFilesForRoom, keepTempFiles, deleteTempFiles } from "../services/roomClient";
import type { TempFileInfo } from "../bindings";
import "./LeaveRoomModal.css";

interface LeaveRoomModalProps {
    roomId: string;
    onClose: () => void;
    /** Leave the room. Rejecting keeps the dialog open and shows the error in it. */
    onConfirm: () => Promise<void>;
}

export function LeaveRoomModal({ roomId, onClose, onConfirm }: LeaveRoomModalProps): JSX.Element {
    const [tempFiles, setTempFiles] = useState<TempFileInfo[]>([]);
    const [loadError, setLoadError] = useState<string | null>(null);
    const [processing, setProcessing] = useState(false);
    const [action, setAction] = useState<"keep" | "delete" | null>(null);
    const [error, setError] = useState<string | null>(null);

    useEffect(() => {
        let cancelled = false;
        getTempFilesForRoom(roomId)
            .then((files) => {
                if (!cancelled) setTempFiles(files);
            })
            .catch((err: unknown) => {
                // Do not pretend there are no files: the user would then
                // "delete" nothing and think the room's files were cleaned up.
                if (!cancelled) setLoadError(errorText(err));
            });
        return () => {
            cancelled = true;
        };
    }, [roomId]);

    // Escape closes the dialog, unless a leave is in progress.
    useEffect(() => {
        const onKeyDown = (e: KeyboardEvent) => {
            if (e.key === "Escape" && !processing) onClose();
        };
        document.addEventListener("keydown", onKeyDown);
        return () => document.removeEventListener("keydown", onKeyDown);
    }, [onClose, processing]);

    const handleBackdropClick = useCallback(
        (e: MouseEvent) => {
            e.preventDefault();
            e.stopPropagation();
        },
        [],
    );

    // Apply the file choice, then leave. Any failure is shown in this dialog
    // (the footer's own error text sits behind the full-screen backdrop), the
    // buttons stay disabled until it settles, and the dialog stays open so the
    // user can retry or cancel.
    const run = useCallback(
        async (which: "keep" | "delete") => {
            if (processing) return;
            setProcessing(true);
            setAction(which);
            setError(null);
            try {
                const ids = tempFiles.map((f) => f.file_id);
                if (which === "keep") {
                    await keepTempFiles(ids);
                } else {
                    await deleteTempFiles(ids);
                }
                await onConfirm();
            } catch (err) {
                setError(errorText(err));
            } finally {
                setProcessing(false);
            }
        },
        [processing, tempFiles, onConfirm],
    );

    return (
        <div
            className="lrm-backdrop"
            onClick={handleBackdropClick}
            role="presentation"
        >
            <dialog
                className="lrm-panel"
                open
                aria-modal="true"
                aria-labelledby="lrm-title"
            >
                <header className="lrm-header">
                    <h2 id="lrm-title" className="lrm-title">Leave Room</h2>
                    <button
                        className="lrm-close"
                        onClick={onClose}
                        aria-label="Close"
                        disabled={processing}
                    >
                        X
                    </button>
                </header>

                {loadError !== null ? (
                    <p className="lrm-desc" role="alert" data-testid="lrm-load-error">
                        Could not list this room&apos;s temporary files ({loadError}). They are
                        kept as temporary files in your library. You can still leave.
                    </p>
                ) : tempFiles.length > 0 ? (
                    <>
                        <p className="lrm-desc">
                            This room has {tempFiles.length} temporary file{tempFiles.length !== 1 ? "s" : ""}:
                        </p>
                        <ul className="lrm-list">
                            {tempFiles.map((f) => (
                                <li key={f.file_id} className="lrm-item">
                                    <span className="lrm-item-name">{f.filename}</span>
                                    <span className="lrm-item-size">{f.size_bytes}</span>
                                </li>
                            ))}
                        </ul>
                        <p className="lrm-choice">What would you like to do with them?</p>
                    </>
                ) : (
                    <p className="lrm-desc">No temporary files in this room.</p>
                )}

                {error !== null && (
                    <p className="lrm-desc" role="alert" data-testid="lrm-error">
                        {error}
                    </p>
                )}

                <footer className="lrm-footer">
                    <button
                        className="lrm-btn lrm-btn--keep"
                        onClick={() => void run("keep")}
                        disabled={processing}
                    >
                        {processing && action === "keep" ? "Keeping..." : "Keep"}
                    </button>
                    <button
                        className="lrm-btn lrm-btn--delete"
                        onClick={() => void run("delete")}
                        disabled={processing || loadError !== null}
                    >
                        {processing && action === "delete" ? "Deleting..." : "Delete"}
                    </button>
                    <button
                        className="lrm-btn lrm-btn--cancel"
                        onClick={onClose}
                        disabled={processing}
                    >
                        Cancel
                    </button>
                </footer>
            </dialog>
        </div>
    );
}
