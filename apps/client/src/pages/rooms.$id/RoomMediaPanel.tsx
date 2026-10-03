import { useEffect, useState } from "react";
import { listLibrary, resolveMediaUrl } from "../../services/mediaCatalog";
import type { LibraryItem } from "../../services/mediaCatalog";
import { getInviteUrl, shareMedia } from "../../services/sharedMedia";
import type { SharedMediaIpc } from "../../services/sharedMedia";
import { usePlaybackStore } from "../../stores/usePlaybackStore";
import { useSharedMediaStore } from "../../stores/useSharedMediaStore";
import type { SharedItemStatus } from "../../stores/useSharedMediaStore";

function messageOf(err: unknown): string {
    if (err instanceof Error) return err.message;
    if (typeof err === "string") return err;
    if (typeof err === "object" && err !== null) {
        const o = err as { message?: unknown };
        if (typeof o.message === "string") return o.message;
    }
    return String(err);
}

function humanBytes(n: number): string {
    const units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let i = 0;
    let v = n;
    while (v >= 1024 && i < units.length - 1) {
        v /= 1024;
        i++;
    }
    return `${v.toFixed(v >= 100 || i === 0 ? 0 : 1)} ${units[i]}`;
}

function statusLabel(s: SharedItemStatus | undefined): string {
    switch (s?.kind) {
        case undefined:
        case "checking":
            return "Checking library...";
        case "waiting":
            return "Waiting for a connection to the host...";
        case "downloading":
            return "Downloading...";
        case "local":
            return s.dedup ? "Already in your library" : "Downloaded to your library";
        case "error":
            return `Could not get this media: ${s.message}`;
    }
}

/**
 * Slice 3: the room's shared-media controls. The host picks one library
 * item to share and sees the invite link viewers need; a viewer sees what
 * is shared, its acquisition state, and a Play button once it is local.
 * Acquisition itself runs in the global `SharedMediaBridge`.
 */
export function RoomMediaPanel(): JSX.Element {
    const isHost = useSharedMediaStore((s) => s.isHost);
    const manifest = useSharedMediaStore((s) => s.manifest);
    const manifestError = useSharedMediaStore((s) => s.manifestError);
    const items = useSharedMediaStore((s) => s.items);
    const [error, setError] = useState<string | null>(null);

    const play = async (localMediaId: string, title: string) => {
        setError(null);
        try {
            const url = await resolveMediaUrl(localMediaId);
            usePlaybackStore.getState().setLocalMedia(url, title);
        } catch (err) {
            setError(`Could not play ${title}: ${messageOf(err)}`);
        }
    };

    return (
        <section className="room-media" data-testid="room-media" aria-label="Shared media">
            <h3 className="room-media__title">Shared media</h3>
            {isHost && <HostControls current={manifest?.media ?? []} />}
            {manifest === null || manifest.media.length === 0 ? (
                <p className="room-media__empty" data-testid="room-media-empty">
                    {isHost
                        ? "Nothing shared yet."
                        : manifestError !== null
                          ? "The shared media could not be verified. Join with the host's invite link to receive it."
                          : "Waiting for the host to share media."}
                </p>
            ) : (
                <ul className="room-media__list">
                    {manifest.media.map((m: SharedMediaIpc) => {
                        const st = isHost ? undefined : items[m.id];
                        const localId = isHost
                            ? m.id
                            : st?.kind === "local"
                              ? st.localMediaId
                              : null;
                        return (
                            <li
                                key={m.id}
                                className="room-media__item"
                                data-testid="room-media-item"
                                data-media-id={m.id}
                                data-status={isHost ? "shared" : (st?.kind ?? "checking")}
                            >
                                <span className="room-media__name">{m.filename}</span>
                                <span className="room-media__size">{humanBytes(m.size_bytes)}</span>
                                <span className="room-media__status" data-testid="room-media-status">
                                    {isHost ? "Shared with the room" : statusLabel(st)}
                                </span>
                                {localId !== null && (
                                    <button
                                        type="button"
                                        className="room-media__btn"
                                        data-testid="room-media-play"
                                        onClick={() => void play(localId, m.filename)}
                                    >
                                        Play
                                    </button>
                                )}
                            </li>
                        );
                    })}
                </ul>
            )}
            {error !== null && <p className="room-media__error">{error}</p>}
        </section>
    );
}

function HostControls({ current }: { current: SharedMediaIpc[] }): JSX.Element {
    const [invite, setInvite] = useState<string | null>(null);
    const [library, setLibrary] = useState<LibraryItem[]>([]);
    const [selected, setSelected] = useState("");
    const [busy, setBusy] = useState(false);
    const [copied, setCopied] = useState(false);
    const [error, setError] = useState<string | null>(null);

    useEffect(() => {
        let cancelled = false;
        getInviteUrl()
            .then((url) => {
                if (!cancelled) setInvite(url);
            })
            .catch((err: unknown) => {
                if (!cancelled) setError(`Invite link unavailable: ${messageOf(err)}`);
            });
        listLibrary("")
            .then((list) => {
                if (cancelled) return;
                setLibrary(list);
                setSelected((prev) => prev || (list[0]?.id ?? ""));
            })
            .catch((err: unknown) => {
                if (!cancelled) setError(`Could not read the library: ${messageOf(err)}`);
            });
        return () => {
            cancelled = true;
        };
    }, []);

    const onShare = async () => {
        if (selected === "" || busy) return;
        setBusy(true);
        setError(null);
        try {
            // The shared item appears once Rust accepts the signed
            // manifest (`manifest://state`), not optimistically here.
            await shareMedia([selected]);
        } catch (err) {
            setError(`Could not share: ${messageOf(err)}`);
        } finally {
            setBusy(false);
        }
    };

    const onCopy = async () => {
        if (invite === null) return;
        try {
            await navigator.clipboard.writeText(invite);
            setCopied(true);
        } catch {
            setCopied(false);
        }
    };

    const sharedId = current[0]?.id;
    return (
        <div className="room-media__host" data-testid="room-media-host">
            <label className="room-media__row">
                <span>Invite link</span>
                <input
                    type="text"
                    readOnly
                    className="room-media__invite"
                    data-testid="room-media-invite"
                    value={invite ?? ""}
                    placeholder="Loading..."
                    onFocus={(e) => e.currentTarget.select()}
                />
                <button
                    type="button"
                    className="room-media__btn"
                    onClick={() => void onCopy()}
                    disabled={invite === null}
                >
                    {copied ? "Copied" : "Copy"}
                </button>
            </label>
            <label className="room-media__row">
                <span>Media</span>
                <select
                    className="room-media__select"
                    data-testid="room-media-select"
                    value={selected}
                    onChange={(e) => setSelected(e.target.value)}
                    disabled={library.length === 0 || busy}
                >
                    {library.length === 0 && <option value="">Library is empty</option>}
                    {library.map((item) => (
                        <option key={item.id} value={item.id}>
                            {item.filename}
                        </option>
                    ))}
                </select>
                <button
                    type="button"
                    className="room-media__btn"
                    data-testid="room-media-share"
                    onClick={() => void onShare()}
                    disabled={selected === "" || busy || selected === sharedId}
                >
                    {busy ? "Sharing..." : selected === sharedId ? "Shared" : "Share"}
                </button>
            </label>
            {error !== null && <p className="room-media__error">{error}</p>}
        </div>
    );
}
