import { create } from "zustand";
import type { Stroke, StrokePoint, StrokeTool } from "../drawing/types";
import type { RemoteStrokeSyncPayload } from "../services/drawingRemote";

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

    /** P5-T03: remove one stroke (in progress or finished) by id. A
     *  no-op if the store does not hold it. */
    removeStroke: (strokeId: string) => void;

    /** Highest drawing sequence number applied in this room (0 until
     *  the first sequenced event or DRAW_SYNC). */
    lastSeq: number;

    /** Admit a drawing event by its sequence number: `false` (ignore
     *  it) when `seq` is not above the last one applied, i.e. a
     *  duplicate or an event a DRAW_SYNC already covers. `0` /
     *  missing means unsequenced and is always admitted. */
    acceptSeq: (seq: number | undefined) => boolean;

    /** Replace the remote drawing state with a DRAW_SYNC snapshot.
     *  Strokes in `localIds` (drawn on this client, which the server
     *  never echoes back) are left to the local canvas. A stroke the
     *  server no longer holds content for keeps the copy already in
     *  the store, if any. */
    applySnapshot: (snapshot: RemoteStrokeSyncPayload, localIds: ReadonlySet<string>) => void;

    getActiveStroke: (strokeId: string) => RemoteStroke | undefined;

    getCompletedStrokes: () => readonly RemoteStroke[];

    getAllStrokes: () => readonly RemoteStroke[];
}

export const useDrawingStore = create<DrawingStoreState>((set, get) => ({
    roomId: null,
    activeStrokes: new Map(),
    completedStrokes: [],
    lastSeq: 0,

    setRoomId: (roomId) => {
        if (roomId !== get().roomId) {
            set({
                roomId,
                activeStrokes: new Map(),
                completedStrokes: [],
                lastSeq: 0,
            });
        }
    },

    acceptSeq: (seq) => {
        if (seq === undefined || seq === 0) return true;
        if (seq <= get().lastSeq) return false;
        set({ lastSeq: seq });
        return true;
    },

    applySnapshot: (snapshot, localIds) => {
        set((state) => {
            const previous = new Map<string, RemoteStroke>();
            for (const s of state.completedStrokes) previous.set(s.id, s);
            for (const s of state.activeStrokes.values()) previous.set(s.id, s);
            const wasActive = (id: string) => state.activeStrokes.has(id);
            const activeStrokes = new Map<string, RemoteStroke>();
            const completedStrokes: RemoteStroke[] = [];
            for (const s of snapshot.strokes) {
                if (localIds.has(s.strokeId)) continue;
                let stroke: RemoteStroke | undefined;
                if (s.begin === null) {
                    stroke = previous.get(s.strokeId);
                } else {
                    stroke = {
                        id: s.strokeId,
                        userId: s.ownerId,
                        tool: s.begin.tool,
                        color: s.begin.color,
                        width: s.begin.width,
                        points: [
                            { x: s.begin.x, y: s.begin.y, pressure: s.begin.pressure, ts: s.begin.tsMs },
                            ...s.points,
                        ],
                        startedAt: s.begin.tsMs,
                        endedAt: s.endTsMs ?? 0,
                    };
                }
                if (stroke === undefined) continue;
                // Still being drawn: an open stroke from the snapshot,
                // or (content gone) our own copy that was still open.
                const open =
                    s.endTsMs === null && (s.begin !== null || wasActive(s.strokeId));
                if (open) {
                    activeStrokes.set(stroke.id, stroke);
                } else {
                    completedStrokes.push(stroke);
                }
            }
            return {
                activeStrokes,
                completedStrokes,
                lastSeq: Math.max(state.lastSeq, snapshot.seq),
            };
        });
    },

    beginStroke: ({ strokeId, userId, tool, color, width, x, y, pressure, tsMs }) => {
        // A repeated BEGIN for a stroke already held is a duplicate:
        // replacing it would throw its points away.
        const held = get();
        if (held.activeStrokes.has(strokeId) || held.completedStrokes.some((s) => s.id === strokeId)) {
            return;
        }
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

    removeStroke: (strokeId) => {
        set((state) => {
            const inActive = state.activeStrokes.has(strokeId);
            const inCompleted = state.completedStrokes.some((s) => s.id === strokeId);
            if (!inActive && !inCompleted) return state;
            const activeStrokes = new Map(state.activeStrokes);
            activeStrokes.delete(strokeId);
            return {
                activeStrokes,
                completedStrokes: state.completedStrokes.filter((s) => s.id !== strokeId),
            };
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
