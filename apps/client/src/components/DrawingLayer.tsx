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

import { useRef } from "react";
import type { RefObject } from "react";
import { useDrawingCanvas } from "../hooks/useDrawingCanvas";

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
}

export function DrawingLayer({
    videoRef,
    userId,
}: DrawingLayerProps): React.ReactNode {
    // The canvas DOM node lives in this component's JSX;
    // the hook attaches a ResizeObserver + re-paint
    // effect to it via the ref. The hook returns the
    // imperative handles a future drawing toolbar will
    // use; P5-T01 mounts the canvas only.
    const canvasRef = useRef<HTMLCanvasElement | null>(null);
    useDrawingCanvas(canvasRef, videoRef, userId);

    return (
        <canvas
            ref={canvasRef}
            className="drawing-layer"
            data-testid="locast-drawing-layer"
            aria-hidden="true"
        />
    );
}