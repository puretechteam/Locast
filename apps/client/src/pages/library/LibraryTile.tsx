import { forwardRef, useState } from "react";
import type { KeyboardEventHandler } from "react";
import type { LibraryItem } from "../../services/mediaCatalog";
import { formatBytes, formatDuration } from "./format";

interface LibraryTileProps {
    item: LibraryItem;
    /** Roving tabindex: only the active tile is in the Tab order. */
    active: boolean;
    onFocusTile: () => void;
    onKeyDown: KeyboardEventHandler<HTMLElement>;
    onPlay: (item: LibraryItem) => void;
    onMakePermanent: (id: string) => void;
    onDelete: (id: string) => void;
}

function describe(item: LibraryItem): string {
    const parts: string[] = [formatBytes(item.size_bytes)];
    if (item.duration_ms !== null) parts.push(formatDuration(item.duration_ms));
    if (item.width !== null && item.height !== null) parts.push(`${item.width}×${item.height}`);
    if (item.container !== null) parts.push(item.container);
    return parts.filter((p) => p !== "").join(" · ");
}

export const LibraryTile = forwardRef<HTMLElement, LibraryTileProps>(function LibraryTile(
    { item, active, onFocusTile, onKeyDown, onPlay, onMakePermanent, onDelete },
    ref,
) {
    const [confirming, setConfirming] = useState(false);
    const temporary = item.status === "temporary";

    return (
        <li className="library-grid__cell">
            <article
                ref={ref}
                className="library-tile"
                data-testid="library-tile"
                data-media-id={item.id}
                tabIndex={active ? 0 : -1}
                aria-label={item.filename}
                onFocus={onFocusTile}
                onKeyDown={onKeyDown}
            >
                <h2 className="library-tile__name" title={item.filename}>
                    {item.filename}
                </h2>
                <p className="library-tile__meta">{describe(item)}</p>
                <p
                    className={`library-tile__badge library-tile__badge--${item.status}`}
                    data-testid="library-tile-status"
                >
                    {temporary ? "Temporary" : "Permanent"}
                </p>
                {confirming ? (
                    <div className="library-tile__actions" role="group" aria-label={`Delete ${item.filename}?`}>
                        <span className="library-tile__confirm-text">Delete from your library?</span>
                        <button
                            type="button"
                            className="library-btn library-btn--danger"
                            onClick={() => onDelete(item.id)}
                        >
                            Confirm delete
                        </button>
                        <button type="button" className="library-btn" onClick={() => setConfirming(false)}>
                            Cancel
                        </button>
                    </div>
                ) : (
                    <div className="library-tile__actions">
                        <button
                            type="button"
                            className="library-btn library-btn--primary"
                            aria-label={`Play ${item.filename}`}
                            onClick={() => onPlay(item)}
                        >
                            Play
                        </button>
                        {temporary && (
                            <button
                                type="button"
                                className="library-btn"
                                onClick={() => onMakePermanent(item.id)}
                            >
                                Make permanent
                            </button>
                        )}
                        <button
                            type="button"
                            className="library-btn"
                            aria-label={`Delete ${item.filename}`}
                            onClick={() => setConfirming(true)}
                        >
                            Delete
                        </button>
                    </div>
                )}
            </article>
        </li>
    );
});
