// P5-T06: keyboard shortcut handler hook.
//
// Responsibilities:
//
// 1. Listen for global keydown events to toggle:
//    - `d` → toolbar visibility
//    - `l` → laser mode
//    - `Escape` → exit drawing mode / deactivate laser
//    - `Ctrl+Z` / `Cmd+Z` → undo
//    - `Ctrl+Y` / `Cmd+Y` → redo (future)
//    - `Ctrl+Shift+Z` → redo (future)
// 2. Expose current mode state so callers can
//    conditionally render UI or suppress events.
//
// The hook does NOT own the canvas or laser renderer;
// it only reports state changes to its owner.

import { useCallback, useEffect, useRef, useState } from "react";
import type { StrokeTool } from "../drawing/types";

export type DrawingTool = StrokeTool;
export type DrawingMode = DrawingTool | "none";
export type CanvasMode = "idle" | "drawing" | "laser";

export interface KeyboardScopeHandle {
    toolbarVisible: boolean;
    laserActive: boolean;
    drawingMode: DrawingMode;
    canvasMode: CanvasMode;
    undo: () => void;
    toggleToolbar: () => void;
    setDrawingMode: (mode: DrawingMode) => void;
    setLaserActive: (active: boolean) => void;
    isDrawing: boolean;
}

const TOOLBAR_TOGGLE_KEY = "d";
const LASER_TOGGLE_KEY = "l";

export function useKeyboardScope(handlers: {
    onUndo?: () => void;
    canDraw?: boolean;
}): KeyboardScopeHandle {
    const canDraw = handlers.canDraw ?? false;
    const [toolbarVisible, setToolbarVisible] = useState(false);
    const [laserActive, setLaserActive] = useState(false);
    const [drawingMode, setDrawingModeState] = useState<DrawingMode>("none");

    const onUndoRef = useRef(handlers.onUndo);
    useEffect(() => {
        onUndoRef.current = handlers.onUndo;
    });

    const isDrawing = drawingMode !== "none";

    const canvasMode: CanvasMode = isDrawing
        ? "drawing"
        : laserActive
          ? "laser"
          : "idle";

    const toggleToolbar = useCallback(() => {
        if (canDraw) {
            setToolbarVisible((v) => !v);
        }
    }, [canDraw]);

    const setDrawingMode = useCallback((mode: DrawingMode) => {
        setDrawingModeState(mode);
        if (mode !== "none") {
            setLaserActive(false);
            setToolbarVisible(false);
        }
    }, []);

    const setLaserActiveHandler = useCallback((active: boolean) => {
        setLaserActive(active);
        if (active) {
            setDrawingModeState("none");
            setToolbarVisible(false);
        }
    }, []);

    const exitAll = useCallback(() => {
        setDrawingModeState("none");
        setLaserActive(false);
        setToolbarVisible(false);
    }, []);

    const handleUndo = useCallback(() => {
        onUndoRef.current?.();
    }, []);

    useEffect(() => {
        const onKeyDown = (e: KeyboardEvent): void => {
            if (e.target instanceof HTMLInputElement || e.target instanceof HTMLTextAreaElement) {
                return;
            }

            const key = e.key.toLowerCase();
            const ctrl = e.ctrlKey || e.metaKey;

            if (ctrl && key === "z" && !e.shiftKey) {
                e.preventDefault();
                handleUndo();
                return;
            }

            if (ctrl && (key === "y" || (key === "z" && e.shiftKey))) {
                // redo — future
                return;
            }

            switch (key) {
                case TOOLBAR_TOGGLE_KEY: {
                    e.preventDefault();
                    if (!laserActive && drawingMode === "none") {
                        toggleToolbar();
                    }
                    break;
                }
                case LASER_TOGGLE_KEY: {
                    e.preventDefault();
                    if (!isDrawing) {
                        setLaserActiveHandler(!laserActive);
                    }
                    break;
                }
                case "escape": {
                    e.preventDefault();
                    exitAll();
                    break;
                }
            }
        };

        window.addEventListener("keydown", onKeyDown);
        return () => window.removeEventListener("keydown", onKeyDown);
    }, [laserActive, drawingMode, isDrawing, canDraw, toggleToolbar, setLaserActiveHandler, exitAll, handleUndo]);

    return {
        toolbarVisible,
        laserActive,
        drawingMode,
        canvasMode,
        undo: handleUndo,
        toggleToolbar,
        setDrawingMode,
        setLaserActive: setLaserActiveHandler,
        isDrawing,
    };
}
