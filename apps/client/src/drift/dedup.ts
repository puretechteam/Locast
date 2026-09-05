/**
 * P4-T07: per-sender monotonic_seq deduplication.
 *
 * Goal (roadmap P4-T07): "client tracks `last_applied_seq[user_id]`;
 * duplicates are dropped, gaps buffered up to 5 s, then applied."
 *
 * Architecture reference: `docs/ARCHITECTURE.md` section 13.2
 * ("Dedup rules") and 13.5 ("Disconnect behaviour"). The
 * server (`apps/server/src/rooms/state.rs::last_acked_seq`) is
 * already authoritative for accepting commands: a server-side
 * duplicate or out-of-order `monotonic_seq` is rejected with
 * `ROOM_ERROR(StaleCommand)`. The CLIENT side dedup exists for
 * a different reason: the server rebroadcasts an accepted
 * command back to every authorized viewer, and a viewer that
 * reconnects, observes a transient network reorder, or simply
 * receives a slightly-delayed echo must not let an old event
 * overwrite a newer one in the local store. The dedup is also
 * the safety net that prevents the host-echo path in `Player.tsx`
 * from accidentally re-applying a command that the host just
 * applied locally.
 *
 * Conventions:
 *  - `senderId` is the originator's `user_id` (the
 *    `PlaybackStateEvent.sender_id` field). The map is keyed
 *    per sender so the host's own `monotonic_seq` is
 *    independent of every viewer's.
 *  - `monotonic_seq` is a `u64` strictly increasing per
 *    sender. The server's `last_acked_seq` enforces the same
 *    invariant at accept time (architecture §13.2).
 *  - "Apply" means: write the event into
 *    `usePlaybackStore.lastApplied` (the existing per-room
 *    flow). "Drop" means: do not touch the store's playback
 *    state. "Buffer" means: park the event in the dedup
 *    module's per-sender pending slot until either the gap
 *    is filled OR the `BUFFER_TIMEOUT_MS` (5 s) elapses.
 *
 * Out-of-order rules (architecture §13.2):
 *  - `seq <= last_applied_seq[sender]` -> DROP (duplicate).
 *  - `seq == last_applied_seq[sender] + 1` -> APPLY, then
 *    attempt to drain any buffered event for the same sender
 *    whose `seq` now matches the new `last_applied_seq + 1`.
 *  - `seq > last_applied_seq[sender] + 1` -> BUFFER (gap).
 *    If the gap is not filled within `BUFFER_TIMEOUT_MS`, the
 *    held event is APPLIED with the gap acknowledged
 *    (`applyHeld`); the local position may briefly desync and
 *    the user can press the existing manual-sync button
 *    (P4-T05) to recover.
 *
 * Late window (architecture §13.2): a buffered event whose
 * `seq` is older than the current `last_applied_seq[sender]`
 * after the gap has been filled by a newer event is DROPPED
 * unconditionally. This is the "out-of-order SEEK" case in
 * the P4-T07 acceptance test.
 *
 * The math is split into two pure functions:
 *  - `evaluateDedup` returns the decision for ONE event
 *    against the current dedup state, and a new dedup state
 *    with any bookkeeping applied (advance `last_applied_seq`,
 *    install a buffered slot, expire an old slot, etc.).
 *  - `tickDedup` returns the events that should now be
 *    applied because their buffer timeout expired. The caller
 *    (the playback store) is responsible for re-evaluating
 *    each expired event with the updated state.
 *
 * The store integration lives in
 * `apps/client/src/stores/usePlaybackStore.ts`; this module
 * contains the pure math only.
 */

/** Roadmap P4-T07 acceptance: "gaps buffered up to 5 s". Matches
 *  architecture §13.2 "buffer for 5 s" and §13.5 disconnect
 *  behavior. */
export const BUFFER_TIMEOUT_MS = 5_000;

/** A buffered event awaiting its missing predecessors. */
export interface BufferedEvent<T> {
    event: T;
    /** Local wall-clock arrival time (browser `Date.now()`),
     *  used to time out the buffer. */
    parkedAtMs: number;
    /** The `monotonic_seq` of the parked event, denormalized
     *  for fast lookup when the gap is filled. */
    seq: number;
}

/** Per-sender dedup bookkeeping. One entry per sender_id that
 *  has produced at least one event on this client. */
export interface SenderDedupState<T> {
    lastAppliedSeq: number;
    /** A single parked event whose `seq > lastAppliedSeq + 1`.
     *  The buffer is a single slot because architecture §13.2
     *  only specifies buffer behavior for one gap at a time;
     *  any second gap while a first is parked is itself
     *  buffered by replacing the older parked event (newer
     *  wins; the older is dropped as "out-of-order SEEK"). */
    pending: BufferedEvent<T> | null;
}

export interface DedupState<T> {
    /** Keyed by `sender_id`. */
    bySender: Map<string, SenderDedupState<T>>;
}

/** Build a fresh dedup state. */
export function initialDedupState<T>(): DedupState<T> {
    return { bySender: new Map() };
}

/** What `evaluateDedup` decides to do with the incoming event. */
export type DedupDecision<T> =
    /** The event's `seq` is strictly greater than any other
     *  event from the same sender we have accepted so far AND
     *  is either the immediate successor (`last + 1`) or the
     *  gap-timed-out held event. The caller should pass the
     *  event to `usePlaybackStore.acceptEvent`. */
    | { kind: "apply"; event: T; /** Seq of the event being
     applied, denormalized so the caller can update its own
     per-sender seq map if it keeps one. */
        seq: number; senderId: string; }
    /** The event's seq is `<= last` (duplicate) or its seq is
     *  older than the current buffered event after a newer
     *  buffer has replaced it (out-of-order late SEEK). The
     *  caller must NOT touch the store. */
    | { kind: "drop"; reason: "duplicate" | "out_of_order"; seq: number; senderId: string; }
    /** The event's seq is greater than `last + 1`. The caller
     *  must NOT touch the store; the dedup module retains the
     *  event in its own state and will re-evaluate it when a
     *  successor arrives OR when `tickDedup` reports the
     *  timeout. */
    | { kind: "buffer"; seq: number; senderId: string; }
    /** A previously buffered event's 5 s timer has expired and
     *  the gap was never filled. The caller should apply the
     *  event (the room may have skipped an event; manual-sync
     *  is the user-driven recovery path). The dedup state has
     *  already been advanced so a future legitimate duplicate
     *  of this seq will still drop. */
    | { kind: "applyHeld"; event: T; seq: number; senderId: string; }
    /** The event's seq is `last + 1` AND a previously buffered
     *  event is now ready to be drained (because the current
     *  event filled the gap). The caller should apply BOTH
     *  events in order. */
    | { kind: "apply"; event: T; seq: number; senderId: string; drain?: { event: T; seq: number; senderId: string } };

/** Evaluate a single inbound playback event against the
 *  current dedup state and produce the next state plus the
 *  decision. The function is pure (does not mutate `state`;
 *  the caller uses the returned state for subsequent calls).
 *
 *  `nowMs` is the local wall clock (`Date.now()`) at the
 *  moment the caller enqueued the event. It is used to
 *  decide whether a previously parked event has outlived its
 *  5 s grace window and should now be force-applied. */
export function evaluateDedup<T extends { sender_id: string; monotonic_seq: number }>(
    state: DedupState<T>,
    event: T,
    nowMs: number,
): { decision: DedupDecision<T>; next: DedupState<T> } {
    // Build a fresh top-level state (Map is mutable; we
    // intentionally copy for purity).
    const next: DedupState<T> = {
        bySender: new Map(state.bySender),
    };
    const sender = next.bySender.get(event.sender_id);
    const seq = event.monotonic_seq;

    // First event from this sender: bootstrap the per-sender
    // state. The server always assigns `monotonic_seq = 1`
    // as the first command from any sender (architecture
    // §13.1; `last_acked_seq` defaults to 0 and the next
    // accepted value is 1). Anything else from a brand-new
    // sender is treated as a duplicate (we have not seen any
    // predecessors; we cannot apply it).
    if (sender === undefined) {
        if (seq <= 0) {
            return {
                decision: { kind: "drop", reason: "duplicate", seq, senderId: event.sender_id },
                next,
            };
        }
        if (seq === 1) {
            next.bySender.set(event.sender_id, {
                lastAppliedSeq: 1,
                pending: null,
            });
            return {
                decision: { kind: "apply", event, seq: 1, senderId: event.sender_id },
                next,
            };
        }
        // seq > 1 from a sender we have never seen: buffer
        // it as a gap. The 5 s timer will eventually force
        // apply it. This matches the architecture's "request
        // replay of missing range" intent -- the client
        // does not currently request a replay, so the gap
        // will simply time out.
        next.bySender.set(event.sender_id, {
            lastAppliedSeq: 0,
            pending: { event, parkedAtMs: nowMs, seq },
        });
        return {
            decision: { kind: "buffer", seq, senderId: event.sender_id },
            next,
        };
    }

    // Existing sender. Duplicate detection (strict `<=`).
    if (seq <= sender.lastAppliedSeq) {
        return {
            decision: { kind: "drop", reason: "duplicate", seq, senderId: event.sender_id },
            next,
        };
    }

    // Immediate successor (`last + 1`). Apply, then check
    // whether the previously parked event (if any) is now
    // the immediate successor and can be drained.
    if (seq === sender.lastAppliedSeq + 1) {
        const updated: SenderDedupState<T> = {
            lastAppliedSeq: seq,
            pending: null,
        };
        // If a buffered event with seq == seq + 1 exists, it
        // can drain (the gap is exactly 1).
        const drained = sender.pending;
        if (drained !== null && drained.seq === seq + 1) {
            updated.lastAppliedSeq = seq + 1;
            updated.pending = null;
            next.bySender.set(event.sender_id, updated);
            return {
                decision: {
                    kind: "apply",
                    event,
                    seq,
                    senderId: event.sender_id,
                    drain: { event: drained.event, seq: seq + 1, senderId: event.sender_id },
                },
                next,
            };
        }
        next.bySender.set(event.sender_id, updated);
        return {
            decision: { kind: "apply", event, seq, senderId: event.sender_id },
            next,
        };
    }

    // seq > last + 1. Gap.
    //
    // Two sub-cases:
    //   (a) the existing pending slot is empty -> park the
    //       new event.
    //   (b) the existing pending slot holds a NEWER seq
    //       (a tighter gap than the one we just received)
    //       -> the inbound event is an out-of-order late
    //       SEEK, DROP it. This matches the §13.2 late
    //       SEEK rule ("Late SEEK after more recent SEEK
    //       -> dropped") and the P4-T07 acceptance test's
    //       third case.
    //   (c) the existing pending slot holds an OLDER seq
    //       (a wider gap than the one we just received)
    //       -> replace the buffered event with the tighter
    //       gap; the older buffered event is dropped (it
    //       can never be drained because a stricter gap
    //       precedes it).
    const existingPending = sender.pending;
    if (existingPending !== null) {
        if (existingPending.seq >= seq) {
            // Case (b) + (c): a newer-or-equal pending slot
            // exists. Drop the inbound event as
            // out-of-order.
            return {
                decision: { kind: "drop", reason: "out_of_order", seq, senderId: event.sender_id },
                next,
            };
        }
        // existingPending.seq < seq: replace with the
        // tighter gap.
        next.bySender.set(event.sender_id, {
            lastAppliedSeq: sender.lastAppliedSeq,
            pending: { event, parkedAtMs: nowMs, seq },
        });
        return {
            decision: { kind: "buffer", seq, senderId: event.sender_id },
            next,
        };
    }

    // No existing pending: park the gap.
    next.bySender.set(event.sender_id, {
        lastAppliedSeq: sender.lastAppliedSeq,
        pending: { event, parkedAtMs: nowMs, seq },
    });
    return {
        decision: { kind: "buffer", seq, senderId: event.sender_id },
        next,
    };
}

/** Drain any buffered events whose `BUFFER_TIMEOUT_MS` has
 *  elapsed. The returned list is the events the caller must
 *  apply, in arrival order. Each event's seq is now the new
 *  `lastAppliedSeq[sender]` (the gap is acknowledged; the
 *  client may be briefly out of sync and the user can press
 *  the manual-sync button to recover).
 *
 *  The dedup state is advanced for each expired event so a
 *  future duplicate of the same seq still drops. */
export function tickDedup<T extends { sender_id: string; monotonic_seq: number }>(
    state: DedupState<T>,
    nowMs: number,
): { expired: Array<{ event: T; seq: number; senderId: string }>; next: DedupState<T> } {
    const next: DedupState<T> = {
        bySender: new Map(state.bySender),
    };
    const expired: Array<{ event: T; seq: number; senderId: string }> = [];
    for (const [senderId, sender] of next.bySender) {
        if (sender.pending === null) continue;
        if (nowMs - sender.pending.parkedAtMs < BUFFER_TIMEOUT_MS) continue;
        // Gap expired. Force-apply the held event.
        const held = sender.pending;
        next.bySender.set(senderId, {
            lastAppliedSeq: held.seq,
            pending: null,
        });
        expired.push({ event: held.event, seq: held.seq, senderId });
    }
    return { expired, next };
}

/** Forget all per-sender dedup state for a fresh room. The
 *  caller (the playback store's `setRoomId`) invokes this on
 *  room change so a sender's seq counter from the previous
 *  room does not collide with the new room's per-sender
 *  sequence. */
export function clearDedupState<T>(): DedupState<T> {
    return { bySender: new Map() };
}