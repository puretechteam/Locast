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
// P5-T03 (undo / clear): Ctrl+Z and the toolbar's Undo send a
// DRAW_UNDO for the user's newest finished stroke when they hold
// UNDO_OWN / UNDO_ANY; the server's DRAW_UNDO / DRAW_CLEAR events
// (delivered to the actor too) are what remove strokes from the
// canvas. Without the capability Ctrl+Z only drops the newest
// stroke from this client's own canvas, as before.
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
import { planUndo, UndoTracker } from "../drawing/undoPolicy";
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

/** A local stroke finished this recently may not have reached the
 *  server yet; a DRAW_SYNC does not remove it. */
const RECENT_LOCAL_STROKE_MS = 3000;

export function DrawingLayer({
    videoRef,
    userId,
    roomId,
}: DrawingLayerProps): React.ReactNode {
    const canvasRef = useRef<HTMLCanvasElement | null>(null);

    useDrawingRoomSync(roomId ?? null);

    const remoteStrokes = useDrawingStore((s) => s.getAllStrokes());

    const {
        beginStroke,
        appendPoint,
        endStroke,
        undo,
        clear,
        removeStroke,
        getLocalStrokes,
        setStrokeStyle,
    } = useDrawingCanvas(canvasRef, videoRef, userId, remoteStrokes);

    // Undo requests already on their way to the server (or given up on
    // after a silent refusal), so a second Ctrl+Z targets the next
    // stroke instead of the same one.
    const undoTrackerRef = useRef<UndoTracker>(new UndoTracker());

    // P5-T03: the server's undo / clear events (the actor's own
    // included) are what take strokes off the canvas. The bridge has
    // already updated the remote store; drop them from the local
    // canvas as well.
    useDrawingEventBridge({
        onUndo: (payload) => {
            removeStroke(payload.strokeId);
            undoTrackerRef.current.confirmed(payload.strokeId);
        },
        onClear: () => {
            clear();
            undoTrackerRef.current.reset();
        },
        // This client missed drawing events and the server sent its
        // whole drawing state. Local strokes the server no longer has
        // (undone or cleared while we were behind) leave the canvas.
        // Kept: the stroke being drawn right now, and strokes finished
        // in the last RECENT_LOCAL_STROKE_MS, whose events may still be
        // on their way to the server (the snapshot can predate them,
        // and the server never echoes our own strokes back).
        onSync: (payload) => {
            const onServer = new Set(payload.strokes.map((s) => s.strokeId));
            const now = Date.now();
            for (const s of getLocalStrokes()) {
                const settled = s.endedAt !== 0 && now - s.endedAt >= RECENT_LOCAL_STROKE_MS;
                if (settled && !onServer.has(s.id)) removeStroke(s.id);
            }
            undoTrackerRef.current.reset();
        },
        localStrokeIds: () => new Set(getLocalStrokes().map((s) => s.id)),
    });

    const [activeTool, setActiveTool] = useState<DrawingTool>("pen");
    const [strokeColor, setStrokeColor] = useState("#e6e6e6");
    const [strokeWidth, setStrokeWidth] = useState(3);

    const youCapSet = useCapabilityStore((s) => s.youCapSet);
    const canDraw = youCapSet !== null && (youCapSet & CAP.DRAW) !== 0;
    // UI gating only: the server decides what is allowed.
    const canUndoOwn =
        youCapSet !== null && (youCapSet & (CAP.UNDO_OWN | CAP.UNDO_ANY)) !== 0;
    const canClearAll = youCapSet !== null && (youCapSet & CAP.CLEAR_ALL) !== 0;

    // P5-T02: the production send path. One DrawingService
    // per room (a new roomId gives a fresh instance, so no
    // stroke id or queued send leaks across rooms).
    // eslint-disable-next-line react-hooks/exhaustive-deps -- roomId is a deliberate memo key: a new room gets a fresh DrawingService.
    const service = useMemo(() => new DrawingService(), [roomId]);
    useEffect(() => {
        // A new room (service) starts with a clean slate; unmount stops
        // the timers.
        const tracker = new UndoTracker();
        undoTrackerRef.current = tracker;
        return () => tracker.reset();
    }, [service]);

    const canUndoOwnRef = useRef(canUndoOwn);
    canUndoOwnRef.current = canUndoOwn;

    // Ctrl+Z / the Undo button. With UNDO_OWN / UNDO_ANY: ask the
    // server to undo the newest finished local stroke (it disappears
    // when the server's event arrives). Without: drop the newest
    // stroke from this canvas only, as before P5-T03.
    const handleUndo = useCallback(() => {
        const plan = planUndo({
            canUndoOwn: canUndoOwnRef.current,
            strokes: getLocalStrokes(),
            pending: undoTrackerRef.current.pending,
            gaveUp: undoTrackerRef.current.gaveUp,
        });
        if (plan.kind === "local") {
            undo();
            return;
        }
        if (plan.kind !== "remote") return;
        const tracker = undoTrackerRef.current;
        tracker.markSent(plan.strokeId);
        service.undoStroke(plan.strokeId).catch(() => {
            // Already reported through the session's onError; allow a retry.
            tracker.sendFailed(plan.strokeId);
        });
    }, [getLocalStrokes, undo, service]);

    const handleClearAll = useCallback(() => {
        service.clearAll().catch(() => undefined);
    }, [service]);

    const keyboard = useKeyboardScope({
        onUndo: handleUndo,
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

    // The pipeline couples the local hook (store + renderer)
    // with the service so the local stroke id is the wire
    // stroke id.
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
                roomId={roomId ?? null}
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
                canUndo={canUndoOwn}
                canClearAll={canClearAll}
                onUndo={handleUndo}
                onClearAll={handleClearAll}
            />
        </>
    );
}