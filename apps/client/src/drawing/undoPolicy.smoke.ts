// P5-T03: smoke test for undo / clear on the client.
//
// Run via `pnpm -C apps/client smoke:drawing-undo`. Plain Node
// `--experimental-strip-types`, no DOM, no Tauri. It runs the REAL
// `planUndo` / `pickUndoTarget` (what Ctrl+Z and the Undo button
// decide) and the REAL remote stroke store (`useDrawingStore`, which
// the `drawing://undo` / `drawing://clear` event handlers call), and
// proves:
//
//   (a) with UNDO_OWN / UNDO_ANY, undo targets the newest FINISHED
//       local stroke that has no undo on its way, so repeated
//       Ctrl+Z walks back through the strokes;
//   (b) a stroke still being drawn is never an undo target;
//   (c) without the capability, undo keeps its old local-only
//       meaning;
//   (e) an undo the server never answers is given up on after the
//       timeout: the stroke is skipped, the next-older one becomes
//       the target, and nothing is retried forever; an echoed undo
//       removes the stroke normally;
//   (d) the store drops a stroke by id (active or completed),
//       ignores an unknown id without changing state, and a clear
//       empties it; replays are harmless.

import { pickUndoTarget, planUndo, UndoTracker } from "./undoPolicy.ts";
import type { UndoCandidate } from "./undoPolicy.ts";
import { useDrawingStore } from "../stores/useDrawingStore.ts";

let failures = 0;

function check(name: string, cond: boolean, detail?: string): void {
    if (cond) {
        process.stdout.write(`  ok ${name}\n`);
    } else {
        process.stdout.write(`  FAIL ${name}${detail === undefined ? "" : ` (${detail})`}\n`);
        failures++;
    }
}

const done = (id: string): UndoCandidate => ({ id, endedAt: 10 });
const open = (id: string): UndoCandidate => ({ id, endedAt: 0 });

process.stdout.write("(a) undo targets the newest finished stroke without an undo on its way\n");
{
    const strokes = [done("s1"), done("s2"), done("s3")];
    check("newest finished stroke first", pickUndoTarget(strokes, new Set()) === "s3");
    check(
        "skips a stroke whose undo is already on its way",
        pickUndoTarget(strokes, new Set(["s3"])) === "s2",
    );
    check(
        "walks back through all of them",
        pickUndoTarget(strokes, new Set(["s3", "s2"])) === "s1",
    );
    check(
        "nothing left to undo",
        pickUndoTarget(strokes, new Set(["s1", "s2", "s3"])) === null,
    );
    check("an empty canvas has nothing to undo", pickUndoTarget([], new Set()) === null);
    const plan = planUndo({ canUndoOwn: true, strokes, pending: new Set(["s3"]) });
    check("capable user: remote plan for s2", plan.kind === "remote" && plan.strokeId === "s2");
    check(
        "capable user, nothing left: no plan",
        planUndo({ canUndoOwn: true, strokes: [], pending: new Set() }).kind === "none",
    );
}

process.stdout.write("(b) a stroke in progress is never undone\n");
{
    const strokes = [done("s1"), open("s2")];
    check("picks the finished stroke, not the open one", pickUndoTarget(strokes, new Set()) === "s1");
    check("only an open stroke: nothing to undo", pickUndoTarget([open("s1")], new Set()) === null);
}

process.stdout.write("(c) no capability: local-only undo as before\n");
{
    const strokes = [done("s1"), open("s2")];
    check(
        "removes the newest local stroke locally",
        planUndo({ canUndoOwn: false, strokes, pending: new Set() }).kind === "local",
    );
    check(
        "an empty canvas has nothing to undo either way",
        planUndo({ canUndoOwn: false, strokes: [], pending: new Set() }).kind === "none",
    );
}

process.stdout.write("(d) remote store: remove by id, clear\n");
{
    const store = useDrawingStore;
    const base = { userId: "u1", tool: "pen" as const, color: "#fff", width: 2, x: 0.1, y: 0.1, pressure: 0, tsMs: 1 };
    store.getState().setRoomId("room-1");
    store.getState().beginStroke({ strokeId: "a", ...base });
    store.getState().endStroke({ strokeId: "a", tsMs: 2 });
    store.getState().beginStroke({ strokeId: "b", ...base });
    store.getState().endStroke({ strokeId: "b", tsMs: 2 });
    store.getState().beginStroke({ strokeId: "c", ...base });
    const ids = () => store.getState().getAllStrokes().map((s) => s.id).sort().join(",");
    check("two finished strokes and one in progress", ids() === "a,b,c", ids());

    store.getState().removeStroke("a");
    check("a finished stroke is dropped by id", ids() === "b,c", ids());
    store.getState().removeStroke("c");
    check("a stroke still in progress is dropped by id", ids() === "b", ids());

    const before = store.getState();
    store.getState().removeStroke("never-seen");
    check(
        "an unknown id changes nothing (same state object)",
        store.getState() === before,
    );
    store.getState().removeStroke("a");
    check("a replayed undo is harmless", ids() === "b", ids());

    // Late DRAW_POINT / DRAW_END for a stroke that was removed are ignored.
    store.getState().appendPoint({ strokeId: "c", x: 0.5, y: 0.5, pressure: 0, tsMs: 3 });
    store.getState().endStroke({ strokeId: "c", tsMs: 4 });
    check("late frames for a removed stroke do not bring it back", ids() === "b", ids());

    store.getState().beginStroke({ strokeId: "d", ...base });
    store.getState().clearRoom();
    check("clear empties finished and in-progress strokes", ids() === "", ids());
    check("clear keeps the room binding", store.getState().roomId === "room-1");
    store.getState().clearRoom();
    check("a replayed clear is harmless", ids() === "");
    // Strokes drawn after a clear are normal again.
    store.getState().beginStroke({ strokeId: "e", ...base });
    store.getState().endStroke({ strokeId: "e", tsMs: 5 });
    check("a stroke after the clear is kept", ids() === "e", ids());
}

process.stdout.write("(e) unanswered undo: give up, then the next-older stroke\n");
{
    // A fake clock/scheduler so the 5 s timeout is deterministic.
    let now = 0;
    const timers = new Map<number, { at: number; cb: () => void }>();
    let nextId = 1;
    const tracker = new UndoTracker(
        (cb, ms) => {
            const id = nextId++;
            timers.set(id, { at: now + ms, cb });
            return id as unknown as ReturnType<typeof setTimeout>;
        },
        (id) => {
            timers.delete(id as unknown as number);
        },
        5000,
    );
    const advance = (ms: number): void => {
        now += ms;
        for (const [id, t] of [...timers]) {
            if (t.at <= now) {
                timers.delete(id);
                t.cb();
            }
        }
    };
    const strokes = [done("s1"), done("s2"), done("s3")];
    const target = (): string | null =>
        pickUndoTarget(strokes, tracker.pending, tracker.gaveUp);

    check("first Ctrl+Z targets the newest stroke", target() === "s3");
    tracker.markSent("s3");
    check("while pending, the next Ctrl+Z targets s2", target() === "s2");
    advance(4999);
    check("just before the timeout s3 is still pending", tracker.pending.has("s3"));
    advance(1);
    check("at the timeout s3 leaves pending", !tracker.pending.has("s3"));
    check("... and is given up on", tracker.gaveUp.has("s3"));
    check("a non-echoed target is skipped: s2 is next", target() === "s2");

    // No infinite retry: s2 is also never answered, then s1, then nothing.
    tracker.markSent("s2");
    advance(5000);
    check("s2 skipped after its timeout, s1 is next", target() === "s1");
    tracker.markSent("s1");
    advance(5000);
    check("every unanswered stroke is skipped: no target left", target() === null);
    check(
        "planUndo agrees: nothing to undo (no retry of the same id)",
        planUndo({
            canUndoOwn: true,
            strokes,
            pending: tracker.pending,
            gaveUp: tracker.gaveUp,
        }).kind === "none",
    );

    // An echoed undo removes the stroke normally and clears its marks.
    tracker.reset();
    tracker.markSent("s3");
    tracker.confirmed("s3");
    check("an echo clears pending", !tracker.pending.has("s3"));
    check("an echo cancels the give-up timer", timers.size === 0);
    advance(10_000);
    check("... so s3 is never marked given-up", !tracker.gaveUp.has("s3"));
    const remaining = [done("s1"), done("s2")];
    check(
        "after the echo the stroke is gone and s2 is the target",
        pickUndoTarget(remaining, tracker.pending, tracker.gaveUp) === "s2",
    );

    // A late echo for a given-up stroke drops it from the gave-up set.
    tracker.reset();
    tracker.markSent("s2");
    advance(5000);
    check("s2 given up", tracker.gaveUp.has("s2"));
    tracker.confirmed("s2");
    check("a late echo drops it from the gave-up set", !tracker.gaveUp.has("s2"));

    // Send failure allows a retry; clear-all / room change forgets all.
    tracker.reset();
    tracker.markSent("s3");
    tracker.sendFailed("s3");
    check("a failed send can be retried at once", pickUndoTarget(strokes, tracker.pending, tracker.gaveUp) === "s3");
    tracker.markSent("s3");
    advance(5000);
    check("given up", tracker.gaveUp.has("s3"));
    tracker.reset();
    check("reset (clear-all / room change) empties both sets", tracker.pending.size === 0 && tracker.gaveUp.size === 0);
    check("reset cancels outstanding timers", timers.size === 0);
}

if (failures > 0) {
    process.stdout.write(`\n${failures} failure(s)\n`);
    process.exit(1);
} else {
    process.stdout.write("\nall checks passed\n");
}
