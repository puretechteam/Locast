// P5-T02: client-side drawing service.
//
// `DrawingService` is the production wiring of
// `DrawingSession` (src/drawing/drawingSession.ts): the
// session owns the ordering queue, the last-point-wins /
// <=80 Hz DRAW_POINT coalescing, and the guaranteed
// DRAW_END; this class only injects the real transport,
// the `drawing_send` Tauri command:
//
//   DrawingService -> commands.drawingSend -> Rust
//   `drawing_send` (signs DRAW_BEGIN with the keyring key,
//   which never leaves Rust) -> SignalingClient WS ->
//   server `rooms/drawing.rs` -> rebroadcast to the other
//   participants.
//
// The app creates one instance per mounted `DrawingLayer`
// (and so per room) and feeds it through
// `PointerStrokePipeline` (src/drawing/pointerPipeline.ts).
// Pure logic lives in the leaf modules so the Node smoke
// tests (`pnpm -C apps/client smoke:drawing-session`) can
// exercise it without the Tauri bindings.

import { commands } from "./ipc";
import { DrawingSession } from "../drawing/drawingSession";
import type { DrawingSessionOptions } from "../drawing/drawingSession";

export type {
    BeginStrokeOptions,
    StrokeHandle,
    StrokePointPayload,
} from "../drawing/drawingSession";

export class DrawingService extends DrawingSession {
    constructor(opts: DrawingSessionOptions = {}) {
        super((input) => commands.drawingSend(input), opts);
    }
}
