import { useEffect } from "react";
import { listenEvent } from "../services/_eventTransport";
import type { RoomSummaryIpc } from "../services/room";
import { resetRoomScopedStores } from "../stores/resetRoomScopedStores";

/**
 * Resets every room-scoped store when the room ends (`room://state` with
 * `null`: the host closed it, grace expired, or the user was removed).
 *
 * The room page does clear its own summary on that event, but only while it is
 * mounted. It is not mounted while a download is running (the blocking guard
 * hides it), nor while the user is on the Library or Rooms pages, and the other
 * room-scoped stores had no owner at all. A room that ended in those windows
 * left its summary, media, positions, capabilities and quality indicator behind,
 * and the next visit to a room page showed the dead room.
 *
 * Mounted at the app level next to `SharedMediaBridge`. Leaving on purpose does
 * not emit this event (`room_leave` just clears Rust's copy), so the leave paths
 * call `resetRoomScopedStores` themselves. Renders nothing.
 */
export function RoomEndBridge(): null {
    useEffect(() => {
        let cancelled = false;
        let unlisten: (() => void) | undefined;
        listenEvent<RoomSummaryIpc | null>("room://state", (summary) => {
            if (!cancelled && summary === null) resetRoomScopedStores();
        })
            .then((u) => {
                if (cancelled) u();
                else unlisten = u;
            })
            .catch((err: unknown) => {
                console.warn("RoomEndBridge: could not subscribe", err);
            });
        return () => {
            cancelled = true;
            unlisten?.();
        };
    }, []);
    return null;
}
