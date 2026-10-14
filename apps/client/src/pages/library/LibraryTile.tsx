import { forwardRef, useState } from "react";
import type { KeyboardEventHandler } from "react";
import type { LibraryItem } from "../../services/mediaCatalog";
import { Button, Icon, StatusPill } from "../../components/ui";
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

/** "1920x1080 / h264 / matroska": whatever the importer found, nothing invented. */
function details(item: LibraryItem): string {
    const parts: string[] = [];
    if (item.width !== null && item.height !== null) parts.push(`${item.width}×${item.height}`);
    if (item.video_codec !== null) parts.push(item.video_codec);
    if (item.container !== null) parts.push(item.container);
    return parts.join(" · ");
}

export const LibraryTile = forwardRef<HTMLElement, LibraryTileProps>(function LibraryTile(
    { item, active, onFocusTile, onKeyDown, onPlay, onMakePermanent, onDelete },
    ref,
) {
    const [confirming, setConfirming] = useState(false);
    const temporary = item.status === "temporary";
    const detail = details(item);

    return (
        <li className="library-grid__cell">
            <article
                ref={ref}
                className="media-row"
                data-testid="library-tile"
                data-media-id={item.id}
                tabIndex={active ? 0 : -1}
                aria-label={item.filename}
                onFocus={onFocusTile}
                onKeyDown={onKeyDown}
            >
                <span className="media-row__icon" aria-hidden="true">
                    <Icon name="film" />
                </span>
                <div className="media-row__main">
                    <h2 className="media-row__name" title={item.filename}>
                        {item.filename}
                    </h2>
                    {detail !== "" && <p className="media-row__detail">{detail}</p>}
                </div>
                <span className="media-row__stat media-row__stat--duration">
                    {item.duration_ms !== null ? formatDuration(item.duration_ms) : ""}
                </span>
                <span className="media-row__stat media-row__stat--size">{formatBytes(item.size_bytes)}</span>
                <span className="media-row__status">
                    <StatusPill tone={temporary ? "warning" : "neutral"} testId="library-tile-status">
                        {temporary ? "Temporary" : "Permanent"}
                    </StatusPill>
                </span>
                {confirming ? (
                    <div className="media-row__actions media-row__actions--confirm" role="group" aria-label={`Delete ${item.filename}?`}>
                        <span className="media-row__confirm-text">Delete from your library?</span>
                        <Button variant="danger-solid" size="sm" onClick={() => onDelete(item.id)}>
                            Confirm delete
                        </Button>
                        <Button size="sm" onClick={() => setConfirming(false)}>
                            Cancel
                        </Button>
                    </div>
                ) : (
                    <div className="media-row__actions">
                        <Button
                            variant="primary"
                            size="sm"
                            icon="play"
                            aria-label={`Play ${item.filename}`}
                            onClick={() => onPlay(item)}
                        >
                            Play
                        </Button>
                        {temporary && (
                            <Button size="sm" onClick={() => onMakePermanent(item.id)}>
                                Make permanent
                            </Button>
                        )}
                        <Button
                            variant="ghost"
                            size="sm"
                            iconOnly
                            icon="trash"
                            title="Delete"
                            aria-label={`Delete ${item.filename}`}
                            onClick={() => setConfirming(true)}
                        />
                    </div>
                )}
            </article>
        </li>
    );
});
