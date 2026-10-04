// P5-T04: laser trail management hook.
//
// Responsibilities:
//
// 1. Maintain a trail of the last 20 positions per user
//    (keyed by user_id). Each position has a timestamp
//    used to compute opacity for the fading effect.
// 2. Expose `addPosition(userId, x, y)` to append a
//    new position to the trail.
// 3. Expose `removeTrail(userId)` to start the 200 ms
//    fade-out of a user's trail.
// 4. Expose `clearAll()` to immediately clear all trails.
// 5. Compute per-user opacity based on time since last
//    position or since `removeTrail` was called.
// 6. A `requestAnimationFrame` loop recomputes active
//    opacity values and invokes a callback for the renderer.
// 7. P5-T04: `addRemotePosition` feeds a REMOTE user's trail
//    (from `laser://move`). A remote trail that receives no
//    update for `REMOTE_IDLE_MS` starts the normal 200 ms
//    fade, so a sender that vanished without a LASER_OFF
//    (crash, dropped packet) does not leave a pointer stuck on
//    screen. Local trails (`addPosition`) never idle out.
//
// The hook does NOT own a canvas; it computes the trail
// data and hands it to `LaserPointer` which renders.

import { useCallback, useRef, useEffect } from "react";
import { laserColor } from "../utils/laserColor";
import { REMOTE_LASER_IDLE_MS } from "../laser/laserTransport";

/** Maximum number of trail positions kept per user. */
const MAX_TRAIL_LENGTH = 20;

/** Fade-out duration in ms when a trail is removed. */
const FADE_OUT_MS = 200;

/** P5-T04: a remote trail with no update for this long starts
 *  fading (ARCHITECTURE section 25.5: 3 s auto-release; more
 *  than twice the sender's 1000 ms keepalive). */
export const REMOTE_IDLE_MS = REMOTE_LASER_IDLE_MS;

/** A single position in a user's laser trail. */
export interface TrailPoint {
    x: number;
    y: number;
    tsMs: number;
}

/** Per-user laser trail state. */
export interface UserTrail {
    userId: string;
    points: TrailPoint[];
    color: string;
    opacity: number;
    fadingOut: boolean;
    fadeOutStartMs: number | null;
    /** P5-T04: fed by `addRemotePosition` (idles out). */
    remote: boolean;
    /** P5-T04: when the trail last received a position. */
    lastUpdateMs: number;
}

/** The full laser state passed to the renderer. */
export interface LaserState {
    trails: UserTrail[];
    nowMs: number;
}

/** Hook return value. */
export interface UseLaserTrailHandle {
    addPosition: (userId: string, x: number, y: number, color?: string) => void;
    /** P5-T04: like `addPosition`, for a remote user's trail,
     *  which fades after `REMOTE_IDLE_MS` without updates. */
    addRemotePosition: (userId: string, x: number, y: number, color?: string) => void;
    removeTrail: (userId: string) => void;
    clearAll: () => void;
    getState: () => LaserState;
}

/**
 * Manage laser trails for all participants.
 *
 * `onUpdate` is called with the current `LaserState` on
 * every `requestAnimationFrame` tick so the renderer can
 * draw without React re-renders.
 */
export function useLaserTrail(
    onUpdate: (state: LaserState) => void,
): UseLaserTrailHandle {
    /** Trails keyed by userId. */
    const trailsRef = useRef<Map<string, UserTrail>>(new Map());

    /** Most recent render timestamp for rAF throttle. */
    const lastRenderRef = useRef<number>(0);

    /** Cancel flag for the rAF loop. */
    const cancelledRef = useRef<boolean>(false);

    /** Add a position to a user's trail. Creates the trail
     *  if it doesn't exist. Trims to MAX_TRAIL_LENGTH.
     *  The color is assigned on first position and used for
     *  the lifetime of the trail. If no color is provided
     *  for a new trail, defaults to the remote-user palette
     *  color for that userId. */
    const append = useCallback(
        (userId: string, x: number, y: number, color: string | undefined, remote: boolean) => {
            const now = Date.now();
            let trail = trailsRef.current.get(userId);
            if (!trail) {
                const trailColor =
                    color ?? laserColor(userId, false);
                trail = {
                    userId,
                    points: [],
                    color: trailColor,
                    opacity: 1,
                    fadingOut: false,
                    fadeOutStartMs: null,
                    remote,
                    lastUpdateMs: now,
                };
                trailsRef.current.set(userId, trail);
            }
            trail.remote = remote;
            trail.lastUpdateMs = now;
            trail.points.push({ x, y, tsMs: now });
            if (trail.points.length > MAX_TRAIL_LENGTH) {
                trail.points.shift();
            }
            trail.fadingOut = false;
            trail.fadeOutStartMs = null;
            trail.opacity = 1;
        },
        [],
    );

    const addPosition = useCallback(
        (userId: string, x: number, y: number, color?: string) => {
            append(userId, x, y, color, false);
        },
        [append],
    );

    const addRemotePosition = useCallback(
        (userId: string, x: number, y: number, color?: string) => {
            append(userId, x, y, color, true);
        },
        [append],
    );

    /** P5-T04: start the fade of every remote trail that has had
     *  no update for REMOTE_IDLE_MS. The fade is timed from the
     *  moment the trail went idle, so a late rAF tick does not
     *  stretch it. */
    const expireIdle = useCallback((now: number) => {
        for (const trail of trailsRef.current.values()) {
            if (!trail.remote || trail.fadingOut) continue;
            const idleAt = trail.lastUpdateMs + REMOTE_IDLE_MS;
            if (now >= idleAt) {
                trail.fadingOut = true;
                trail.fadeOutStartMs = idleAt;
            }
        }
    }, []);

    /** Start fading a user's trail over FADE_OUT_MS. */
    const removeTrail = useCallback((userId: string) => {
        const trail = trailsRef.current.get(userId);
        if (trail && !trail.fadingOut) {
            trail.fadingOut = true;
            trail.fadeOutStartMs = Date.now();
        }
    }, []);

    /** Immediately clear all trails. */
    const clearAll = useCallback(() => {
        trailsRef.current.clear();
    }, []);

    /** Get the current laser state snapshot. */
    const getState = useCallback((): LaserState => {
        const now = Date.now();
        expireIdle(now);
        const trails: UserTrail[] = [];
        for (const trail of trailsRef.current.values()) {
            let opacity = trail.opacity;
            if (trail.fadingOut && trail.fadeOutStartMs !== null) {
                const elapsed = now - trail.fadeOutStartMs;
                if (elapsed >= FADE_OUT_MS) {
                    // Fully faded: remove from map on next tick.
                    opacity = 0;
                } else {
                    opacity = 1 - elapsed / FADE_OUT_MS;
                }
            }
            trails.push({
                ...trail,
                opacity,
                points: [...trail.points],
            });
        }
        return { trails, nowMs: now };
    }, [expireIdle]);

    /** rAF render loop. Throttled to ~60fps. */
    useEffect(() => {
        cancelledRef.current = false;

        const loop = (timestamp: number): void => {
            if (cancelledRef.current) return;

            // Throttle to ~60fps.
            if (timestamp - lastRenderRef.current >= 16) {
                lastRenderRef.current = timestamp;
                const state = getState();

                // Remove fully faded trails.
                for (const [userId, trail] of trailsRef.current.entries()) {
                    if (trail.fadingOut && trail.fadeOutStartMs !== null) {
                        const elapsed = Date.now() - trail.fadeOutStartMs;
                        if (elapsed >= FADE_OUT_MS) {
                            trailsRef.current.delete(userId);
                        }
                    }
                }

                onUpdate(state);
            }

            requestAnimationFrame(loop);
        };

        const rafId = requestAnimationFrame(loop);
        return () => {
            cancelledRef.current = true;
            cancelAnimationFrame(rafId);
        };
    }, [getState, onUpdate]);

    return { addPosition, addRemotePosition, removeTrail, clearAll, getState };
}
