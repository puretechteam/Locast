import { create } from "zustand";
import type { PlaybackStateEvent } from "../services/playback";

export type { PlaybackStateEvent };

export type PlaybackKind = "play" | "pause" | "seek";

/**
 * The most recent accepted playback event that has NOT
 * yet been applied to the local <video> element. P4-T02
 * buffers at most one event at a time: a newer event
 * always replaces a parked one (the parked event's
 * `server_seq` is also recorded so a stale parked event
 * cannot override a newer one if the media-readiness
 * flip happens out of order).
 */
export interface PendingPlayback {
    event: PlaybackStateEvent;
    parkedAtMs: number;
}

interface PlaybackStoreState {
    /** Current room id (mirrored from `useRoomStore` via
     * the event bridge). Used to scope `server_seq` per
     * room — switching rooms resets the counter so a
     * new room's events with `server_seq = 1` are
     * accepted. */
    roomId: string | null;

    /** The most recent accepted playback event that
     * has been applied to the local <video> element.
     * Null on first mount, before any accepted event
     * arrives. */
    lastApplied: PlaybackStateEvent | null;

    /** The event the <video> element should consume
     * when it becomes ready. The Player component
     * drains this on `canplay` / `loadedmetadata` and
     * clears it. A newer event always replaces a
     * parked one. */
    pending: PendingPlayback | null;

    /** Last `server_seq` the store has applied for the
     * current room. Used to drop stale or duplicate
     * server rebroadcasts. Reset to 0 when `roomId`
     * changes. */
    lastAppliedServerSeq: number;

    /** True once the local <video> element has fired
     * `canplay` (or `loadedmetadata`) and is ready to
     * seek / play. The Player component drives this
     * via the event bridge. */
    mediaReady: boolean;

    /** The current media's resolved `locast://` URL,
     * if the host has published a manifest and the
     * viewer has called `mediaResolveUrl`. Null when
     * no media is loaded yet. */
    mediaSrc: string | null;

    /** Local-user suppression flag: when true, the
     * Player's <video> element's `play` / `pause` /
     * `seeked` DOM event handlers are NOT allowed to
     * bubble into new `PLAYBACK_CMD` envelopes. This is
     * a defense-in-depth flag set by the Player when it
     * applies a remote-accepted event; the host's UI
     * controls (the Play/Pause/Seek buttons) bypass it
     * because they call `playbackSend` directly, not
     * via the <video> element. */
    suppressLocalEcho: boolean;

    /** P4-T05: the host's next per-sender monotonic
     * sequence. Shared across all host-authoritative
     * `PLAYBACK_CMD` emit paths (the existing
     * `PlaybackControls` Play/Pause/Seek buttons AND
     * the new Sync button's host branch) so two
     * per-component counters cannot race and produce a
     * duplicate or out-of-order `monotonic_seq` that
     * the server would reject (architecture §13.1,
     * P4-T01). Initialized to 1 at store creation
     * because the server expects the FIRST per-sender
     * command to carry `monotonic_seq = 1`. The counter
     * is reset to 1 on `setRoomId(null)` and on
     * `clear()` because host migration restarts the
     * per-sender sequence for the new host. Viewers
     * (i.e. non-hosts) do not need this counter; the
     * Sync button's viewer branch is a local-only DOM
     * action and never reads or writes this field. */
    hostNextSeq: number;
    /** P4-T05: read the host's next monotonic sequence
     * AND advance the counter by 1 in a single,
     * race-free step. Returns the `seq` to use for the
     * outgoing PLAYBACK_CMD. The counter advances
     * unconditionally on `bumpHostSeq`; the caller is
     * responsible for retrying on the SAME `seq` if
     * the server rejects (the existing `PlaybackControls`
     * pattern — see comments there). */
    bumpHostSeq: () => number;

    setMediaReady: (ready: boolean) => void;
    setRoomId: (roomId: string | null) => void;
    setMediaSrc: (src: string | null) => void;
    /** Attempt to accept a server event. Returns
     * `true` if the event was accepted (and either
     * applied or parked), `false` if the event was
     * dropped as stale, duplicate, or out of scope. */
    acceptEvent: (event: PlaybackStateEvent) => boolean;
    setSuppressLocalEcho: (suppress: boolean) => void;
    /** Called by the Player after the <video> element
     * has finished applying the event so the store
     * records `lastApplied` and clears `pending`. */
    markApplied: (serverSeq: number) => void;
    clear: () => void;
}

/**
 * P4-T02 client-side playback state. The server is
 * authoritative: this store only mirrors the latest
 * accepted server event and applies `server_seq`
 * ordering + media-readiness buffering.
 *
 * The store deliberately does NOT track host-vs-viewer
 * identity beyond `roomId`; the Player component decides
 * whether to ignore the host's own echo.
 */
export const usePlaybackStore = create<PlaybackStoreState>((set, get) => ({
    roomId: null,
    lastApplied: null,
    pending: null,
    lastAppliedServerSeq: 0,
    mediaReady: false,
    mediaSrc: null,
    suppressLocalEcho: false,
    hostNextSeq: 1,

    setMediaReady: (ready) => set({ mediaReady: ready }),
    setRoomId: (roomId) => {
        // Switching rooms resets the ordering counter
        // and the pending slot. The new room's events
        // will start at `server_seq = 1`. P4-T05 also
        // resets the host's monotonic sequence on room
        // change so a re-host (e.g. host migration
        // landing back on the same identity) does not
        // see a stale seq counter collide with the new
        // room's per-sender sequence.
        if (get().roomId !== roomId) {
            set({
                roomId,
                lastApplied: null,
                pending: null,
                lastAppliedServerSeq: 0,
                hostNextSeq: 1,
            });
        }
    },
    setMediaSrc: (src) => set({ mediaSrc: src }),
    setSuppressLocalEcho: (suppress) => set({ suppressLocalEcho: suppress }),

    acceptEvent: (event) => {
        const state = get();
        // Drop events from a different room. Defense in
        // depth: the Rust client already filters by
        // `state.lock().await.id == env.room_id` but
        // the React side has its own notion of the
        // current room via `useRoomStore`.
        if (state.roomId !== null && state.roomId !== event.room_id) {
            return false;
        }
        // Drop duplicate / stale events (server_seq <=
        // last applied). Strictly monotonic per the
        // server's contract (P4-T01).
        if (event.server_seq <= state.lastAppliedServerSeq) {
            return false;
        }
        if (state.mediaReady) {
            // The <video> element is ready; record this
            // as the new authoritative state. The Player
            // component will read `lastApplied` and
            // apply it to the DOM. We do NOT mutate
            // `lastAppliedServerSeq` here — that
            // happens in `markApplied` once the Player
            // has finished applying.
            set({ lastApplied: event });
            return true;
        }
        // Media is not ready yet. Park the event. A
        // newer event replaces a parked one. The
        // parked event's `server_seq` is recorded
        // implicitly via `event.server_seq`; when the
        // Player drains the pending slot, it will
        // accept whichever event is parked at drain
        // time, which is by construction the newest
        // accepted event.
        set({ pending: { event, parkedAtMs: Date.now() } });
        return true;
    },

    markApplied: (serverSeq) => {
        const state = get();
        // Only advance if the applied seq matches the
        // current `lastApplied.server_seq` (or, if the
        // media was not ready, the parked event's
        // server_seq). If a newer event arrived while
        // the Player was applying the previous one, the
        // newer one is in `lastApplied` and we should
        // NOT advance past it.
        const next = state.lastApplied;
        if (next && next.server_seq === serverSeq) {
            set({ lastAppliedServerSeq: serverSeq });
        }
    },

    clear: () =>
        set({
            roomId: null,
            lastApplied: null,
            pending: null,
            lastAppliedServerSeq: 0,
            mediaReady: false,
            mediaSrc: null,
            suppressLocalEcho: false,
            hostNextSeq: 1,
        }),

    /**
     * P4-T05: atomically read the host's next monotonic
     * sequence and advance the counter by 1. The counter
     * lives in the playback store (not in
     * `PlaybackControls` and not in the Sync button's
     * closure) so multiple host-authoritative emit
     * paths share one source of truth. See the
     * `hostNextSeq` field comment for the reset rules
     * on room change.
     */
    bumpHostSeq: () => {
        const next = get().hostNextSeq;
        set({ hostNextSeq: next + 1 });
        return next;
    },
}));
