// P5-T01: React hook that drives the drawing canvas.
//
// Responsibilities:
//
// 1. Track the video element's intrinsic dimensions
//    (`videoWidth` / `videoHeight`) via a `ResizeObserver`
//    on the video element. When the source changes, the
//    observer fires and the hook records the new
//    intrinsic size.
// 2. Maintain the canonical stroke history (`strokes`).
//    The renderer is re-invoked from a `useEffect` on
//    every relevant change (strokes, intrinsic size, the
//    canvas's CSS display size).
// 3. Expose the imperative API a future drawing toolbar
//    (or the test suite) uses: `beginStroke`,
//    `appendPoint`, `endStroke`, `clear`, `undo`,
//    `setStrokeStyle`.
// 4. Mount the test seam (`window.__locastDrawing`) in
//    `MODE === "test"` so Playwright can drive strokes
//    deterministically (the Vite harness cannot load
//    arbitrary media into a `<video>` element, so a real
//    pen-tool acceptance test is infeasible; the seam
//    matches the pattern established by P4-T05's
//    `__locastDrift`).
//
// The hook does NOT install pointer event listeners on
// the canvas: P5-T01 ships the canvas with
// `pointer-events: none` so the native `<video controls>`
// overlay remains usable. Future tasks (a drawing mode
// toggle) will switch the canvas to `pointer-events:
// auto` and call into the hook's imperative API from a
// new pointer-pipeline handler.

import { useCallback, useEffect, useRef, useState } from "react";
import type { RefObject } from "react";
import type { Stroke, StrokePoint, StrokeTool } from "../drawing/types";
import { makeStrokeId, newStroke } from "../drawing/types";
import { renderStrokes } from "../drawing/strokeRenderer";
import type { RemoteStroke } from "../stores/useDrawingStore";

/** Configuration for the default pen style. The
 *  drawing toolbar (future task) will override these. */
const DEFAULT_COLOR = "#e6e6e6";
const DEFAULT_WIDTH = 3;
const DEFAULT_TOOL: StrokeTool = "pen";

/** A snapshot of the canvas backing-store dimensions
 *  (the video's intrinsic resolution). `null` when the
 *  video element has no metadata yet (no source, or
 *  metadata not loaded). */
export interface IntrinsicSize {
    width: number;
    height: number;
}

/** The hook's return value. The caller owns the
 *  `<canvas>` element via `canvasRef`; the hook owns
 *  the stroke history and the intrinsic-size state. */
export interface DrawingCanvasHandle {
    strokes: readonly Stroke[];
    intrinsicSize: IntrinsicSize | null;
    beginStroke: (opts?: Partial<BeginStrokeOpts>) => string;
    appendPoint: (point: StrokePoint) => void;
    endStroke: (endedAt?: number) => void;
    clear: () => void;
    undo: () => void;
    setStrokeStyle: (style: { color?: string; width?: number; tool?: StrokeTool }) => void;
    getActiveStroke: () => Stroke | null;
}

interface BeginStrokeOpts {
    tool: StrokeTool;
    color: string;
    width: number;
    userId: string;
}

/**
 * Drive the drawing canvas.
 *
 * `canvasRef` is the ref the calling component
 * (`DrawingLayer`) attaches to the `<canvas>` element.
 * The hook does NOT own the canvas DOM node itself
 * (the component does), so the ref is passed in and
 * used by the re-paint effect.
 *
 * `videoRef` is the live `<video>` element ref (shared
 * with the parent Player so drift sampling, manual
 * sync, and drawing all reference the same DOM node).
 *
 * `userId` is the local user's id; it is stamped into
 * every stroke the hook produces so future renderer
 * code can distinguish local vs remote strokes (§15.2).
 * Optional: defaults to `"local"` so the test seam
 * does not require a full identity-store stub.
 */
export function useDrawingCanvas(
    canvasRef: RefObject<HTMLCanvasElement | null>,
    videoRef: RefObject<HTMLVideoElement | null>,
    userId?: string | null,
    remoteStrokes?: readonly RemoteStroke[],
): DrawingCanvasHandle {
    // The canvas backing-store is the video's intrinsic
    // resolution. The intrinsic-size state is mirrored
    // to `intrinsicSizeRef` so the test seam can read
    // it synchronously between back-to-back calls.
    const [intrinsicSize, setIntrinsicSize] = useState<IntrinsicSize | null>(null);

    // The canonical stroke history. Live strokes
    // (`endedAt === 0`) live here too so the renderer
    // re-paints them in real time.
    const [strokes, setStrokes] = useState<Stroke[]>([]);

    // The active style (color/width/tool). Future
    // toolbar mutates this; defaults below are v1
    // placeholders.
    const styleRef = useRef<{ color: string; width: number; tool: StrokeTool }>({
        color: DEFAULT_COLOR,
        width: DEFAULT_WIDTH,
        tool: DEFAULT_TOOL,
    });
    const userIdRef = useRef<string>(userId ?? "local");

    // The currently-active (unfinished) stroke. Held in
    // a ref so `appendPoint` does not trigger a render
    // on every frame; the public `strokes` state is the
    // canonical read for the renderer.
    const activeRef = useRef<Stroke | null>(null);

    // Refs that mirror state for the test seam. The
    // imperative callbacks (`beginStroke`, `appendPoint`,
    // `endStroke`, `clear`, `undo`) update this ref
    // SYNCHRONOUSLY before calling `setStrokes` so that a
    // back-to-back `getStrokes()` call inside the same
    // `page.evaluate` block (no React render in between)
    // returns the latest state. The `setStrokes` call
    // schedules the corresponding React render, which
    // the re-paint effect picks up.
    const strokesRef = useRef<Stroke[]>(strokes);
    const intrinsicSizeRef = useRef<IntrinsicSize | null>(intrinsicSize);
    useEffect(() => {
        // Synchronize with the committed value after each
        // render (e.g. when an external reducer updates
        // state). The synchronous updates in the
        // imperative callbacks above cover the seam path;
        // this effect covers everything else (HMR,
        // future external setters).
        strokesRef.current = strokes;
        intrinsicSizeRef.current = intrinsicSize;
    });

    // Mirror userId changes into the ref without
    // re-creating the imperative API.
    useEffect(() => {
        userIdRef.current = userId ?? "local";
    }, [userId]);

    /**
     * Track the video's intrinsic dimensions. Uses a
     * `ResizeObserver` on the video element AND a
     * `loadedmetadata` listener (the observer fires
     * when the element's CSS box size changes; metadata
     * is what reveals `videoWidth` / `videoHeight`).
     *
     * When a NEW source arrives (different intrinsic
     * dimensions), the observer's entry fires and the
     * hook records the new dimensions. The canvas
     * backing store is resized on every entry so a
     * source change clears stale pixels (§15.2's "Canvas
     * is re-rendered from the local stroke history on
     * every relevant state change").
     */
    useEffect(() => {
        const video = videoRef.current;
        if (video === null) return undefined;

        const sync = (): void => {
            const w = video.videoWidth;
            const h = video.videoHeight;
            if (w > 0 && h > 0) {
                setIntrinsicSize((prev) =>
                    prev?.width === w && prev?.height === h
                        ? prev
                        : { width: w, height: h },
                );
            } else {
                // No source / metadata not loaded.
                setIntrinsicSize(null);
            }
        };

        // Initial sync (in case metadata is already
        // loaded when the effect mounts).
        sync();

        const ro = new ResizeObserver(() => {
            sync();
        });
        ro.observe(video);
        video.addEventListener("loadedmetadata", sync);

        return () => {
            ro.disconnect();
            video.removeEventListener("loadedmetadata", sync);
        };
    }, [videoRef]);

    /**
     * Repaint the canvas whenever the stroke history or
     * the intrinsic dimensions change. The canvas
     * backing store is sized to the video's intrinsic
     * dimensions on every render (matching §15.2
     * "Canvas size matches the video's intrinsic
     * dimensions"); the CSS display size is read from
     * the element's `getBoundingClientRect()` so a
     * CSS-driven resize is reflected without a
     * separate listener.
     */
    useEffect(() => {
        const canvas = canvasRef.current;
        if (canvas === null) return;
        if (intrinsicSize === null) return;

        // Backing store: video's intrinsic resolution.
        canvas.width = intrinsicSize.width;
        canvas.height = intrinsicSize.height;

        // CSS display size: the canvas element's actual
        // rendered box (driven by the parent's flex
        // layout + the video's aspect ratio). This
        // keeps the visual size correct even if the player
        // is resized.
        const rect = canvas.getBoundingClientRect();
        const cssWidth = rect.width || intrinsicSize.width;
        const cssHeight = rect.height || intrinsicSize.height;
        canvas.style.width = `${cssWidth}px`;
        canvas.style.height = `${cssHeight}px`;

        const ctx = canvas.getContext("2d");
        if (ctx === null) return;
        renderStrokes(
            ctx,
            strokes,
            remoteStrokes,
            intrinsicSize,
            cssWidth,
        );
    }, [strokes, remoteStrokes, intrinsicSize]);

    /**
     * Public imperative API. The future drawing toolbar
     * calls these from `onPointerDown` / `onPointerMove`
     * / `onPointerUp` handlers once the canvas has
     * `pointer-events: auto`; P5-T01's test seam uses
     * them directly.
     */
    const beginStroke = useCallback((opts?: Partial<BeginStrokeOpts>): string => {
        const tool = opts?.tool ?? styleRef.current.tool;
        const color = opts?.color ?? styleRef.current.color;
        const width = opts?.width ?? styleRef.current.width;
        const id = makeStrokeId("stroke");
        const stroke = newStroke({
            id,
            userId: opts?.userId ?? userIdRef.current,
            tool,
            color,
            width,
            startedAt: Date.now(),
        });
        activeRef.current = stroke;
        // Append the live stroke to the canonical state
        // at the tail of the list so:
        //   - earlier strokes keep their index for
        //     deterministic undo semantics (undo removes
        //     the most recent = tail);
        //   - the re-paint effect picks up the new
        //     stroke without re-numbering earlier ones.
        // The synchronous mirror is updated FIRST so a
        // back-to-back `getStrokes()` call inside the
        // same `page.evaluate` returns the latest
        // state.
        strokesRef.current = [...strokesRef.current, stroke];
        setStrokes(strokesRef.current);
        return id;
    }, []);

    const appendPoint = useCallback((point: StrokePoint): void => {
        const active = activeRef.current;
        if (active === null) return;
        // Update the synchronous mirror first.
        strokesRef.current = strokesRef.current.map((s) =>
            s.id === active.id
                ? { ...s, points: [...s.points, point] }
                : s,
        );
        setStrokes(strokesRef.current);
    }, []);

    const endStroke = useCallback((endedAt?: number): void => {
        const active = activeRef.current;
        if (active === null) return;
        const ended = endedAt ?? Date.now();
        strokesRef.current = strokesRef.current.map((s) =>
            s.id === active.id ? { ...s, endedAt: ended } : s,
        );
        setStrokes(strokesRef.current);
        activeRef.current = null;
    }, []);

    const clear = useCallback((): void => {
        activeRef.current = null;
        strokesRef.current = [];
        setStrokes([]);
    }, []);

    const undo = useCallback((): void => {
        strokesRef.current = strokesRef.current.slice(
            0,
            strokesRef.current.length - 1,
        );
        setStrokes(strokesRef.current);
    }, []);

    const setStrokeStyle = useCallback(
        (next: { color?: string; width?: number; tool?: StrokeTool }): void => {
            if (next.color !== undefined) {
                styleRef.current.color = next.color;
            }
            if (next.width !== undefined) {
                styleRef.current.width = next.width;
            }
            if (next.tool !== undefined) {
                styleRef.current.tool = next.tool;
            }
        },
        [],
    );

    const getActiveStroke = useCallback((): Stroke | null => {
        return activeRef.current;
    }, []);

    /**
     * Test seam. Mounted only when `MODE === "test"`
     * so production bundles are tree-shaken clean.
     * Mirrors the `__locastDrift` and `__locastStore`
     * seams used by P4-T05 / P4-T07 tests: deterministic
     * driving of state without needing a real DOM
     * pointer pipeline.
     *
     * Installed ONCE (no deps) and reads the latest
     * state via refs so re-installs do not blank the
     * seam mid-test. The seam wraps the hook's
     * imperative callbacks in plain arrow functions so
     * the seam value remains a real `function` rather
     * than a possibly-optimized-out `useCallback`
     * binding across StrictMode double-renders.
     *
     * `setIntrinsicSizeForTest` exists because
     * `HTMLVideoElement.videoWidth` /
     * `videoHeight` are [LegacyUnforgeable] in WebIDL
     * and CANNOT be redefined by the Vite test harness.
     * The seam lets a Playwright test inject a synthetic
     * intrinsic size that the hook then drives into
     * the canvas backing store, exercising the resize
     * re-paint path.
     */
    useEffect(() => {
        if (import.meta.env.MODE !== "test") return undefined;
        const w = window as unknown as Record<string, unknown>;
        w.__locastDrawing = {
            getStrokes: () => strokesRef.current,
            getIntrinsicSize: () => intrinsicSizeRef.current,
            setIntrinsicSizeForTest: (size: IntrinsicSize | null) => {
                if (size === null) {
                    intrinsicSizeRef.current = null;
                    setIntrinsicSize(null);
                    return;
                }
                intrinsicSizeRef.current = size;
                setIntrinsicSize(size);
            },
            beginStroke: (opts?: Partial<BeginStrokeOpts>) => beginStroke(opts),
            appendPoint: (point: StrokePoint) => appendPoint(point),
            endStroke: (endedAt?: number) => endStroke(endedAt),
            clear: () => clear(),
            undo: () => undo(),
            setStrokeStyle: (next: { color?: string; width?: number; tool?: StrokeTool }) =>
                setStrokeStyle(next),
        };
        return () => {
            if (w.__locastDrawing !== undefined) {
                delete w.__locastDrawing;
            }
        };
    }, [
        beginStroke,
        appendPoint,
        endStroke,
        clear,
        undo,
        setStrokeStyle,
    ]);

    return {
        strokes,
        intrinsicSize,
        beginStroke,
        appendPoint,
        endStroke,
        clear,
        undo,
        setStrokeStyle,
        getActiveStroke,
    };
}