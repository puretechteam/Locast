// Unit test for the drawing store's room scoping. `resetRoomScopedStores`
// relies on `setRoomId(null)` to drop a left room's strokes and sequence
// counter, including when the same room id is joined again.

import { useDrawingStore } from "./useDrawingStore.ts";

let failures = 0;

function check(name: string, cond: boolean): void {
    if (cond) {
        process.stdout.write(`  ok ${name}\n`);
    } else {
        process.stdout.write(`  FAIL ${name}\n`);
        failures++;
    }
}

const s = useDrawingStore;
s.getState().setRoomId("room-a");
s.getState().acceptSeq(7);
s.setState({ completedStrokes: [{ id: "stroke-1" } as never] });
check("a stroke and a sequence number are recorded", s.getState().completedStrokes.length === 1 && s.getState().lastSeq === 7);

// Re-joining the same id without a reset keeps the old state (the bug the
// reset exists to prevent).
s.getState().setRoomId("room-a");
check("an unchanged room id is a no-op", s.getState().completedStrokes.length === 1);

s.getState().setRoomId(null);
check("setRoomId(null) clears strokes", s.getState().completedStrokes.length === 0);
check("setRoomId(null) resets the sequence counter", s.getState().lastSeq === 0);
check("setRoomId(null) clears the room id", s.getState().roomId === null);

s.getState().setRoomId("room-a");
check("the same room joined again starts empty and accepts seq 1", s.getState().acceptSeq(1));

if (failures > 0) {
    process.stdout.write(`\n${failures} failure(s)\n`);
    process.exit(1);
}
process.stdout.write("\nall checks passed\n");
