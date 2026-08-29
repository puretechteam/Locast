import { useEffect, useState } from "react";
import { Link } from "react-router-dom";
import { listRecentRooms, upsertRecentRoom } from "../../services/recentRooms";
import { onRoomStateChange } from "../../services/room";
import { useRoomStore } from "../../stores/useRoomStore";
import { commands } from "../../services/ipc";
import type { RecentRoomEntry, RecentRoomRole } from "../../services/recentRooms";
import type { RoomSummaryIpc } from "../../services/room";

const RECENTS_REFRESH_MS = 5_000;
const IDENTITY_PLACEHOLDER_NAME = "guest";
const UPSERT_DEBOUNCE_MS = 1_000;

interface ActiveRow {
    summary: RoomSummaryIpc;
    hostDisplayName: string;
}

function hostDisplayNameOf(summary: RoomSummaryIpc): string {
    const host = summary.participants.find((p) => p.user_id === summary.host_user_id);
    return host?.display_name ?? "(unknown host)";
}

function roleOf(summary: RoomSummaryIpc, localUserId: string): RecentRoomRole {
    return summary.host_user_id === localUserId ? "host" : "guest";
}

function formatRelative(ms: number, now: number): string {
    const delta = now - ms;
    if (delta < 0) return "just now";
    const sec = Math.floor(delta / 1000);
    if (sec < 60) return `${sec}s ago`;
    const min = Math.floor(sec / 60);
    if (min < 60) return `${min}m ago`;
    const hr = Math.floor(min / 60);
    if (hr < 24) return `${hr}h ago`;
    const day = Math.floor(hr / 24);
    return `${day}d ago`;
}

export function RoomsIndexPage(): JSX.Element {
    const summary = useRoomStore((s) => s.summary);
    const [recents, setRecents] = useState<RecentRoomEntry[]>([]);
    const [hydrated, setHydrated] = useState(false);
    const [now, setNow] = useState<number>(() => Date.now());

    useEffect(() => {
        const id = window.setInterval(() => setNow(Date.now()), RECENTS_REFRESH_MS);
        return () => window.clearInterval(id);
    }, []);

    useEffect(() => {
        let cancelled = false;
        const unlistens: Array<() => void> = [];
        let lastSeenNonNull: RoomSummaryIpc | null = null;
        let localUserId: string | null = null;
        let pendingUpsert: ReturnType<typeof setTimeout> | null = null;

        async function refreshRecents(): Promise<void> {
            try {
                const rows = await listRecentRooms();
                if (!cancelled) setRecents(rows);
            } catch (err) {
                if (!cancelled) {
                    const detail = err instanceof Error ? err.message : String(err);
                    console.error("RoomsIndexPage: listRecentRooms failed", detail);
                }
            }
        }

        async function resolveLocalUserId(): Promise<string | null> {
            if (localUserId !== null) return localUserId;
            try {
                const id = await commands.identityGet(IDENTITY_PLACEHOLDER_NAME);
                if (!cancelled) localUserId = id.user_id;
                return localUserId;
            } catch (err) {
                if (!cancelled) {
                    const detail = err instanceof Error ? err.message : String(err);
                    console.error(
                        "RoomsIndexPage: identityGet failed; skipping recents upserts",
                        detail,
                    );
                }
                return null;
            }
        }

        async function performUpsert(entry: RecentRoomEntry): Promise<void> {
            try {
                await upsertRecentRoom(entry);
                if (!cancelled) await refreshRecents();
            } catch (err) {
                if (!cancelled) {
                    const detail = err instanceof Error ? err.message : String(err);
                    console.error("RoomsIndexPage: upsertRecentRoom failed", detail);
                }
            }
        }

        function scheduleUpsert(s: RoomSummaryIpc, ended: boolean): void {
            if (pendingUpsert !== null) clearTimeout(pendingUpsert);
            pendingUpsert = setTimeout(() => {
                pendingUpsert = null;
                if (cancelled) return;
                void (async () => {
                    const uid = await resolveLocalUserId();
                    if (cancelled || uid === null) return;
                    const entry: RecentRoomEntry = {
                        room_id: s.id,
                        code: s.code,
                        title: s.title,
                        host_user_id: s.host_user_id,
                        host_display_name: hostDisplayNameOf(s),
                        role: roleOf(s, uid),
                        last_seen_ms: Date.now(),
                        last_ended_ms: ended ? Date.now() : null,
                        created_ms: s.created_ms,
                    };
                    await performUpsert(entry);
                })();
            }, UPSERT_DEBOUNCE_MS);
        }

        (async () => {
            try {
                await refreshRecents();
                await resolveLocalUserId();

                if (summary !== null) {
                    lastSeenNonNull = summary;
                    scheduleUpsert(summary, false);
                }

                const u1 = await onRoomStateChange((next) => {
                    if (cancelled) return;
                    if (next === null) {
                        if (lastSeenNonNull !== null) {
                            const ended = lastSeenNonNull;
                            lastSeenNonNull = null;
                            scheduleUpsert(ended, true);
                        }
                    } else {
                        lastSeenNonNull = next;
                        scheduleUpsert(next, false);
                    }
                });
                if (cancelled) {
                    u1();
                    return;
                }
                unlistens.push(u1);
            } finally {
                if (!cancelled) setHydrated(true);
            }
        })().catch((err: unknown) => {
            if (!cancelled) {
                const detail = err instanceof Error ? err.message : String(err);
                console.error("RoomsIndexPage: init failed", detail);
                setHydrated(true);
            }
        });

        return () => {
            cancelled = true;
            if (pendingUpsert !== null) {
                clearTimeout(pendingUpsert);
                pendingUpsert = null;
            }
            while (unlistens.length > 0) {
                const u = unlistens.pop();
                if (u) u();
            }
        };
    }, [summary]);

    const activeRow: ActiveRow | null = summary !== null
        ? { summary, hostDisplayName: hostDisplayNameOf(summary) }
        : null;

    const hasContent = activeRow !== null || recents.length > 0;

    return (
        <div className="page-shell__content-inner">
            {hasContent && (
                <ul className="rooms-index__list">
                    <li>
                        <Link to="/rooms/new">Create room</Link>
                    </li>
                    <li>
                        <Link to="/rooms/join">Join room</Link>
                    </li>
                    <li>
                        <Link to="/library">Back to library</Link>
                    </li>
                </ul>
            )}

            {!hydrated && <p className="rooms-index__empty">Loading rooms...</p>}

            {hydrated && (
                <>
                    <section className="rooms-index__section" aria-label="Active room">
                        <h2 className="rooms-index__heading">Active</h2>
                        {activeRow === null ? (
                            <p className="rooms-index__empty">No active room.</p>
                        ) : (
                            <ul className="rooms-index__list">
                                <li className="rooms-index__tile">
                                    <div className="rooms-index__tile-title">
                                        {activeRow.summary.title}
                                    </div>
                                    <div className="rooms-index__tile-meta">
                                        <code className="rooms-index__tile-code">
                                            {activeRow.summary.code}
                                        </code>
                                        <span className="rooms-index__tile-host">
                                            Host: {activeRow.hostDisplayName}
                                        </span>
                                    </div>
                                    <Link
                                        to={`/rooms/${activeRow.summary.id}`}
                                        className="rooms-index__tile-link"
                                    >
                                        Go to room
                                    </Link>
                                </li>
                            </ul>
                        )}
                    </section>

                    <section className="rooms-index__section" aria-label="Recent rooms">
                        <h2 className="rooms-index__heading">Recent</h2>
                        {recents.length === 0 ? (
                            <p className="rooms-index__empty">No recent rooms yet.</p>
                        ) : (
                            <ul className="rooms-index__list">
                                {recents.map((r) => {
                                    const ended = r.last_ended_ms !== null;
                                    const tileClass = ended
                                        ? "rooms-index__tile rooms-index__tile--ended"
                                        : "rooms-index__tile";
                                    return (
                                        <li key={r.room_id} className={tileClass}>
                                            <div className="rooms-index__tile-title">
                                                {r.title}
                                            </div>
                                            <div className="rooms-index__tile-meta">
                                                <code className="rooms-index__tile-code">
                                                    {r.code}
                                                </code>
                                                <span className="rooms-index__tile-host">
                                                    Host: {r.host_display_name}
                                                </span>
                                                <span className="rooms-index__tile-badge">
                                                    {r.role}
                                                </span>
                                            </div>
                                            <div className="rooms-index__tile-sub">
                                                Seen {formatRelative(r.last_seen_ms, now)}
                                                {ended &&
                                                    ` · ended ${formatRelative(
                                                        r.last_ended_ms as number,
                                                        now,
                                                    )}`}
                                            </div>
                                        </li>
                                    );
                                })}
                            </ul>
                        )}
                    </section>

                    {!hasContent && (
                        <ul className="rooms-index__list">
                            <li>
                                <Link to="/rooms/new">Create room</Link>
                            </li>
                            <li>
                                <Link to="/rooms/join">Join room</Link>
                            </li>
                            <li>
                                <Link to="/library">Back to library</Link>
                            </li>
                        </ul>
                    )}
                </>
            )}
        </div>
    );
}
