import { useEffect, useRef } from "react";
import { listenEvent } from "../services/_eventTransport";
import type {
    StrokeBeginEvent,
    StrokePointEvent,
    StrokeEndEvent,
    StrokeUndoEvent,
    StrokeClearEvent,
} from "../bindings/index";
import type { StrokeTool } from "../drawing/types";
import {
    fromStrokeBeginEvent,
    fromStrokePointEvent,
    fromStrokeEndEvent,
    fromStrokeUndoEvent,
    fromStrokeClearEvent,
    type RemoteStrokeBeginPayload,
    type RemoteStrokePointPayload,
    type RemoteStrokeEndPayload,
    type RemoteStrokeUndoPayload,
    type RemoteStrokeClearPayload,
} from "../services/drawingRemote";
import { useDrawingStore } from "../stores/useDrawingStore";

interface DrawingEventHandlers {
    onBegin?: (payload: RemoteStrokeBeginPayload) => void;
    onPoint?: (payload: RemoteStrokePointPayload) => void;
    onEnd?: (payload: RemoteStrokeEndPayload) => void;
    /** P5-T03: called AFTER the remote store dropped the stroke, for
     *  every accepted undo (the local user's own included). The owner
     *  of the local canvas uses it to drop the stroke it holds. */
    onUndo?: (payload: RemoteStrokeUndoPayload) => void;
    /** P5-T03: called AFTER the remote store was emptied, for every
     *  accepted clear (the local user's own included). */
    onClear?: (payload: RemoteStrokeClearPayload) => void;
}

export function useDrawingEventBridge(handlers: DrawingEventHandlers = {}): void {
    const handlersRef = useRef(handlers);
    handlersRef.current = handlers;

    useEffect(() => {
        let cancelled = false;
        const unsubs: Array<() => void> = [];

        (async () => {
            const onBegin = (ev: StrokeBeginEvent) => {
                if (cancelled) return;
                const currentRoomId = useDrawingStore.getState().roomId;
                const payload = fromStrokeBeginEvent(ev);
                if (payload.roomId !== currentRoomId) return;
                useDrawingStore.getState().beginStroke({
                    strokeId: payload.strokeId,
                    userId: payload.senderId,
                    tool: payload.tool,
                    color: payload.color,
                    width: payload.width,
                    x: payload.x,
                    y: payload.y,
                    pressure: payload.pressure,
                    tsMs: payload.tsMs,
                });
                handlersRef.current.onBegin?.(payload);
            };

            const onPoint = (ev: StrokePointEvent) => {
                if (cancelled) return;
                const currentRoomId = useDrawingStore.getState().roomId;
                const payload = fromStrokePointEvent(ev);
                if (payload.roomId !== currentRoomId) return;
                useDrawingStore.getState().appendPoint({
                    strokeId: payload.strokeId,
                    x: payload.x,
                    y: payload.y,
                    pressure: payload.pressure,
                    tsMs: payload.tsMs,
                });
                handlersRef.current.onPoint?.(payload);
            };

            const onEnd = (ev: StrokeEndEvent) => {
                if (cancelled) return;
                const currentRoomId = useDrawingStore.getState().roomId;
                const payload = fromStrokeEndEvent(ev);
                if (payload.roomId !== currentRoomId) return;
                useDrawingStore.getState().endStroke({
                    strokeId: payload.strokeId,
                    tsMs: payload.tsMs,
                });
                handlersRef.current.onEnd?.(payload);
            };

            const onUndo = (ev: StrokeUndoEvent) => {
                if (cancelled) return;
                const currentRoomId = useDrawingStore.getState().roomId;
                const payload = fromStrokeUndoEvent(ev);
                if (payload.roomId !== currentRoomId) return;
                useDrawingStore.getState().removeStroke(payload.strokeId);
                handlersRef.current.onUndo?.(payload);
            };

            const onClear = (ev: StrokeClearEvent) => {
                if (cancelled) return;
                const currentRoomId = useDrawingStore.getState().roomId;
                const payload = fromStrokeClearEvent(ev);
                if (payload.roomId !== currentRoomId) return;
                useDrawingStore.getState().clearRoom();
                handlersRef.current.onClear?.(payload);
            };

            try {
                const u1 = await listenEvent<StrokeBeginEvent>("drawing://begin", onBegin);
                if (cancelled) { u1(); return; }
                unsubs.push(u1);

                const u2 = await listenEvent<StrokePointEvent>("drawing://point", onPoint);
                if (cancelled) { u2(); return; }
                unsubs.push(u2);

                const u3 = await listenEvent<StrokeEndEvent>("drawing://end", onEnd);
                if (cancelled) { u3(); return; }
                unsubs.push(u3);

                const u4 = await listenEvent<StrokeUndoEvent>("drawing://undo", onUndo);
                if (cancelled) { u4(); return; }
                unsubs.push(u4);

                const u5 = await listenEvent<StrokeClearEvent>("drawing://clear", onClear);
                if (cancelled) { u5(); return; }
                unsubs.push(u5);

                if (typeof window !== "undefined") {
                    (window as unknown as { __locast_drawing_subscribed?: boolean }).__locast_drawing_subscribed = true;
                }
            } catch (err) {
                console.warn("useDrawingEventBridge: listen failed", err);
            }
        })();

        return () => {
            cancelled = true;
            for (const u of unsubs) {
                try { u(); } catch { /* swallow */ }
            }
        };
    }, []);

    useEffect(() => {
        if (import.meta.env.MODE !== "test") return;
        const w = window as unknown as {
            __locastDrawingStore?: {
                getAllStrokes: () => unknown;
                setRoomId: (id: string | null) => void;
                clearRoom: () => void;
                beginStroke: (opts: {
                    strokeId: string;
                    userId: string;
                    tool: StrokeTool;
                    color: string;
                    width: number;
                    x: number;
                    y: number;
                    pressure: number;
                    tsMs: number;
                }) => void;
                appendPoint: (opts: {
                    strokeId: string;
                    x: number;
                    y: number;
                    pressure: number;
                    tsMs: number;
                }) => void;
                endStroke: (opts: {
                    strokeId: string;
                    tsMs: number;
                }) => void;
                removeStroke: (strokeId: string) => void;
            };
        };
        w.__locastDrawingStore = {
            getAllStrokes: () => useDrawingStore.getState().getAllStrokes(),
            setRoomId: (id) => useDrawingStore.getState().setRoomId(id),
            clearRoom: () => useDrawingStore.getState().clearRoom(),
            beginStroke: (opts) => useDrawingStore.getState().beginStroke(opts),
            appendPoint: (opts) => useDrawingStore.getState().appendPoint(opts),
            endStroke: (opts) => useDrawingStore.getState().endStroke(opts),
            removeStroke: (strokeId) => useDrawingStore.getState().removeStroke(strokeId),
        };
        return () => {
            if (w.__locastDrawingStore) delete w.__locastDrawingStore;
        };
    }, []);
}

export function useDrawingRoomSync(roomId: string | null): void {
    const setRoomId = useDrawingStore((s) => s.setRoomId);
    useEffect(() => {
        setRoomId(roomId);
    }, [roomId, setRoomId]);
}
