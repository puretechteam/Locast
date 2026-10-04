// Recovery from dropped drawing events: the store's sequence gate,
// duplicate-safe BEGIN, and DRAW_SYNC snapshot application. Runs the
// production `useDrawingStore` under Node.
//
// Run: node --experimental-strip-types --no-warnings src/drawing/drawingSync.smoke.ts

import { useDrawingStore } from "../stores/useDrawingStore.ts";
import type { RemoteStrokeSyncPayload, RemoteSyncStroke } from "../services/drawingRemote.ts";

let failures = 0;

function check(name: string, cond: boolean, detail?: string): void {
    if (cond) {
        process.stdout.write(`  ok ${name}\n`);
    } else {
        process.stdout.write(`  FAIL ${name}${detail === undefined ? "" : ` (${detail})`}\n`);
        failures++;
    }
}

const store = () => useDrawingStore.getState();
const ids = () => store().getAllStrokes().map((s) => s.id);

function begin(id: string, x = 0.1): void {
    store().beginStroke({
        strokeId: id,
        userId: "remote",
        tool: "pen",
        color: "#fff",
        width: 2,
        x,
        y: 0.1,
        pressure: 0.5,
        tsMs: 1,
    });
}

function synced(id: string, points: number, ended: boolean): RemoteSyncStroke {
    return {
        strokeId: id,
        ownerId: "remote",
        begin: { tool: "pen", color: "#abc", width: 3, x: 0.5, y: 0.5, pressure: 0.5, tsMs: 1 },
        points: Array.from({ length: points }, (_, i) => ({ x: i / 10, y: 0.2, pressure: 0.5, ts: 2 + i })),
        endTsMs: ended ? 99 : null,
    };
}

process.stdout.write("sequence gate\n");
store().setRoomId("room-1");
check("unsequenced events are admitted", store().acceptSeq(undefined) && store().acceptSeq(0));
check("a new seq is admitted", store().acceptSeq(3));
check("the same seq again is a duplicate", !store().acceptSeq(3));
check("an older seq is ignored", !store().acceptSeq(2));
check("the next seq is admitted", store().acceptSeq(4));
check("lastSeq follows", store().lastSeq === 4);

process.stdout.write("duplicate BEGIN\n");
store().clearRoom();
begin("a");
store().appendPoint({ strokeId: "a", x: 0.2, y: 0.2, pressure: 0.5, tsMs: 2 });
begin("a", 0.9);
const a = store().getActiveStroke("a");
check("a repeated BEGIN keeps the stroke's points", a !== undefined && a.points.length === 2 && a.points[0]?.x === 0.1);
store().endStroke({ strokeId: "a", tsMs: 3 });
begin("a");
check("a BEGIN for a finished stroke is ignored", ids().filter((x) => x === "a").length === 1);

process.stdout.write("snapshot\n");
store().setRoomId("room-2");
begin("kept");
store().endStroke({ strokeId: "kept", tsMs: 5 });
begin("undone-elsewhere");
store().endStroke({ strokeId: "undone-elsewhere", tsMs: 6 });
begin("open-but-cleared");
const snapshot: RemoteStrokeSyncPayload = {
    roomId: "room-2",
    seq: 10,
    strokes: [
        // Content no longer on the server: keep our copy.
        { strokeId: "kept", ownerId: "remote", begin: null, points: [], endTsMs: null },
        synced("missed", 3, true),
        synced("mine", 2, true),
        synced("in-progress", 1, false),
        // Content gone AND we never had it: nothing to draw.
        { strokeId: "unknown", ownerId: "remote", begin: null, points: [], endTsMs: null },
    ],
};
store().applySnapshot(snapshot, new Set(["mine"]));
check(
    "the canvas is exactly the server's (minus the local stroke)",
    JSON.stringify(ids().sort()) === JSON.stringify(["in-progress", "kept", "missed"]),
    ids().join(","),
);
check("a stroke missed entirely is restored with its points", store().getCompletedStrokes().find((s) => s.id === "missed")?.points.length === 4);
check("an in-progress stroke stays open", store().getActiveStroke("in-progress") !== undefined);
check("the snapshot's seq becomes the floor", store().lastSeq === 10);
check("events the snapshot covers are ignored", !store().acceptSeq(10) && !store().acceptSeq(7));
check("later events are applied", store().acceptSeq(11));
store().appendPoint({ strokeId: "in-progress", x: 0.9, y: 0.9, pressure: 0.5, tsMs: 50 });
check("the open stroke continues live", store().getActiveStroke("in-progress")?.points.length === 3);

process.stdout.write("snapshot without content keeps an open copy open\n");
store().setRoomId("room-4");
begin("long");
store().applySnapshot(
    { roomId: "room-4", seq: 5, strokes: [{ strokeId: "long", ownerId: "remote", begin: null, points: [], endTsMs: null }] },
    new Set(),
);
check("the open copy stays open", store().getActiveStroke("long") !== undefined);
store().appendPoint({ strokeId: "long", x: 0.4, y: 0.4, pressure: 0.5, tsMs: 9 });
check("and keeps receiving points", store().getActiveStroke("long")?.points.length === 2);

process.stdout.write("room change\n");
store().setRoomId("room-3");
check("a new room starts with no strokes and seq 0", ids().length === 0 && store().lastSeq === 0);

if (failures > 0) {
    process.stdout.write(`${failures} failure(s)\n`);
    process.exit(1);
}
process.stdout.write("all drawing sync checks passed\n");
