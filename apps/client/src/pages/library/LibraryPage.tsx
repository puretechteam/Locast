import { useEffect } from "react";
import { Link, useNavigate } from "react-router-dom";
import { useMediaStore } from "../../stores/useMediaStore";
import { useRoomStore } from "../../stores/useRoomStore";
import type { LibraryItem } from "../../services/mediaCatalog";
import { LibraryGrid } from "./LibraryGrid";
import "../../styles/library.css";

export function LibraryPage(): JSX.Element {
    const items = useMediaStore((s) => s.items);
    const query = useMediaStore((s) => s.query);
    const status = useMediaStore((s) => s.status);
    const error = useMediaStore((s) => s.error);
    const importing = useMediaStore((s) => s.importing);
    const notice = useMediaStore((s) => s.notice);
    const refresh = useMediaStore((s) => s.refresh);
    const setQuery = useMediaStore((s) => s.setQuery);
    const importFiles = useMediaStore((s) => s.importFiles);
    const makePermanent = useMediaStore((s) => s.makePermanent);
    const remove = useMediaStore((s) => s.remove);
    const dismissNotice = useMediaStore((s) => s.dismissNotice);
    const playItem = useMediaStore((s) => s.playItem);
    const roomId = useRoomStore((s) => s.summary?.id ?? null);
    const navigate = useNavigate();

    // Play opens the player: the current room if the user is in one, otherwise
    // the local player (`/rooms/local`, no networking).
    async function onPlay(item: LibraryItem): Promise<void> {
        if (await playItem(item)) navigate(`/rooms/${roomId ?? "local"}`);
    }

    useEffect(() => {
        void refresh();
    }, [refresh]);

    const searching = query.trim() !== "";
    const loadedEmpty = status === "ready" && items.length === 0;

    return (
        <div className="library">
            <div className="library__toolbar">
                <input
                    type="search"
                    className="library__search"
                    aria-label="Search library"
                    placeholder="Search by file name"
                    value={query}
                    onChange={(e) => void setQuery(e.target.value)}
                />
                <button
                    type="button"
                    className="library-btn library-btn--primary"
                    onClick={() => void importFiles()}
                    disabled={importing}
                >
                    {importing ? "Importing..." : "Import files"}
                </button>
                <nav className="library__links" aria-label="Rooms">
                    <Link to="/rooms/new">Create a room</Link>
                    <Link to="/rooms/join">Join a room</Link>
                    <Link to="/rooms">Rooms</Link>
                </nav>
            </div>

            {notice !== null && (
                <p
                    className={`library__notice library__notice--${notice.kind}`}
                    role={notice.kind === "error" ? "alert" : "status"}
                    data-testid="library-notice"
                >
                    <span>{notice.message}</span>
                    <button type="button" className="library-btn" onClick={dismissNotice}>
                        Dismiss
                    </button>
                </p>
            )}

            {(status === "idle" || status === "loading") && (
                <p className="library__state" role="status" aria-busy="true">
                    Loading your library...
                </p>
            )}

            {status === "error" && (
                <div className="library__state library__state--error" role="alert">
                    <p>Could not load your library: {error}</p>
                    <button type="button" className="library-btn" onClick={() => void refresh()}>
                        Try again
                    </button>
                </div>
            )}

            {loadedEmpty && !searching && (
                <div className="library__state" data-testid="library-empty">
                    <p>Your library is empty.</p>
                    <p>Import media files from your computer to start watching with friends.</p>
                </div>
            )}

            {loadedEmpty && searching && (
                <p className="library__state" data-testid="library-no-matches">
                    No files match &ldquo;{query.trim()}&rdquo;.
                </p>
            )}

            {items.length > 0 && (
                <LibraryGrid
                    items={items}
                    onPlay={(item) => void onPlay(item)}
                    onMakePermanent={(id) => void makePermanent(id)}
                    onDelete={(id) => void remove(id)}
                />
            )}
        </div>
    );
}
