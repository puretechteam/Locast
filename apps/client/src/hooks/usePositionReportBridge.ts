import { useEffect } from "react";
import { onPositionReport } from "../services/playback";
import { useViewerPositionStore } from "../stores/useViewerPositionStore";
import { useRoomStore } from "../stores/useRoomStore";

/**
 * P4-T03: bridge the `position://report` Tauri event
 * into the per-viewer position store. Mounted once by
 * `RoomPage` alongside `usePlaybackEventBridge` so the
 * 1 Hz inbound stream is scoped to the room page's
 * mount / unmount (and therefore to the user's current
 * room lifecycle).
 *
 * The hook returns `null` (no JSX).
 */
export function usePositionReportBridge(): null {
    const currentRoomId = useRoomStore((s) => s.summary?.id ?? null);
    const setViewerPosition = useViewerPositionStore((s) => s.setViewerPosition);
    const clear = useViewerPositionStore((s) => s.clear);

    useEffect(() => {
        // When the user changes rooms, wipe the per-viewer
        // position map so the new room's reports do not
        // mix with stale rows from the previous room.
        clear();
    }, [currentRoomId, clear]);

    useEffect(() => {
        let cancelled = false;
        const unsubs: Array<() => void> = [];
        (async () => {
            try {
                const u = await onPositionReport((e) => {
                    if (cancelled) return;
                    setViewerPosition(e);
                });
                if (cancelled) {
                    u();
                    return;
                }
                unsubs.push(u);
            } catch (err) {
                // Surface the failure in dev; in prod the
                // viewer-position UI simply stays empty
                // until a re-mount retries.
                console.warn("usePositionReportBridge: listen failed", err);
            }
        })();
        return () => {
            cancelled = true;
            for (const u of unsubs) {
                try {
                    u();
                } catch {
                    /* swallow */
                }
            }
        };
    }, [setViewerPosition]);

    return null;
}