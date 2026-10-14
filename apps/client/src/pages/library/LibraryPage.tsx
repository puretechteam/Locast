import { useEffect } from "react";
import { useNavigate } from "react-router-dom";
import { useMediaStore } from "../../stores/useMediaStore";
import { useRoomStore } from "../../stores/useRoomStore";
import type { LibraryItem } from "../../services/mediaCatalog";
import { Button, EmptyState, Icon, LinkButton, Notice, PageHeader, Spinner } from "../../components/ui";
import { LibraryGrid } from "./LibraryGrid";
import { formatBytes } from "./format";
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
    // A truly empty library gets one prominent Import action in its empty
    // state instead of a second, duplicate button in the header.
    const emptyLibrary = loadedEmpty && !searching;
    const showToolbar = searching || (status === "ready" && items.length > 0);

    let description: string;
    if (status !== "ready") description = "Your local media, ready to watch together.";
    else if (searching) description = `${items.length} ${items.length === 1 ? "match" : "matches"} for “${query.trim()}”`;
    else if (items.length === 0) description = "Import video files to watch them together with friends.";
    else {
        const total = formatBytes(items.reduce((sum, i) => sum + i.size_bytes, 0));
        description = `${items.length} ${items.length === 1 ? "file" : "files"}${total !== "" ? ` · ${total}` : ""}`;
    }

    const importButton = (
        <Button
            variant="primary"
            icon={importing ? undefined : "upload"}
            onClick={() => void importFiles()}
            disabled={importing}
        >
            {importing && <Spinner />}
            {importing ? "Importing..." : "Import files"}
        </Button>
    );

    return (
        <div className="page library">
            <PageHeader
                title="Library"
                description={description}
                actions={
                    <>
                        <LinkButton to="/rooms/join" icon="join">
                            Join a room
                        </LinkButton>
                        <LinkButton to="/rooms/new" icon="plus">
                            Create a room
                        </LinkButton>
                        {!emptyLibrary && importButton}
                    </>
                }
            />

            {notice !== null && (
                <Notice
                    kind={notice.kind === "error" ? "error" : "success"}
                    testId="library-notice"
                    actions={
                        <Button variant="ghost" size="sm" onClick={dismissNotice}>
                            Dismiss
                        </Button>
                    }
                >
                    {notice.message}
                </Notice>
            )}

            {importing && (
                <div className="library__importing" aria-busy="true">
                    <p>Importing your files. Large videos can take a little while.</p>
                    <div className="progress progress--indeterminate">
                        <span />
                    </div>
                </div>
            )}

            {showToolbar && (
                <div className="library__toolbar">
                    <div className="search">
                        <Icon name="search" className="search__icon" />
                        <input
                            type="search"
                            className="input"
                            aria-label="Search library"
                            placeholder="Search by file name"
                            value={query}
                            onChange={(e) => void setQuery(e.target.value)}
                        />
                    </div>
                </div>
            )}

            {(status === "idle" || status === "loading") && (
                <p className="loading" role="status" aria-busy="true">
                    <Spinner />
                    Loading your library...
                </p>
            )}

            {status === "error" && (
                <EmptyState
                    icon="alert"
                    tone="error"
                    role="alert"
                    title="Could not load your library"
                    actions={
                        <Button icon="refresh" onClick={() => void refresh()}>
                            Try again
                        </Button>
                    }
                >
                    {error}
                </EmptyState>
            )}

            {emptyLibrary && (
                <EmptyState
                    icon="film"
                    title="Add your first video"
                    testId="library-empty"
                    actions={
                        <>
                            {importButton}
                            <LinkButton to="/rooms/join" icon="join">
                                Join a room
                            </LinkButton>
                        </>
                    }
                >
                    Your library is empty. Import video files from this computer, then share one in a room and
                    your friends receive it directly from you.
                </EmptyState>
            )}

            {loadedEmpty && searching && (
                <EmptyState
                    icon="search"
                    plain
                    title="No matching files"
                    testId="library-no-matches"
                    actions={
                        <Button onClick={() => void setQuery("")}>Clear search</Button>
                    }
                >
                    No files match &ldquo;{query.trim()}&rdquo;. Check the spelling or try a shorter word.
                </EmptyState>
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
