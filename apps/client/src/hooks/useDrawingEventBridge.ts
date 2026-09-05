import { useEffect, useRef } from "react";
import { listenEvent } from "../services/_eventTransport";
import type { StrokeBeginEvent, StrokePointEvent, StrokeEndEvent } from "../bindings/index";
import type { StrokeTool } from "../drawing/types";
import {
    fromStrokeBeginEvent,
    fromStrokePointEvent,
    fromStrokeEndEvent,
    type RemoteStrokeBeginPayload,
    type RemoteStrokePointPayload,
    type RemoteStrokeEndPayload,
} from "../services/drawingRemote";
import { useDrawingStore } from "../stores/useDrawingStore";

interface DrawingEventHandlers {
    onBegin?: (payload: RemoteStrokeBeginPayload) => void;
    onPoint?: (payload: RemoteStrokePointPayload) => void;
    onEnd?: (payload: RemoteStrokeEndPayload) => void;
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
            };
        };
        w.__locastDrawingStore = {
            getAllStrokes: () => useDrawingStore.getState().getAllStrokes(),
            setRoomId: (id) => useDrawingStore.getState().setRoomId(id),
            clearRoom: () => useDrawingStore.getState().clearRoom(),
            beginStroke: (opts) => useDrawingStore.getState().beginStroke(opts),
            appendPoint: (opts) => useDrawingStore.getState().appendPoint(opts),
            endStroke: (opts) => useDrawingStore.getState().endStroke(opts),
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
