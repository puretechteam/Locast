// P5-T01: the drawing canvas overlay component.
//
// Renders a transparent `<canvas>` positioned over the
// `<video>` element. The component:
//
// - Owns the canvas DOM element via a ref the parent
//   hook (`useDrawingCanvas`) drives.
// - Pointer event handling is managed by P5-T06 when
//   drawing mode is active.
// - Reads `data-testid` selectors that the Playwright
//   suite uses to verify presence, intrinsic-size, and
//   resize behavior.
// P5-T03: also subscribes to remote drawing events
// (DRAW_BEGIN/POINT/END rebroadcast) and renders them
// on the same canvas.
// P5-T04: laser pointer overlay integrated here.
// P5-T06: drawing toolbar and keyboard shortcuts.

import { useCallback, useEffect, useRef, useState } from "react";
import type { RefObject } from "react";
import { useDrawingCanvas } from "../hooks/useDrawingCanvas";
import { useDrawingEventBridge, useDrawingRoomSync } from "../hooks/useDrawingEventBridge";
import { useDrawingStore } from "../stores/useDrawingStore";
import { useCapabilityStore, CAP } from "../stores/useCapabilityStore";
import { useKeyboardScope } from "../hooks/useKeyboardScope";
import type { DrawingTool, DrawingMode } from "../hooks/useKeyboardScope";
import type { StrokeTool } from "../drawing/types";
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

    const handlePointerDown = useCallback(
        (e: React.PointerEvent<HTMLCanvasElement>) => {
            if (!isDrawing) return;
            const canvas = canvasRef.current;
            if (!canvas) return;
            const rect = canvas.getBoundingClientRect();
            const x = Math.max(0, Math.min(1, (e.clientX - rect.left) / rect.width));
            const y = Math.max(0, Math.min(1, (e.clientY - rect.top) / rect.height));
            beginStroke({
                tool: keyboard.drawingMode as StrokeTool,
                color: strokeColor,
                width: strokeWidth,
            });
            appendPoint({ x, y, pressure: e.pressure || 0, ts: Date.now() });
        },
        [isDrawing, beginStroke, appendPoint, keyboard.drawingMode, strokeColor, strokeWidth],
    );

    const handlePointerMove = useCallback(
        (e: React.PointerEvent<HTMLCanvasElement>) => {
            if (!isDrawing) return;
            const canvas = canvasRef.current;
            if (!canvas) return;
            const rect = canvas.getBoundingClientRect();
            const x = Math.max(0, Math.min(1, (e.clientX - rect.left) / rect.width));
            const y = Math.max(0, Math.min(1, (e.clientY - rect.top) / rect.height));
            appendPoint({ x, y, pressure: e.pressure || 0, ts: Date.now() });
        },
        [isDrawing, appendPoint],
    );

    const handlePointerUp = useCallback(
        (e: React.PointerEvent<HTMLCanvasElement>) => {
            if (!isDrawing) return;
            const canvas = canvasRef.current;
            if (!canvas) return;
            const rect = canvas.getBoundingClientRect();
            const x = Math.max(0, Math.min(1, (e.clientX - rect.left) / rect.width));
            const y = Math.max(0, Math.min(1, (e.clientY - rect.top) / rect.height));
            appendPoint({ x, y, pressure: e.pressure || 0, ts: Date.now() });
            endStroke();
        },
        [isDrawing, appendPoint, endStroke],
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