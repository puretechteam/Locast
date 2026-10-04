// P5-T02: drawing constants. Extracted into a leaf
// module so the Node-based smoke tests can import the
// cap without dragging in the React / Tauri bindings
// (`services/drawing.ts` imports from `./ipc` which is
// Vite-only).

/** P5-T02: maximum DRAW_POINT messages per second per
 *  local user. Architecture §15.8 allows at most 120 Hz;
 *  the client uses 80 Hz because the server rate-limits
 *  each connection to 100 msg/s sustained, and a higher
 *  point rate plus BEGIN/END/presence traffic could
 *  exhaust that budget during a long stroke. The
 *  React-side coalescer's natural ceiling is the display
 *  refresh rate (typically 60 Hz via
 *  `requestAnimationFrame`). */
export const MAX_DRAW_POINT_HZ = 80;