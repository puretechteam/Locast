// P5-T04: laser pointer canvas overlay.
//
// Renders fading polyline trails for all participants'
// laser pointers. The component:
//
// - Is a separate `<canvas>` positioned above the
//   drawing canvas (z-index stacking in room.css).
// - Does NOT handle pointer events; it is purely
//   visual.
// - Receives trail data from `useLaserTrail` via the
//   `LaserState` passed to `onLaserUpdate`.
// - Renders a fading polyline trail (last 20 positions)
//   and a red dot at the head for each participant.
// - The fade-out animation is computed in `useLaserTrail`
//   and applied here as canvas globalAlpha.
//
// The component does NOT manage laser activation state;
// that is owned by the caller (e.g. a keyboard handler
// in a future toolbar task). This component simply renders
// whatever trails are provided.

import { useEffect, useRef, useCallback } from "react";
import type { RefObject } from "react";
import { useLaserTrail, type LaserState } from "../hooks/useLaserTrail";

/**
 * Props
 * -----
 * `videoRef` is the same ref the parent (`Player`)
 * passes to the `<video>` element. The hook attaches a
 * `ResizeObserver` to it so the canvas backing store
 * follows the video's intrinsic resolution.
 */
export interface LaserPointerProps {
    videoRef: RefObject<HTMLVideoElement | null>;
}

/** Render a single user's laser trail. */
function renderTrail(
    ctx: CanvasRenderingContext2D,
    points: { x: number; y: number }[],
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
        ctx.strokeStyle = `rgba(255, 50, 50, ${pointOpacity})`;
        ctx.lineWidth = (4 * t) * widthScale; // Thinner at tail
        ctx.lineCap = "round";
        ctx.lineJoin = "round";
        ctx.moveTo(x0, y0);
        ctx.lineTo(x1, y1);
        ctx.stroke();
    }

    // Draw the red dot at the head.
    if (points.length > 0) {
        const head = points[points.length - 1]!;
        const hx = head.x * intrinsicSize.width;
        const hy = head.y * intrinsicSize.height;
        const radius = 6 * widthScale;

        ctx.beginPath();
        ctx.fillStyle = `rgba(255, 0, 0, ${opacity})`;
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
}: LaserPointerProps): React.ReactNode {
    const canvasRef = useRef<HTMLCanvasElement | null>(null);

    /** Current intrinsic size. */
    const intrinsicSizeRef = useRef<{ width: number; height: number } | null>(null);

    /** Laser state from the trail hook. */
    const laserStateRef = useRef<LaserState | null>(null);

    /** Render the laser trails onto the canvas. */
    const render = useCallback(() => {
        const canvas = canvasRef.current;
        const state = laserStateRef.current;
        const size = intrinsicSizeRef.current;
        if (!canvas || !state || !size) return;

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
                trail.opacity,
                size,
                cssWidth,
                cssHeight,
            );
        }
    }, []);

    /** Update handler passed to useLaserTrail. */
    const onLaserUpdate = useCallback((state: LaserState) => {
        laserStateRef.current = state;
        render();
    }, [render]);

    const { addPosition, removeTrail, clearAll, getState } = useLaserTrail(onLaserUpdate);

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

    /** Expose the laser control functions on window in test mode. */
    useEffect(() => {
        if (import.meta.env.MODE !== "test") return;
        const w = window as unknown as {
            __locastLaser?: {
                addPosition: (userId: string, x: number, y: number) => void;
                removeTrail: (userId: string) => void;
                clearAll: () => void;
                getState: () => ReturnType<typeof getState>;
            };
        };
        w.__locastLaser = { addPosition, removeTrail, clearAll, getState };
        return () => {
            if (w.__locastLaser) delete w.__locastLaser;
        };
    }, [addPosition, removeTrail, clearAll, getState]);

    return (
        <canvas
            ref={canvasRef}
            className="laser-pointer-layer"
            data-testid="locast-laser-pointer"
            aria-hidden="true"
        />
    );
}
