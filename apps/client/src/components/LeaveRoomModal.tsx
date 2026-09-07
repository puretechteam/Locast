import { useCallback, useEffect, useState } from "react";
import type { MouseEvent } from "react";
import { getTempFilesForRoom, keepTempFiles, deleteTempFiles } from "../services/roomClient";
import type { TempFileInfo } from "../bindings";
import "./LeaveRoomModal.css";

interface LeaveRoomModalProps {
    roomId: string;
    onClose: () => void;
    onConfirm: () => void;
}

export function LeaveRoomModal({ roomId, onClose, onConfirm }: LeaveRoomModalProps): JSX.Element {
    const [tempFiles, setTempFiles] = useState<TempFileInfo[]>([]);
    const [processing, setProcessing] = useState(false);
    const [action, setAction] = useState<"keep" | "delete" | null>(null);

    useEffect(() => {
        getTempFilesForRoom(roomId).then(setTempFiles).catch(() => setTempFiles([]));
    }, [roomId]);

    const handleBackdropClick = useCallback(
        (e: MouseEvent) => {
            e.preventDefault();
            e.stopPropagation();
        },
        [],
    );

    const handleKeep = useCallback(async () => {
        setProcessing(true);
        setAction("keep");
        try {
            await keepTempFiles(tempFiles.map((f) => f.file_id));
            onConfirm();
        } finally {
            setProcessing(false);
        }
    }, [tempFiles, onConfirm]);

    const handleDelete = useCallback(async () => {
        setProcessing(true);
        setAction("delete");
        try {
            await deleteTempFiles(tempFiles.map((f) => f.file_id));
            onConfirm();
        } finally {
            setProcessing(false);
        }
    }, [tempFiles, onConfirm]);

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
                    <button className="lrm-close" onClick={onClose} aria-label="Close">X</button>
                </header>

                {tempFiles.length > 0 ? (
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

                <footer className="lrm-footer">
                    <button
                        className="lrm-btn lrm-btn--keep"
                        onClick={handleKeep}
                        disabled={processing}
                    >
                        {processing && action === "keep" ? "Keeping..." : "Keep"}
                    </button>
                    <button
                        className="lrm-btn lrm-btn--delete"
                        onClick={handleDelete}
                        disabled={processing}
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
