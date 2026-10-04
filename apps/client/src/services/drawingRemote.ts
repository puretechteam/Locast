import type { StrokeTool } from "../drawing/types";
import type {
    StrokeBeginEvent,
    StrokePointEvent,
    StrokeEndEvent,
    StrokeUndoEvent,
    StrokeClearEvent,
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
}

export interface RemoteStrokePointPayload {
    roomId: string;
    senderId: string;
    strokeId: string;
    x: number;
    y: number;
    pressure: number;
    tsMs: number;
}

export interface RemoteStrokeEndPayload {
    roomId: string;
    senderId: string;
    strokeId: string;
    tsMs: number;
}

/** P5-T03: an accepted DRAW_UNDO. `senderId` is the server-stamped
 *  ACTOR (who undid), which can differ from the stroke's owner. */
export interface RemoteStrokeUndoPayload {
    roomId: string;
    senderId: string;
    strokeId: string;
}

/** P5-T03: an accepted DRAW_CLEAR. `senderId` is the actor. */
export interface RemoteStrokeClearPayload {
    roomId: string;
    senderId: string;
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
    };
}

export function fromStrokeEndEvent(ev: StrokeEndEvent): RemoteStrokeEndPayload {
    return {
        roomId: ev.room_id,
        senderId: ev.sender_id,
        strokeId: ev.stroke_id,
        tsMs: ev.ts_ms,
    };
}

export function fromStrokeUndoEvent(ev: StrokeUndoEvent): RemoteStrokeUndoPayload {
    return {
        roomId: ev.room_id,
        senderId: ev.sender_id,
        strokeId: ev.stroke_id,
    };
}

export function fromStrokeClearEvent(ev: StrokeClearEvent): RemoteStrokeClearPayload {
    return {
        roomId: ev.room_id,
        senderId: ev.sender_id,
    };
}
