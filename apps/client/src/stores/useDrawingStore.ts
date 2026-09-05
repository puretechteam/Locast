import { create } from "zustand";
import type { Stroke, StrokePoint, StrokeTool } from "../drawing/types";

export type { Stroke, StrokePoint, StrokeTool };

export interface RemoteStroke {
    id: string;
    userId: string;
    tool: StrokeTool;
    color: string;
    width: number;
    points: StrokePoint[];
    startedAt: number;
    endedAt: number;
}

interface DrawingStoreState {
    roomId: string | null;
    activeStrokes: Map<string, RemoteStroke>;
    completedStrokes: RemoteStroke[];

    setRoomId: (roomId: string | null) => void;

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

    clearRoom: () => void;

    getActiveStroke: (strokeId: string) => RemoteStroke | undefined;

    getCompletedStrokes: () => readonly RemoteStroke[];

    getAllStrokes: () => readonly RemoteStroke[];
}

export const useDrawingStore = create<DrawingStoreState>((set, get) => ({
    roomId: null,
    activeStrokes: new Map(),
    completedStrokes: [],

    setRoomId: (roomId) => {
        if (roomId !== get().roomId) {
            set({
                roomId,
                activeStrokes: new Map(),
                completedStrokes: [],
            });
        }
    },

    beginStroke: ({ strokeId, userId, tool, color, width, x, y, pressure, tsMs }) => {
        const stroke: RemoteStroke = {
            id: strokeId,
            userId,
            tool,
            color,
            width,
            points: [{ x, y, pressure, ts: tsMs }],
            startedAt: tsMs,
            endedAt: 0,
        };
        set((state) => {
            const activeStrokes = new Map(state.activeStrokes);
            activeStrokes.set(strokeId, stroke);
            return { activeStrokes };
        });
    },

    appendPoint: ({ strokeId, x, y, pressure, tsMs }) => {
        set((state) => {
            const activeStrokes = new Map(state.activeStrokes);
            const stroke = activeStrokes.get(strokeId);
            if (!stroke) return state;
            activeStrokes.set(strokeId, {
                ...stroke,
                points: [...stroke.points, { x, y, pressure, ts: tsMs }],
            });
            return { activeStrokes };
        });
    },

    endStroke: ({ strokeId, tsMs }) => {
        set((state) => {
            const activeStrokes = new Map(state.activeStrokes);
            const stroke = activeStrokes.get(strokeId);
            if (!stroke) return state;
            activeStrokes.delete(strokeId);
            const completed: RemoteStroke = { ...stroke, endedAt: tsMs };
            return {
                activeStrokes,
                completedStrokes: [...state.completedStrokes, completed],
            };
        });
    },

    clearRoom: () => {
        set({
            activeStrokes: new Map(),
            completedStrokes: [],
        });
    },

    getActiveStroke: (strokeId) => {
        return get().activeStrokes.get(strokeId);
    },

    getCompletedStrokes: () => {
        return get().completedStrokes;
    },

    getAllStrokes: () => {
        const { activeStrokes, completedStrokes } = get();
        const all: RemoteStroke[] = [
            ...completedStrokes,
            ...Array.from(activeStrokes.values()),
        ];
        return all;
    },
}));

export function clearDrawingStore(): void {
    useDrawingStore.getState().clearRoom();
}
