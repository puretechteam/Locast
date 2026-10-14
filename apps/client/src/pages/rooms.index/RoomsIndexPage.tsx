import { useEffect, useState } from "react";
import { listRecentRooms, upsertRecentRoom } from "../../services/recentRooms";
import { onRoomStateChange } from "../../services/room";
import { useRoomStore } from "../../stores/useRoomStore";
import { commands } from "../../services/ipc";
import type { RecentRoomEntry, RecentRoomRole } from "../../services/recentRooms";
import type { RoomSummaryIpc } from "../../services/room";
import { EmptyState, Icon, LinkButton, PageHeader, Spinner, StatusPill } from "../../components/ui";
import "../../styles/rooms.css";

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
    // `you_user_id` is the server-assigned id that `host_user_id` is in. The
    // identity id passed as `localUserId` is a different id, so comparing it
    // with `host_user_id` recorded every host as a guest.
    const me = summary.you_user_id ?? localUserId;
    return summary.host_user_id === me ? "host" : "guest";
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

    // The room you are in is shown under Active, not again under Recent.
    const recentOthers = recents.filter((r) => r.room_id !== summary?.id);

    const activeRow: ActiveRow | null = summary !== null
        ? { summary, hostDisplayName: hostDisplayNameOf(summary) }
        : null;

    return (
        <div className="page rooms-index">
            <PageHeader
                title="Rooms"
                description="Watch together with friends. Create a room, or join one with a code or an invite link."
                back={{ to: "/library", label: "Back to library" }}
                actions={
                    <>
                        <LinkButton to="/rooms/join" icon="join">
                            Join a room
                        </LinkButton>
                        <LinkButton to="/rooms/new" variant="primary" icon="plus">
                            Create a room
                        </LinkButton>
                    </>
                }
            />

            {!hydrated && (
                <p className="loading" role="status" aria-busy="true">
                    <Spinner />
                    Loading rooms...
                </p>
            )}

            {hydrated && (
                <>
                    <section className="rooms-index__section" aria-labelledby="rooms-active-heading">
                        <h2 id="rooms-active-heading" className="section-title">
                            Active room
                        </h2>
                        {activeRow === null ? (
                            <EmptyState icon="users" plain compact title="You are not in a room">
                                Create a room to host a watch party, or join a friend&apos;s room with its code.
                            </EmptyState>
                        ) : (
                            <ActiveRoomRow row={activeRow} />
                        )}
                    </section>

                    <section className="rooms-index__section" aria-labelledby="rooms-recent-heading">
                        <h2 id="rooms-recent-heading" className="section-title">
                            Recent rooms
                        </h2>
                        {recentOthers.length === 0 ? (
                            <p className="rooms-index__none">
                                Rooms you host or join appear here, so you can find a code again.
                            </p>
                        ) : (
                            <ul className="rooms-index__list">
                                {recentOthers.map((r) => (
                                    <RecentRoomRow key={r.room_id} entry={r} now={now} />
                                ))}
                            </ul>
                        )}
                    </section>
                </>
            )}
        </div>
    );
}

function ActiveRoomRow({ row }: { row: ActiveRow }): JSX.Element {
    const { summary, hostDisplayName } = row;
    const present = summary.participants.filter((p) => p.status !== "Left").length;
    return (
        <div className="room-row room-row--active" data-testid="rooms-active">
            <div className="room-row__main">
                <h3 className="room-row__title">{summary.title}</h3>
                <p className="room-row__meta">
                    <code className="room-row__code">{summary.code}</code>
                    <span>Host: {hostDisplayName.trim() || "Unnamed"}</span>
                    <span>
                        {present} {present === 1 ? "person" : "people"}
                    </span>
                </p>
            </div>
            {summary.host_disconnected ? (
                <StatusPill tone="warning" dot pulse>
                    Host reconnecting
                </StatusPill>
            ) : (
                <StatusPill tone="success" dot>
                    Live
                </StatusPill>
            )}
            <LinkButton to={`/rooms/${summary.id}`} variant="primary" icon="play">
                Go to room
            </LinkButton>
        </div>
    );
}

function RecentRoomRow({ entry, now }: { entry: RecentRoomEntry; now: number }): JSX.Element {
    const ended = entry.last_ended_ms !== null;
    return (
        <li className={`room-row${ended ? " room-row--ended" : ""}`}>
            <div className="room-row__main">
                <h3 className="room-row__title">{entry.title}</h3>
                <p className="room-row__meta">
                    <code className="room-row__code">{entry.code}</code>
                    <span>Host: {entry.host_display_name.trim() || "Unnamed"}</span>
                    <span className="room-row__when">
                        <Icon name="clock" />
                        Seen {formatRelative(entry.last_seen_ms, now)}
                        {ended && ` · ended ${formatRelative(entry.last_ended_ms as number, now)}`}
                    </span>
                </p>
            </div>
            <StatusPill tone={entry.role === "host" ? "accent" : "neutral"}>
                {entry.role === "host" ? "Host" : "Guest"}
            </StatusPill>
            {ended ? (
                <StatusPill>Ended</StatusPill>
            ) : entry.role === "host" ? null : (
                <LinkButton to={`/rooms/join?code=${encodeURIComponent(entry.code)}`} size="sm" icon="join">
                    Join again
                </LinkButton>
            )}
        </li>
    );
}
