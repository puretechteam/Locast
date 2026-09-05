// P5-T01: the drawing canvas overlay component.
//
// Renders a transparent `<canvas>` positioned over the
// `<video>` element. The component:
//
// - Owns the canvas DOM element via a ref the parent
//   hook (`useDrawingCanvas`) drives.
// - Inherits its pointer-input behavior from the
//   canvas's CSS `pointer-events` rule (see
//   `apps/client/src/styles/room.css`). P5-T01 ships
//   `pointer-events: none` so the native
//   `<video controls>` overlay remains usable; a future
//   task will toggle to `auto` when a drawing mode is
//   active.
// - Reads `data-testid` selectors that the Playwright
//   suite uses to verify presence, intrinsic-size, and
//   resize behavior end.
// P5-T03: also subscribes to remote drawing events
// (DRAW_BEGIN/POINT/END rebroadcast) and renders them
// on the same canvas.

import { useRef } from "react";
import type { RefObject } from "react";
import { useDrawingCanvas } from "../hooks/useDrawingCanvas";
import { useDrawingEventBridge, useDrawingRoomSync } from "../hooks/useDrawingEventBridge";
import { useDrawingStore } from "../stores/useDrawingStore";

/**
 * Props
 * -----
 * `videoRef` is the same ref the parent (`Player`)
 * passes to the `<video>` element. The hook attaches a
 * `ResizeObserver` to it so the canvas backing store
 * follows the video's intrinsic resolution.
 *
 * `userId` is the local user's id; stamped into every
 * stroke so future renderer code can distinguish local
 * vs remote strokes (§15.2).
 */
export interface DrawingLayerProps {
    videoRef: RefObject<HTMLVideoElement | null>;
    userId?: string | null;
    roomId?: string | null;
}

export function DrawingLayer({
    videoRef,
    userId,
    roomId,
}: DrawingLayerProps): React.ReactNode {
    const canvasRef = useRef<HTMLCanvasElement | null>(null);

    useDrawingRoomSync(roomId ?? null);
    useDrawingEventBridge({});

    const remoteStrokes = useDrawingStore((s) => s.getAllStrokes());

    useDrawingCanvas(canvasRef, videoRef, userId, remoteStrokes);

    return (
        <canvas
            ref={canvasRef}
            className="drawing-layer"
            data-testid="locast-drawing-layer"
            aria-hidden="true"
        />
    );
}