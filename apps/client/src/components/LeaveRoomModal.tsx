import { useCallback, useEffect, useState } from "react";
import { errorText } from "../services/errors";
import { getTempFilesForRoom, keepTempFiles, deleteTempFiles } from "../services/roomClient";
import type { TempFileInfo } from "../bindings";
import { formatBytes } from "../pages/library/format";
import { Button, Dialog, Icon, Spinner } from "./ui";
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
    const [loaded, setLoaded] = useState(false);
    const [processing, setProcessing] = useState(false);
    const [action, setAction] = useState<"keep" | "delete" | null>(null);
    const [error, setError] = useState<string | null>(null);

    useEffect(() => {
        let cancelled = false;
        getTempFilesForRoom(roomId)
            .then((files) => {
                if (!cancelled) {
                    setTempFiles(files);
                    setLoaded(true);
                }
            })
            .catch((err: unknown) => {
                // Do not pretend there are no files: the user would then
                // "delete" nothing and think the room's files were cleaned up.
                if (!cancelled) {
                    setLoadError(errorText(err));
                    setLoaded(true);
                }
            });
        return () => {
            cancelled = true;
        };
    }, [roomId]);

    // Apply the file choice, then leave. Any failure is shown in this dialog,
    // the buttons stay disabled until it settles, and the dialog stays open so
    // the user can retry or cancel.
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

    const hasFiles = tempFiles.length > 0;
    const askAboutFiles = hasFiles || loadError !== null;

    return (
        <Dialog
            title="Leave room"
            className="lrm-panel"
            backdropClassName="lrm-backdrop"
            // Escape closes the dialog, unless a leave is in progress.
            onEscape={processing ? undefined : onClose}
            onClose={onClose}
            closeDisabled={processing}
            footer={
                <>
                    <Button className="lrm-btn--cancel" onClick={onClose} disabled={processing} data-autofocus>
                        Cancel
                    </Button>
                    {askAboutFiles && (
                        <Button
                            variant="danger"
                            className="lrm-btn--delete"
                            onClick={() => void run("delete")}
                            disabled={processing || loadError !== null}
                        >
                            {processing && action === "delete" ? <Spinner /> : <Icon name="trash" />}
                            {processing && action === "delete" ? "Deleting..." : "Delete files and leave"}
                        </Button>
                    )}
                    <Button
                        variant={askAboutFiles ? "secondary" : "primary"}
                        className="lrm-btn--keep"
                        onClick={() => void run("keep")}
                        disabled={processing || !loaded}
                    >
                        {processing && action === "keep" ? <Spinner /> : null}
                        {processing && action === "keep"
                            ? "Leaving..."
                            : askAboutFiles
                              ? "Keep files and leave"
                              : "Leave room"}
                    </Button>
                </>
            }
        >
            {!loaded ? (
                <p className="lrm-desc">Checking this room&apos;s downloaded files...</p>
            ) : loadError !== null ? (
                <p className="lrm-desc" role="alert" data-testid="lrm-load-error">
                    Could not list this room&apos;s temporary files ({loadError}). They are kept as temporary
                    files in your library. You can still leave.
                </p>
            ) : hasFiles ? (
                <>
                    <p className="lrm-desc">
                        This room added {tempFiles.length} temporary file{tempFiles.length !== 1 ? "s" : ""} to
                        your library. Keep them to watch later, or delete them to free up space.
                    </p>
                    <ul className="lrm-list">
                        {tempFiles.map((f) => (
                            <li key={f.file_id} className="lrm-item">
                                <span className="lrm-item-name" title={f.filename}>
                                    {f.filename}
                                </span>
                                <span className="lrm-item-size">{formatBytes(f.size_bytes)}</span>
                            </li>
                        ))}
                    </ul>
                </>
            ) : (
                <p className="lrm-desc">No temporary files in this room. Everyone else stays in the room.</p>
            )}

            {error !== null && (
                <p className="dialog__error" role="alert" data-testid="lrm-error">
                    {error}
                </p>
            )}
        </Dialog>
    );
}
