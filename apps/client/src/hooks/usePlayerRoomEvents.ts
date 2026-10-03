// apps/client/src/hooks/usePlayerRoomEvents.ts
//
// P1-T10: the Player subscribes to the `room://event` channel as a deliberate
// no-op. P4 (synchronized playback) will give this handler a body; registering
// it now fixes the lifecycle: one listener while a player is mounted, removed
// on unmount, never duplicated by a remount or React StrictMode's double
// effect. Receiving an event must not touch playback.

import { useEffect } from "react";
import { onRoomEvent } from "../services/room";

let active = 0;
let received = 0;

/** Test seam (same pattern as the other `__locast*` hooks): only in `--mode test`. */
if (import.meta.env.MODE === "test") {
    (window as unknown as { __locastPlayerRoomEvents?: unknown }).__locastPlayerRoomEvents = {
        active: () => active,
        received: () => received,
    };
}

export function usePlayerRoomEvents(): void {
    useEffect(() => {
        let cancelled = false;
        let unlisten: (() => void) | null = null;

        onRoomEvent(() => {
            if (cancelled) return;
            // Intentionally empty: no playback state changes in P1-T10.
            received += 1;
        })
            .then((off) => {
                if (cancelled) {
                    // Unmounted before the subscription finished: drop it now.
                    off();
                    return;
                }
                unlisten = off;
                active += 1;
            })
            .catch((err: unknown) => {
                console.warn("Player: could not listen to room://event", err);
            });

        return () => {
            cancelled = true;
            if (unlisten !== null) {
                unlisten();
                unlisten = null;
                active -= 1;
            }
        };
    }, []);
}
