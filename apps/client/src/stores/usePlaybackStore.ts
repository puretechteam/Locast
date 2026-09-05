import { create } from "zustand";
import type { PlaybackStateEvent } from "../services/playback";
import {
    clearDedupState,
    evaluateDedup,
    initialDedupState,
    tickDedup,
    type DedupState,
} from "../drift/dedup";

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

    /** P4-T07: per-sender `monotonic_seq` dedup state.
     *  Lives inside the playback store so it is
     *  automatically reset on room change (`setRoomId`)
     *  and on `clear()`. The store's `acceptEvent` is
     *  the single chokepoint that consults this state;
     *  callers in the React layer (the bridge hook) just
     *  hand the inbound `playback://state` event to
     *  `acceptEvent`. The dedup state is independent of
     *  the existing `server_seq` counter, which remains
     *  authoritative for room-level ordering; the per-
     *  sender dedup is the P4-T07 safety net for
     *  per-sender gaps, duplicates, and out-of-order
     *  late arrivals (architecture §13.2). */
    dedupState: DedupState<PlaybackStateEvent>;

    setMediaReady: (ready: boolean) => void;
    setRoomId: (roomId: string | null) => void;
    setMediaSrc: (src: string | null) => void;
    /** Attempt to accept a server event. Returns
     * `true` if the event was accepted (and either
     * applied or parked), `false` if the event was
     * dropped as stale, duplicate, or out of scope. */
    acceptEvent: (event: PlaybackStateEvent) => boolean;
    /** P4-T07: read the current per-sender dedup state
     * for the test seam. Production code should NOT call
     * this -- it is a debug-only surface used by the
     * Vite harness to assert the dedup behavior end to
     * end. */
    getDedupState: () => Readonly<DedupState<PlaybackStateEvent>>;
    /** P4-T07: force-evaluate any buffered events whose
     * 5 s grace window has elapsed. The playback store
     * does not own a setInterval for this; the test
     * harness drives it explicitly so the dedup math is
     * exercised deterministically without real time
     * advances. Returns the number of events that were
     * force-applied as a result of the tick (0 when no
     * parked event has expired). */
    tickDedup: (nowMs: number) => number;
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
    dedupState: initialDedupState(),

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
                // P4-T07: a new room has a fresh per-sender
                // monotonic sequence namespace. Reusing the
                // old dedup map would let a duplicate seq
                // from a previous room drop a legitimate
                // event from the new room (or vice versa).
                dedupState: clearDedupState(),
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
        // server's contract (P4-T01). The per-sender
        // `monotonic_seq` check is performed below; this
        // server_seq check is the room-level ordering
        // invariant and is kept because the server
        // already enforces it.
        if (event.server_seq <= state.lastAppliedServerSeq) {
            return false;
        }

        // P4-T07: per-sender `monotonic_seq` dedup.
        // The pure math lives in `drift/dedup.ts`; here
        // we apply the decision.
        const dedup = evaluateDedup(state.dedupState, event, Date.now());
        // Persist the post-dedup state immediately so a
        // re-entrant call (a future event arriving in the
        // same tick) sees the advanced `last_applied_seq`.
        set({ dedupState: dedup.next });

        if (dedup.decision.kind === "drop") {
            return false;
        }
        if (dedup.decision.kind === "buffer") {
            // Parked in the dedup module's own pending
            // slot; the store's authoritative playback
            // state is unchanged. A future successor will
            // drain (or `tickDedup` will force-apply after
            // 5 s).
            return true;
        }
        if (dedup.decision.kind === "applyHeld") {
            // The dedup math decided the buffered event
            // has expired (the test seam drove a clock
            // advance past `BUFFER_TIMEOUT_MS`). Treat it
            // as an ordinary apply.
        }

        // apply / applyHeld: write the event into the
        // store. The Player component will read it from
        // `lastApplied` and apply it to the DOM.
        //
        // If the apply decision has a `drain` payload,
        // the parked event has a HIGHER seq than the
        // current event (it was buffered because its
        // predecessor -- the current event -- was
        // missing). The dedup state has already advanced
        // to the drained event's seq. We apply the
        // drained event to the store (the current event
        // is logically superseded); the current event is
        // recorded nowhere in the playback store because
        // the drain semantics are "the gap is filled, we
        // catch up to the parked event".
        const drained =
            dedup.decision.kind === "apply" &&
            "drain" in dedup.decision &&
            dedup.decision.drain !== undefined
                ? dedup.decision.drain.event
                : event;
        if (state.mediaReady) {
            set({ lastApplied: drained });
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
        set({ pending: { event: drained, parkedAtMs: Date.now() } });
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
            // P4-T07: a fresh client should not carry
            // per-sender dedup state across room
            // lifetimes; clearing the map is the only
            // safe way to ensure a new room's first
            // `monotonic_seq = 1` event is not mistaken
            // for a duplicate of a previous room's
            // last event.
            dedupState: clearDedupState(),
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

    /** P4-T07: read-only view of the per-sender dedup
     * state. Exposed for the Vite test seam so the
     * Playwright suite can assert the dedup math end
     * to end without rebuilding the React tree. */
    getDedupState: () => get().dedupState,

    /** P4-T07: force a 5 s tick on the dedup state.
     * The production React layer does not own a
     * `setInterval` for this; the store's `acceptEvent`
     * is the only entry that exercises `evaluateDedup`,
     * and `tickDedup` is the drain path the test seam
     * drives explicitly so the dedup math is exercised
     * deterministically without real time advances.
     *
     * Returns the number of held events that were
     * force-applied because their parked timer
     * exceeded `BUFFER_TIMEOUT_MS`. */
    tickDedup: (nowMs) => {
        const state = get();
        const { expired, next } = tickDedup(state.dedupState, nowMs);
        if (expired.length === 0) {
            // Still persist any clock bookkeeping the
            // tick may have done (none today, but the
            // future may add per-sender timestamps).
            set({ dedupState: next });
            return 0;
        }
        // The first expired event is the one to apply
        // (the buffer is a single slot per sender). If
        // multiple senders had parked events expire in
        // the same tick, apply each in deterministic
        // sender-id order so the test assertions can
        // predict the result.
        expired.sort((a, b) => a.senderId.localeCompare(b.senderId));
        const applied = expired[0];
        if (applied === undefined) {
            set({ dedupState: next });
            return 0;
        }
        set({
            dedupState: next,
            lastApplied: state.mediaReady ? applied.event : state.lastApplied,
            pending: state.mediaReady
                ? state.pending
                : { event: applied.event, parkedAtMs: nowMs },
        });
        return expired.length;
    },
}));
    
