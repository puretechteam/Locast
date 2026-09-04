import { create } from "zustand";
import type { PositionReportEvent } from "../services/playback";

/**
 * P4-T03: per-viewer position telemetry. Stores the most
 * recent POSITION_REPORT received from each remote
 * participant, keyed by `user_id`. The store is purely a
 * cache of remote telemetry -- it does NOT affect the
 * server-authoritative playback state (which lives in
 * `usePlaybackStore`) and does NOT drive the local `<video>`
 * element.
 *
 * Multi-viewer distinction: the host's UI reads
 * `getViewerPosition(user_id)` to show each viewer's most
 * recent reported position. A simple "latest per sender"
 * map is sufficient for P4-T03; drift smoothing, median
 * computation, and TTL-based "stale viewer" cleanup are
 * out of scope here and belong to P4-T04 / P6-T04.
 *
 * Disconnect/leave cleanup is handled at the React layer
 * via the existing `room://event` (ParticipantLeft) /
 * `room://state` (RoomClosed) listeners: those events
 * clear the map or remove the per-user entries. The
 * server's stale-participant cleanup (driven by 5s
 * PRESENCE, see P4-T08) drops the participant from the
 * room entirely; the React layer mirrors that by
 * discarding stale entries when the room summary changes.
 *
 * React-side "stale" heuristic: a viewer who has not
 * reported in the last 10 s is marked stale in the UI
 * but NOT removed from the store (the host's UI can show
 * "last seen 12s ago" without dropping the row). Removal
 * happens on ParticipantLeft / RoomClosed.
 */
export interface ViewerPosition {
    userId: string;
    mediaPositionMs: number;
    playing: boolean;
    clientTsMs: number;
    /** Local wall-clock arrival time (browser Date.now()). */
    receivedAtMs: number;
}

interface ViewerPositionStoreState {
    byUserId: Record<string, ViewerPosition>;
    setViewerPosition: (e: PositionReportEvent) => void;
    removeViewer: (userId: string) => void;
    clear: () => void;
}

export const useViewerPositionStore = create<ViewerPositionStoreState>((set, get) => ({
    byUserId: {},
    setViewerPosition: (e) => {
        // Drop stale / cross-room events: the Rust side
        // already scope-checks against the current room,
        // but defense in depth.
        if (e.room_id === "") {
            return;
        }
        // Ignore reports from the LOCAL user. The WS
        // forwarder's originator filter already prevents
        // the server from echoing our own report back to
        // us, but a future bug there must not produce a
        // self-feedback row in the store. The caller
        // (Player.tsx) does NOT compare against the local
        // user_id because the host should also see the
        // host's *own* reported position via the same UI
        // path that surfaces viewer positions; if a host
        // ever gets a self-report (e.g. via a misrouted
        // rebroadcast in a future P), the
        // `displayPositionMs` selector in RoomPage will
        // resolve to the server-authoritative
        // `lastApplied.media_position_ms` (the more
        // accurate source). See RoomPage.tsx for the
        // resolution rule.
        const next = { ...get().byUserId };
        next[e.sender_id] = {
            userId: e.sender_id,
            mediaPositionMs: e.media_position_ms,
            playing: e.playing,
            clientTsMs: e.client_ts_ms,
            receivedAtMs: Date.now(),
        };
        set({ byUserId: next });
    },
    removeViewer: (userId) => {
        const cur = get().byUserId;
        if (!(userId in cur)) return;
        const next = { ...cur };
        delete next[userId];
        set({ byUserId: next });
    },
    clear: () => set({ byUserId: {} }),
}));