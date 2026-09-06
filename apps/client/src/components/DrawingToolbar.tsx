// P5-T06: drawing toolbar overlay.
//
// Renders a floating toolbar above the video with:
// - Tool buttons: pen, eraser, arrow, rect, circle, text
// - Color picker
// - Stroke width
// - Close button
//
// The toolbar is toggled by the `d` key and positioned
// in the top-right corner of the player stage.

import { useCallback } from "react";
import type { DrawingTool } from "../hooks/useKeyboardScope";

export interface DrawingToolbarProps {
    visible: boolean;
    activeTool: DrawingTool;
    color: string;
    strokeWidth: number;
    onToolSelect: (tool: DrawingTool) => void;
    onColorChange: (color: string) => void;
    onStrokeWidthChange: (width: number) => void;
    onClose: () => void;
}

interface ToolDef {
    id: DrawingTool;
    label: string;
    title: string;
}

const TOOLS: ToolDef[] = [
    { id: "pen", label: "P", title: "Pen (P)" },
    { id: "eraser", label: "E", title: "Eraser (E)" },
    { id: "arrow", label: "A", title: "Arrow (A)" },
    { id: "rect", label: "R", title: "Rectangle (R)" },
    { id: "circle", label: "C", title: "Circle (C)" },
    { id: "text", label: "T", title: "Text (T)" },
];

const PRESET_COLORS = [
    "#e6e6e6",
    "#ff5c69",
    "#ff9f43",
    "#feca57",
    "#48dbfb",
    "#1dd1a1",
    "#5f27cd",
    "#ff6b6b",
];

export function DrawingToolbar({
    visible,
    activeTool,
    color,
    strokeWidth,
    onToolSelect,
    onColorChange,
    onStrokeWidthChange,
    onClose,
}: DrawingToolbarProps): React.ReactNode {
    const handleToolClick = useCallback(
        (tool: DrawingTool) => {
            onToolSelect(tool);
        },
        [onToolSelect],
    );

    if (!visible) return null;

    return (
        <div
            className="drawing-toolbar"
            role="toolbar"
            aria-label="Drawing tools"
            data-testid="drawing-toolbar"
        >
            <div className="drawing-toolbar__header">
                <span className="drawing-toolbar__title">Draw</span>
                <button
                    className="drawing-toolbar__close"
                    onClick={onClose}
                    aria-label="Close toolbar"
                    title="Close (Escape)"
                    data-testid="drawing-toolbar-close"
                >
                    ✕
                </button>
            </div>

            <div className="drawing-toolbar__tools" role="group" aria-label="Tool selection">
                {TOOLS.map((tool) => (
                    <button
                        key={tool.id}
                        className={`drawing-toolbar__tool${activeTool === tool.id ? " drawing-toolbar__tool--active" : ""}`}
                        onClick={() => handleToolClick(tool.id)}
                        title={tool.title}
                        aria-pressed={activeTool === tool.id}
                        data-testid={`drawing-toolbar-tool-${tool.id}`}
                    >
                        {tool.label}
                    </button>
                ))}
            </div>

            <div className="drawing-toolbar__section">
                <label className="drawing-toolbar__label" htmlFor="drawing-toolbar-color">
                    Color
                </label>
                <div className="drawing-toolbar__colors" role="group" aria-label="Color selection">
                    {PRESET_COLORS.map((c) => (
                        <button
                            key={c}
                            className={`drawing-toolbar__color-swatch${color === c ? " drawing-toolbar__color-swatch--active" : ""}`}
                            style={{ backgroundColor: c }}
                            onClick={() => onColorChange(c)}
                            title={c}
                            aria-label={`Color ${c}`}
                            aria-pressed={color === c}
                            data-testid={`drawing-toolbar-color-${c.replace("#", "")}`}
                        />
                    ))}
                    <div className="drawing-toolbar__color-input-wrapper">
                        <input
                            id="drawing-toolbar-color"
                            type="color"
                            className="drawing-toolbar__color-input"
                            value={color}
                            onChange={(e) => onColorChange(e.target.value)}
                            title="Custom color"
                            aria-label="Custom color"
                            data-testid="drawing-toolbar-color-custom"
                        />
                    </div>
                </div>
            </div>

            <div className="drawing-toolbar__section">
                <label className="drawing-toolbar__label" htmlFor="drawing-toolbar-width">
                    Width: {strokeWidth}px
                </label>
                <input
                    id="drawing-toolbar-width"
                    type="range"
                    className="drawing-toolbar__width-slider"
                    min={1}
                    max={20}
                    step={1}
                    value={strokeWidth}
                    onChange={(e) => onStrokeWidthChange(Number(e.target.value))}
                    aria-label="Stroke width"
                    data-testid="drawing-toolbar-width-slider"
                />
            </div>
        </div>
    );
}
