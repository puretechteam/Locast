// P5-T03: what Ctrl+Z / the Undo button does.
//
// Pure (no React, DOM or Tauri) so the Node smoke test
// `undoPolicy.smoke.ts` runs the production decision.
//
// - The user may undo their own strokes in the room (UNDO_OWN, or
//   UNDO_ANY which includes it): "undo" targets the user's most
//   recent FINISHED local stroke that has no undo already on its
//   way, and the stroke is removed from every canvas (the user's
//   own included) when the server's DRAW_UNDO event arrives.
// - Otherwise Ctrl+Z keeps its pre-P5-T03 meaning: drop the newest
//   local stroke from this client's own canvas only. Nothing is
//   sent, so other participants keep seeing it. (The server
//   refuses an undo from a user without the capability, so this
//   local-only fallback is the only thing such a user can do.)

/** The part of a local stroke the policy reads. */
export interface UndoCandidate {
    id: string;
    /** `0` while the stroke is still being drawn. */
    endedAt: number;
}

export type UndoPlan =
    /** Send a DRAW_UNDO for this stroke; remove it on the server's event. */
    | { kind: "remote"; strokeId: string }
    /** No capability: remove the newest local stroke locally only. */
    | { kind: "local" }
    /** Nothing to undo. */
    | { kind: "none" };

/**
 * The newest finished stroke that is neither in `pending` (undo sent,
 * answer awaited) nor in `gaveUp` (undo sent, no answer came in time),
 * or `null`. Strokes are in draw order, oldest first.
 */
export function pickUndoTarget(
    strokes: readonly UndoCandidate[],
    pending: ReadonlySet<string>,
    gaveUp: ReadonlySet<string> = new Set(),
): string | null {
    for (let i = strokes.length - 1; i >= 0; i--) {
        const s = strokes[i];
        if (
            s !== undefined &&
            s.endedAt !== 0 &&
            !pending.has(s.id) &&
            !gaveUp.has(s.id)
        ) {
            return s.id;
        }
    }
    return null;
}

/** How long a sent undo waits for the server's event before it is
 *  given up on. */
export const UNDO_PENDING_MS = 5000;

type TimerId = ReturnType<typeof setTimeout>;

/**
 * Bookkeeping for undo requests that are on their way to the server.
 *
 * The server answers a refused or no-op undo with silence, so an id
 * cannot stay "pending" forever (that would hide the stroke from
 * Ctrl+Z for good) and must not simply become targetable again (the
 * same newest stroke would be retried indefinitely). When the timer
 * fires the id moves from `pending` to `gaveUp`, and `pickUndoTarget`
 * skips both sets, so the next Ctrl+Z targets the next-older stroke.
 */
export class UndoTracker {
    readonly pending = new Set<string>();
    readonly gaveUp = new Set<string>();
    private readonly timers = new Map<string, TimerId>();

    private readonly setTimer: (cb: () => void, ms: number) => TimerId;
    private readonly clearTimer: (id: TimerId) => void;
    private readonly timeoutMs: number;

    constructor(
        setTimer: (cb: () => void, ms: number) => TimerId = setTimeout,
        clearTimer: (id: TimerId) => void = clearTimeout,
        timeoutMs: number = UNDO_PENDING_MS,
    ) {
        this.setTimer = setTimer;
        this.clearTimer = clearTimer;
        this.timeoutMs = timeoutMs;
    }

    /** An undo for `strokeId` was just sent. */
    markSent(strokeId: string): void {
        this.dropTimer(strokeId);
        this.pending.add(strokeId);
        this.timers.set(
            strokeId,
            this.setTimer(() => {
                this.timers.delete(strokeId);
                if (this.pending.delete(strokeId)) this.gaveUp.add(strokeId);
            }, this.timeoutMs),
        );
    }

    /** The send itself failed (reported elsewhere): allow a retry. */
    sendFailed(strokeId: string): void {
        this.dropTimer(strokeId);
        this.pending.delete(strokeId);
    }

    /** The server's undo event removed `strokeId`. */
    confirmed(strokeId: string): void {
        this.dropTimer(strokeId);
        this.pending.delete(strokeId);
        this.gaveUp.delete(strokeId);
    }

    /** Clear-all or a room change: forget everything. */
    reset(): void {
        for (const id of this.timers.values()) this.clearTimer(id);
        this.timers.clear();
        this.pending.clear();
        this.gaveUp.clear();
    }

    private dropTimer(strokeId: string): void {
        const t = this.timers.get(strokeId);
        if (t !== undefined) {
            this.clearTimer(t);
            this.timers.delete(strokeId);
        }
    }
}

/** Decide what an undo request does. */
export function planUndo(opts: {
    /** The local user holds UNDO_OWN or UNDO_ANY (UI convenience only;
     *  the server decides). */
    canUndoOwn: boolean;
    strokes: readonly UndoCandidate[];
    pending: ReadonlySet<string>;
    /** Undos that were sent but never answered (see `UndoTracker`). */
    gaveUp?: ReadonlySet<string>;
}): UndoPlan {
    if (!opts.canUndoOwn) {
        return opts.strokes.length > 0 ? { kind: "local" } : { kind: "none" };
    }
    const target = pickUndoTarget(opts.strokes, opts.pending, opts.gaveUp);
    return target === null ? { kind: "none" } : { kind: "remote", strokeId: target };
}
