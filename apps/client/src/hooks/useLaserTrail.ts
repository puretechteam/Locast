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
//
// The hook does NOT own a canvas; it computes the trail
// data and hands it to `LaserPointer` which renders.

import { useCallback, useRef, useEffect } from "react";

/** Maximum number of trail positions kept per user. */
const MAX_TRAIL_LENGTH = 20;

/** Fade-out duration in ms when a trail is removed. */
const FADE_OUT_MS = 200;

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
    opacity: number;
    fadingOut: boolean;
    fadeOutStartMs: number | null;
}

/** The full laser state passed to the renderer. */
export interface LaserState {
    trails: UserTrail[];
    nowMs: number;
}

/** Hook return value. */
export interface UseLaserTrailHandle {
    addPosition: (userId: string, x: number, y: number) => void;
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
     *  if it doesn't exist. Trims to MAX_TRAIL_LENGTH. */
    const addPosition = useCallback((userId: string, x: number, y: number) => {
        const now = Date.now();
        let trail = trailsRef.current.get(userId);
        if (!trail) {
            trail = {
                userId,
                points: [],
                opacity: 1,
                fadingOut: false,
                fadeOutStartMs: null,
            };
            trailsRef.current.set(userId, trail);
        }
        trail.points.push({ x, y, tsMs: now });
        if (trail.points.length > MAX_TRAIL_LENGTH) {
            trail.points.shift();
        }
        trail.fadingOut = false;
        trail.fadeOutStartMs = null;
        trail.opacity = 1;
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
    }, []);

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

    return { addPosition, removeTrail, clearAll, getState };
}
