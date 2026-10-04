// P5-T04: laser pointer canvas overlay.
//
// Renders fading polyline trails for all participants'
// laser pointers. The component:
//
// - Is a separate `<canvas>` positioned above the
//   drawing canvas (z-index stacking in room.css).
// - Is purely visual (`pointer-events: none`).
// - Receives trail data from `useLaserTrail` via the
//   `LaserState` passed to `onLaserUpdate`.
// - Renders a fading polyline trail (last 20 positions)
//   and a colored dot at the head for each participant.
// - The fade-out animation is computed in `useLaserTrail`
//   and applied here as canvas globalAlpha.
//
// Laser activation is controlled by the `laserActive` prop
// and the `localUserId`. When `laserActive` is true, the
// component tracks window-level pointer moves and emits
// positions for the local user in red.
//
// P5-T04 (network transport, `useLaserTransport`): the same
// normalized positions also go to the room as LASER_MOVE (at
// most 60 Hz, only with `cap.LASER`), and pointerup /
// deactivation sends LASER_OFF. Remote `laser://move` /
// `laser://off` events for `roomId` draw / fade the senders'
// trails; a remote trail with no update for 3 s fades on its
// own. The local trail renders whether or not anything is sent.

import { useEffect, useRef, useCallback } from "react";
import type { RefObject } from "react";
import { useLaserTrail, type LaserState } from "../hooks/useLaserTrail";
import { useLaserTransport } from "../hooks/useLaserTransport";
import { useCapabilityStore, CAP } from "../stores/useCapabilityStore";
import { hexToRgba, laserColor } from "../utils/laserColor";

/**
 * Props
 * -----
 * `videoRef` is the same ref the parent (`Player`)
 * passes to the `<video>` element. The hook attaches a
 * `ResizeObserver` to it so the canvas backing store
 * follows the video's intrinsic resolution.
 *
 * `localUserId` is the local user's id; used to stamp
 * the local laser trail.
 *
 * `laserActive` controls whether the local user's laser
 * is tracking the pointer.
 *
 * `roomId` (P5-T04) is the room the laser is shared with;
 * `null` / omitted keeps the laser local-only.
 */
export interface LaserPointerProps {
    videoRef: RefObject<HTMLVideoElement | null>;
    localUserId: string;
    laserActive: boolean;
    roomId?: string | null;
}

/** Render a single user's laser trail.
 *  The color is provided by the trail data. */
function renderTrail(
    ctx: CanvasRenderingContext2D,
    points: { x: number; y: number }[],
    color: string,
    opacity: number,
    intrinsicSize: { width: number; height: number },
    cssWidth: number,
    _cssHeight: number,
): void {
    if (points.length === 0) return;
    if (opacity <= 0) return;

    // Width scale: stroke width in backing-store pixels.
    const cssW = cssWidth > 0 ? cssWidth : intrinsicSize.width;
    const widthScale = intrinsicSize.width / cssW;

    ctx.globalAlpha = opacity;

    // Draw the trail as a fading polyline.
    // The trail uses a gradient opacity from head to tail.
    for (let i = 1; i < points.length; i++) {
        const t = i / points.length; // 0 at tail, 1 at head
        const pointOpacity = t * opacity;

        const p0 = points[i - 1]!;
        const p1 = points[i]!;

        const x0 = p0.x * intrinsicSize.width;
        const y0 = p0.y * intrinsicSize.height;
        const x1 = p1.x * intrinsicSize.width;
        const y1 = p1.y * intrinsicSize.height;

        ctx.beginPath();
        ctx.strokeStyle = hexToRgba(color, pointOpacity);
        ctx.lineWidth = (4 * t) * widthScale; // Thinner at tail
        ctx.lineCap = "round";
        ctx.lineJoin = "round";
        ctx.moveTo(x0, y0);
        ctx.lineTo(x1, y1);
        ctx.stroke();
    }

    // Draw the laser dot at the head.
    if (points.length > 0) {
        const head = points[points.length - 1]!;
        const hx = head.x * intrinsicSize.width;
        const hy = head.y * intrinsicSize.height;
        const radius = 6 * widthScale;

        ctx.beginPath();
        ctx.fillStyle = hexToRgba(color, opacity);
        ctx.arc(hx, hy, radius, 0, Math.PI * 2);
        ctx.fill();

        // Inner white dot for visibility.
        ctx.beginPath();
        ctx.fillStyle = `rgba(255, 255, 255, ${opacity * 0.8})`;
        ctx.arc(hx, hy, radius * 0.4, 0, Math.PI * 2);
        ctx.fill();
    }

    ctx.globalAlpha = 1;
}

export function LaserPointer({
    videoRef,
    localUserId,
    laserActive,
    roomId = null,
}: LaserPointerProps): React.ReactNode {
    const canvasRef = useRef<HTMLCanvasElement | null>(null);

    /** Current intrinsic size. */
    const intrinsicSizeRef = useRef<{ width: number; height: number } | null>(null);

    /** Laser state from the trail hook. */
    const laserStateRef = useRef<LaserState | null>(null);

    /** Test mode only: per-frame render cost, so the Playwright
     *  suite can check the 16 ms frame budget. */
    const renderStatsRef = useRef({ count: 0, maxMs: 0, lastMs: 0, maxTrails: 0 });

    /** Render the laser trails onto the canvas. */
    const render = useCallback(() => {
        const canvas = canvasRef.current;
        const state = laserStateRef.current;
        const size = intrinsicSizeRef.current;
        if (!canvas || !state || !size) return;
        const t0 = import.meta.env.MODE === "test" ? performance.now() : 0;

        const ctx = canvas.getContext("2d");
        if (!ctx) return;

        const rect = canvas.getBoundingClientRect();
        const cssWidth = rect.width || size.width;
        const cssHeight = rect.height || size.height;

        // Clear the canvas.
        ctx.clearRect(0, 0, size.width, size.height);

        // Render each user's trail.
        for (const trail of state.trails) {
            renderTrail(
                ctx,
                trail.points,
                trail.color,
                trail.opacity,
                size,
                cssWidth,
                cssHeight,
            );
        }

        if (import.meta.env.MODE === "test") {
            const ms = performance.now() - t0;
            const st = renderStatsRef.current;
            st.count += 1;
            st.lastMs = ms;
            st.maxMs = Math.max(st.maxMs, ms);
            st.maxTrails = Math.max(st.maxTrails, state.trails.length);
        }
    }, []);

    /** Update handler passed to useLaserTrail. */
    const onLaserUpdate = useCallback((state: LaserState) => {
        laserStateRef.current = state;
        render();
    }, [render]);

    const { addPosition, addRemotePosition, removeTrail, clearAll, getState } =
        useLaserTrail(onLaserUpdate);

    // P5-T04: share the local laser with the room and draw the
    // other participants' lasers. UI gating only: the server
    // enforces cap.LASER (and drops denials silently). The host
    // holds every cap, LASER included, in `you_cap_set`.
    const youCapSet = useCapabilityStore((s) => s.youCapSet);
    const canSend =
        roomId !== null && youCapSet !== null && (youCapSet & CAP.LASER) !== 0;
    const onRemoteMove = useCallback(
        (senderId: string, x: number, y: number) => {
            addRemotePosition(senderId, x, y, laserColor(senderId, false));
        },
        [addRemotePosition],
    );
    const transport = useLaserTransport({
        roomId,
        localUserId,
        canSend,
        onRemoteMove,
        onRemoteOff: removeTrail,
        onReset: clearAll,
    });

    /** Track video intrinsic dimensions. */
    useEffect(() => {
        const video = videoRef.current;
        if (!video) return;

        const sync = (): void => {
            const w = video.videoWidth;
            const h = video.videoHeight;
            if (w > 0 && h > 0) {
                intrinsicSizeRef.current = { width: w, height: h };
                const canvas = canvasRef.current;
                if (canvas) {
                    canvas.width = w;
                    canvas.height = h;
                }
            }
        };

        sync();

        const ro = new ResizeObserver(sync);
        ro.observe(video);
        video.addEventListener("loadedmetadata", sync);

        return () => {
            ro.disconnect();
            video.removeEventListener("loadedmetadata", sync);
        };
    }, [videoRef]);

    /** Track window-level pointer moves when laser is active. */
    useEffect(() => {
        if (!laserActive) return;

        const localColor = laserColor(localUserId, true);

        const onPointerMove = (e: PointerEvent): void => {
            const canvas = canvasRef.current;
            if (!canvas) return;
            const rect = canvas.getBoundingClientRect();
            if (rect.width === 0 || rect.height === 0) return;
            const x = Math.max(0, Math.min(1, (e.clientX - rect.left) / rect.width));
            const y = Math.max(0, Math.min(1, (e.clientY - rect.top) / rect.height));
            addPosition(localUserId, x, y, localColor);
            transport.move(x, y);
        };

        const onPointerUp = (): void => {
            removeTrail(localUserId);
            transport.off();
        };

        window.addEventListener("pointermove", onPointerMove);
        window.addEventListener("pointerup", onPointerUp);
        return () => {
            window.removeEventListener("pointermove", onPointerMove);
            window.removeEventListener("pointerup", onPointerUp);
            removeTrail(localUserId);
            transport.off();
        };
    }, [laserActive, localUserId, addPosition, removeTrail, transport]);

    /** Expose the laser control functions on window in test mode. */
    useEffect(() => {
        if (import.meta.env.MODE !== "test") return;
        const w = window as unknown as {
            __locastLaser?: {
                addPosition: (userId: string, x: number, y: number, color?: string) => void;
                addRemotePosition: (userId: string, x: number, y: number, color?: string) => void;
                removeTrail: (userId: string) => void;
                clearAll: () => void;
                getState: () => ReturnType<typeof getState>;
                localUserId: string;
                getRenderStats: () => {
                    count: number;
                    maxMs: number;
                    lastMs: number;
                    maxTrails: number;
                };
                resetRenderStats: () => void;
                setIntrinsicSize: (width: number, height: number) => void;
            };
        };
        w.__locastLaser = {
            addPosition,
            addRemotePosition,
            removeTrail,
            clearAll,
            getState,
            localUserId,
            getRenderStats: () => ({ ...renderStatsRef.current }),
            resetRenderStats: () => {
                renderStatsRef.current = { count: 0, maxMs: 0, lastMs: 0, maxTrails: 0 };
            },
            // The harness video has no decodable media, so it never
            // reports an intrinsic size; this gives the canvas one
            // so frames are actually drawn (and timed).
            setIntrinsicSize: (width, height) => {
                intrinsicSizeRef.current = { width, height };
                const canvas = canvasRef.current;
                if (canvas) {
                    canvas.width = width;
                    canvas.height = height;
                }
            },
        };
        return () => {
            if (w.__locastLaser) delete w.__locastLaser;
        };
    }, [addPosition, addRemotePosition, removeTrail, clearAll, getState, localUserId]);

    return (
        <canvas
            ref={canvasRef}
            className="laser-pointer-layer"
            data-testid="locast-laser-pointer"
            aria-hidden="true"
        />
    );
}
