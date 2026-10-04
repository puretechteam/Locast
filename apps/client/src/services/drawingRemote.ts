import type { StrokeTool } from "../drawing/types";
import type {
    StrokeBeginEvent,
    StrokePointEvent,
    StrokeEndEvent,
    StrokeUndoEvent,
    StrokeClearEvent,
    StrokeSyncEvent,
} from "../bindings/index";

export interface RemoteStrokeBeginPayload {
    roomId: string;
    senderId: string;
    strokeId: string;
    tool: StrokeTool;
    color: string;
    width: number;
    x: number;
    y: number;
    pressure: number;
    tsMs: number;
    /** Drawing sequence number; 0 when the event carried none. */
    seq: number;
}

export interface RemoteStrokePointPayload {
    roomId: string;
    senderId: string;
    strokeId: string;
    x: number;
    y: number;
    pressure: number;
    tsMs: number;
    /** Drawing sequence number; 0 when the event carried none. */
    seq: number;
}

export interface RemoteStrokeEndPayload {
    roomId: string;
    senderId: string;
    strokeId: string;
    tsMs: number;
    /** Drawing sequence number; 0 when the event carried none. */
    seq: number;
}

/** P5-T03: an accepted DRAW_UNDO. `senderId` is the server-stamped
 *  ACTOR (who undid), which can differ from the stroke's owner. */
export interface RemoteStrokeUndoPayload {
    roomId: string;
    senderId: string;
    strokeId: string;
    /** Drawing sequence number; 0 when the event carried none. */
    seq: number;
}

/** P5-T03: an accepted DRAW_CLEAR. `senderId` is the actor. */
export interface RemoteStrokeClearPayload {
    roomId: string;
    senderId: string;
    /** Drawing sequence number; 0 when the event carried none. */
    seq: number;
}

/** One stroke of a DRAW_SYNC snapshot. `begin` is `null` when the
 *  server no longer holds the stroke's content (keep the copy on
 *  screen, if any). */
export interface RemoteSyncStroke {
    strokeId: string;
    ownerId: string;
    begin: {
        tool: StrokeTool;
        color: string;
        width: number;
        x: number;
        y: number;
        pressure: number;
        tsMs: number;
    } | null;
    points: Array<{ x: number; y: number; pressure: number; ts: number }>;
    endTsMs: number | null;
}

/** A DRAW_SYNC: the room's whole drawing state as of `seq`. */
export interface RemoteStrokeSyncPayload {
    roomId: string;
    seq: number;
    strokes: RemoteSyncStroke[];
}

export function fromStrokeBeginEvent(ev: StrokeBeginEvent): RemoteStrokeBeginPayload {
    return {
        roomId: ev.room_id,
        senderId: ev.sender_id,
        strokeId: ev.stroke_id,
        tool: ev.tool as StrokeTool,
        color: ev.color,
        width: ev.width,
        x: ev.x,
        y: ev.y,
        pressure: ev.pressure,
        tsMs: ev.ts_ms,
        seq: ev.seq ?? 0,
    };
}

export function fromStrokePointEvent(ev: StrokePointEvent): RemoteStrokePointPayload {
    return {
        roomId: ev.room_id,
        senderId: ev.sender_id,
        strokeId: ev.stroke_id,
        x: ev.x,
        y: ev.y,
        pressure: ev.pressure,
        tsMs: ev.ts_ms,
        seq: ev.seq ?? 0,
    };
}

export function fromStrokeEndEvent(ev: StrokeEndEvent): RemoteStrokeEndPayload {
    return {
        roomId: ev.room_id,
        senderId: ev.sender_id,
        strokeId: ev.stroke_id,
        tsMs: ev.ts_ms,
        seq: ev.seq ?? 0,
    };
}

export function fromStrokeUndoEvent(ev: StrokeUndoEvent): RemoteStrokeUndoPayload {
    return {
        roomId: ev.room_id,
        senderId: ev.sender_id,
        strokeId: ev.stroke_id,
        seq: ev.seq ?? 0,
    };
}

export function fromStrokeClearEvent(ev: StrokeClearEvent): RemoteStrokeClearPayload {
    return {
        roomId: ev.room_id,
        senderId: ev.sender_id,
        seq: ev.seq ?? 0,
    };
}

export function fromStrokeSyncEvent(ev: StrokeSyncEvent): RemoteStrokeSyncPayload {
    return {
        roomId: ev.room_id,
        seq: ev.seq,
        strokes: ev.strokes.map((s) => ({
            strokeId: s.stroke_id,
            ownerId: s.owner_id,
            begin:
                s.begin === null
                    ? null
                    : {
                          tool: s.begin.tool as StrokeTool,
                          color: s.begin.color,
                          width: s.begin.width,
                          x: s.begin.x,
                          y: s.begin.y,
                          pressure: s.begin.pressure,
                          tsMs: s.begin.ts_ms,
                      },
            points: s.points.map((p) => ({ x: p.x, y: p.y, pressure: p.pressure, ts: p.ts_ms })),
            endTsMs: s.end_ts_ms,
        })),
    };
}
