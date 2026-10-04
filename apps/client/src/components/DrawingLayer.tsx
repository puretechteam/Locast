// P5-T01: the drawing canvas overlay component.
//
// Renders a transparent `<canvas>` positioned over the
// `<video>` element. The component:
//
// - Owns the canvas DOM element via a ref the parent
//   hook (`useDrawingCanvas`) drives.
// - Pointer events (active in drawing mode) are fed through
//   `PointerStrokePipeline`, which drives both the local
//   stroke (hook) and the P5-T02 network send
//   (`DrawingService`: DRAW_BEGIN / DRAW_POINT / DRAW_END
//   via the `drawing_send` Tauri command).
// - Reads `data-testid` selectors that the Playwright
//   suite uses to verify presence, intrinsic-size, and
//   resize behavior.
// P5-T03: also subscribes to remote drawing events
// (DRAW_BEGIN/POINT/END rebroadcast) and renders them
// on the same canvas.
// P5-T04: laser pointer overlay integrated here.
// P5-T06: drawing toolbar and keyboard shortcuts.

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { RefObject } from "react";
import { useDrawingCanvas } from "../hooks/useDrawingCanvas";
import { useDrawingEventBridge, useDrawingRoomSync } from "../hooks/useDrawingEventBridge";
import { useDrawingStore } from "../stores/useDrawingStore";
import { useCapabilityStore, CAP } from "../stores/useCapabilityStore";
import { useKeyboardScope } from "../hooks/useKeyboardScope";
import type { DrawingTool, DrawingMode } from "../hooks/useKeyboardScope";
import type { StrokeTool } from "../drawing/types";
import { DrawingService } from "../services/drawing";
import { PointerStrokePipeline } from "../drawing/pointerPipeline";
import type { PointerSample } from "../drawing/pointerPipeline";
import { LaserPointer } from "./LaserPointer";
import { DrawingToolbar } from "./DrawingToolbar";

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

    const {
        beginStroke,
        appendPoint,
        endStroke,
        undo,
        setStrokeStyle,
    } = useDrawingCanvas(canvasRef, videoRef, userId, remoteStrokes);

    const [activeTool, setActiveTool] = useState<DrawingTool>("pen");
    const [strokeColor, setStrokeColor] = useState("#e6e6e6");
    const [strokeWidth, setStrokeWidth] = useState(3);

    const youCapSet = useCapabilityStore((s) => s.youCapSet);
    const canDraw = youCapSet !== null && (youCapSet & CAP.DRAW) !== 0;

    const keyboard = useKeyboardScope({
        onUndo: undo,
        canDraw,
    });

    // P6-T02 test seam: expose keyboard scope state so
    // the Playwright harness can verify toolbar visibility
    // gating without keyboard simulation.
    useEffect(() => {
        if (import.meta.env.MODE !== "test") return;
        const w = window as unknown as {
            __locastKeyboardScope?: {
                toolbarVisible: boolean;
                laserActive: boolean;
                drawingMode: DrawingMode;
                canvasMode: string;
                canDraw: boolean;
                toggleToolbar: () => void;
                setDrawingMode: (mode: DrawingMode) => void;
                setLaserActive: (active: boolean) => void;
            };
        };
        w.__locastKeyboardScope = {
            toolbarVisible: keyboard.toolbarVisible,
            laserActive: keyboard.laserActive,
            drawingMode: keyboard.drawingMode,
            canvasMode: keyboard.canvasMode,
            canDraw,
            toggleToolbar: keyboard.toggleToolbar,
            setDrawingMode: keyboard.setDrawingMode,
            setLaserActive: keyboard.setLaserActive,
        };
    }, [keyboard, canDraw]);

    const handleToolSelect = useCallback(
        (tool: DrawingTool) => {
            setActiveTool(tool);
            setStrokeStyle({ tool: tool as StrokeTool });
            keyboard.setDrawingMode(tool);
        },
        [keyboard, setStrokeStyle],
    );

    const handleColorChange = useCallback(
        (color: string) => {
            setStrokeColor(color);
            setStrokeStyle({ color });
        },
        [setStrokeStyle],
    );

    const handleStrokeWidthChange = useCallback(
        (width: number) => {
            setStrokeWidth(width);
            setStrokeStyle({ width });
        },
        [setStrokeStyle],
    );

    const handleToolbarClose = useCallback(() => {
        keyboard.setDrawingMode("none");
    }, [keyboard]);

    const isDrawing = keyboard.drawingMode !== "none";

    // P5-T02: the production send path. One DrawingService
    // per room (a new roomId gives a fresh instance, so no
    // stroke id or queued send leaks across rooms). The
    // pipeline couples the local hook (store + renderer)
    // with the service so the local stroke id is the wire
    // stroke id.
    const service = useMemo(() => new DrawingService(), [roomId]);
    const canDrawRef = useRef(canDraw);
    canDrawRef.current = canDraw;
    const pipeline = useMemo(
        () =>
            new PointerStrokePipeline(
                { beginStroke, appendPoint, endStroke },
                service,
                { canSend: () => canDrawRef.current },
            ),
        [beginStroke, appendPoint, endStroke, service],
    );

    // Never leave a stroke dangling: leaving the room /
    // unmounting the layer / switching rooms closes the
    // active stroke (flushes the last point, sends
    // DRAW_END).
    useEffect(() => {
        return () => {
            pipeline.finish();
            void service.dispose();
        };
    }, [pipeline, service]);

    // Losing the DRAW capability mid-stroke closes the stroke
    // locally and stops sending: the server refuses every
    // DRAW_* from a user without DRAW with a ROOM_ERROR, and
    // the room client treats an unsolicited ROOM_ERROR as the
    // end of the room.
    useEffect(() => {
        if (!canDraw) pipeline.finish({ sendEnd: false });
    }, [canDraw, pipeline]);

    // Leaving drawing mode (toolbar close, Escape, laser)
    // ends any stroke in progress.
    useEffect(() => {
        if (!isDrawing) pipeline.finish();
    }, [isDrawing, pipeline]);

    const sampleFrom = useCallback(
        (e: React.PointerEvent<HTMLCanvasElement>): PointerSample | null => {
            const canvas = canvasRef.current;
            if (!canvas) return null;
            const rect = canvas.getBoundingClientRect();
            const x = Math.max(0, Math.min(1, (e.clientX - rect.left) / rect.width));
            const y = Math.max(0, Math.min(1, (e.clientY - rect.top) / rect.height));
            return { x, y, pressure: e.pressure || 0, ts: Date.now(), pointerId: e.pointerId };
        },
        [],
    );

    const handlePointerDown = useCallback(
        (e: React.PointerEvent<HTMLCanvasElement>) => {
            if (!isDrawing) return;
            const sample = sampleFrom(e);
            if (sample === null) return;
            pipeline.down(
                {
                    tool: keyboard.drawingMode as StrokeTool,
                    color: strokeColor,
                    width: strokeWidth,
                },
                sample,
            );
        },
        [isDrawing, sampleFrom, pipeline, keyboard.drawingMode, strokeColor, strokeWidth],
    );

    const handlePointerMove = useCallback(
        (e: React.PointerEvent<HTMLCanvasElement>) => {
            if (!isDrawing) return;
            const sample = sampleFrom(e);
            if (sample === null) return;
            pipeline.move(sample);
        },
        [isDrawing, sampleFrom, pipeline],
    );

    const handlePointerUp = useCallback(
        (e: React.PointerEvent<HTMLCanvasElement>) => {
            if (!isDrawing) return;
            const sample = sampleFrom(e);
            if (sample === null) return;
            pipeline.up(sample);
        },
        [isDrawing, sampleFrom, pipeline],
    );

    // pointercancel (touch palm rejection, OS gesture) and
    // leaving the canvas end the stroke where it is.
    const handlePointerCancel = useCallback(
        (e: React.PointerEvent<HTMLCanvasElement>) => {
            pipeline.cancel({ x: 0, y: 0, pressure: 0, ts: 0, pointerId: e.pointerId });
        },
        [pipeline],
    );

    return (
        <>
            <canvas
                ref={canvasRef}
                className="drawing-layer"
                data-testid="locast-drawing-layer"
                aria-hidden="true"
                style={{ pointerEvents: isDrawing ? "auto" : "none" }}
                onPointerDown={handlePointerDown}
                onPointerMove={handlePointerMove}
                onPointerUp={handlePointerUp}
                onPointerCancel={handlePointerCancel}
                onPointerLeave={handlePointerUp}
            />
            {/* P5-T04/P5-T06: laser pointer overlay */}
            <LaserPointer
                videoRef={videoRef}
                localUserId={userId ?? "local"}
                laserActive={keyboard.laserActive}
            />
            {/* P5-T06: drawing toolbar */}
            <DrawingToolbar
                visible={keyboard.toolbarVisible}
                activeTool={activeTool}
                color={strokeColor}
                strokeWidth={strokeWidth}
                onToolSelect={handleToolSelect}
                onColorChange={handleColorChange}
                onStrokeWidthChange={handleStrokeWidthChange}
                onClose={handleToolbarClose}
            />
        </>
    );
}