// P4-T07: runnable smoke test for the per-sender
// monotonic_seq dedup math.
//
// The repo does not yet have a Vitest test runner (P0-T04
// TODO). Node 22.6+ has stable `--experimental-strip-types`
// support for running plain `.ts` files; we use it here so
// the math is exercised end-to-end without a bundler /
// test framework dependency.
//
// Run via: `pnpm -C apps/client smoke:dedup` (script defined
// in package.json). Intended for CI and local verification
// of the acceptance scenarios in `docs/ROADMAP.md` P4-T07:
//   "a unit test replays a stream with a duplicate seq, a
//    gap, and an out-of-order seq; duplicates are dropped,
//    the gap is filled within 5 s, the out-of-order SEEK
//    is dropped."

import {
    BUFFER_TIMEOUT_MS,
    clearDedupState,
    evaluateDedup,
    initialDedupState,
    tickDedup,
} from "./dedup.ts";

let failures = 0;

function check(name: string, cond: boolean): void {
    if (cond) {
        process.stdout.write(`  ok ${name}\n`);
    } else {
        process.stdout.write(`  FAIL ${name}\n`);
        failures++;
    }
}

interface MiniEvent {
    sender_id: string;
    monotonic_seq: number;
    /** tag to make assertions easier to read */
    tag: string;
    server_seq?: number;
    media_position_ms?: number;
    kind?: "play" | "pause" | "seek";
    room_id?: string;
    server_ts_ms?: number;
}

// Minimal shape that satisfies evaluateDedup's generic
// constraint (`{ sender_id; monotonic_seq }`). The store
// passes full PlaybackStateEvent objects; the smoke test
// only needs the two indexed fields.
type Ev = MiniEvent;

process.stdout.write("dedup math smoke\n");

// ----- bootstrap: seq == 1 from a fresh sender applies -----
process.stdout.write("first seq from a sender (seq == 1)\n");
{
    const s0 = initialDedupState<Ev>();
    const r1 = evaluateDedup(s0, { sender_id: "host", monotonic_seq: 1, tag: "first-play" }, 1000);
    check("seq=1 first event -> apply", r1.decision.kind === "apply");
    if (r1.decision.kind === "apply") {
        check("apply decision includes senderId", r1.decision.senderId === "host");
        check("apply decision includes seq", r1.decision.seq === 1);
    }
    check("state advanced lastAppliedSeq", r1.next.bySender.get("host")?.lastAppliedSeq === 1);
}

// ----- duplicate seq (<= last) is dropped -----
process.stdout.write("duplicate seq\n");
{
    let s = initialDedupState<Ev>();
    s = evaluateDedup(s, { sender_id: "host", monotonic_seq: 1, tag: "a" }, 1000).next;
    const dup = evaluateDedup(s, { sender_id: "host", monotonic_seq: 1, tag: "a-dup" }, 1500);
    check("duplicate seq=1 -> drop", dup.decision.kind === "drop");
    if (dup.decision.kind === "drop") {
        check("drop reason is duplicate", dup.decision.reason === "duplicate");
    }
    check("duplicate did not advance state", dup.next.bySender.get("host")?.lastAppliedSeq === 1);

    // A seq of 0 (theoretical "older than any valid seq")
    // is also treated as duplicate from a brand-new sender.
    const s0 = initialDedupState<Ev>();
    const r = evaluateDedup(s0, { sender_id: "alice", monotonic_seq: 0, tag: "bad" }, 1000);
    check("seq=0 from new sender -> drop", r.decision.kind === "drop");
}

// ----- gap: seq > last + 1 is buffered, then drained -----
process.stdout.write("gap buffering and draining\n");
{
    let s = initialDedupState<Ev>();
    s = evaluateDedup(s, { sender_id: "host", monotonic_seq: 1, tag: "play" }, 1000).next;
    // Jump straight to seq=3 (gap).
    const r2 = evaluateDedup(s, { sender_id: "host", monotonic_seq: 3, tag: "seek-30s" }, 2000);
    check("seq=3 with last=1 -> buffer", r2.decision.kind === "buffer");
    check(
        "buffered pending.seq == 3",
        r2.next.bySender.get("host")?.pending?.seq === 3,
    );

    // The missing seq=2 arrives: should fill the gap AND
    // drain the parked seq=3 in the same call.
    const r3 = evaluateDedup(r2.next, { sender_id: "host", monotonic_seq: 2, tag: "play-after-pause" }, 2500);
    check("seq=2 filling gap -> apply (drain)", r3.decision.kind === "apply");
    if (r3.decision.kind === "apply") {
        check(
            "apply decision carries drained event",
            "drain" in r3.decision && r3.decision.drain?.event.tag === "seek-30s",
        );
        check(
            "drain.seq == 3",
            "drain" in r3.decision && r3.decision.drain?.seq === 3,
        );
    }
    check(
        "state advanced to drained seq (3)",
        r3.next.bySender.get("host")?.lastAppliedSeq === 3,
    );
    check(
        "pending cleared after drain",
        r3.next.bySender.get("host")?.pending === null,
    );
}

// ----- out-of-order: a wider-gap event arriving after a
// tighter-gap is parked drops the inbound as out-of-order -----
process.stdout.write("out-of-order SEEK\n");
{
    let s = initialDedupState<Ev>();
    s = evaluateDedup(s, { sender_id: "host", monotonic_seq: 1, tag: "a" }, 1000).next;
    // Park a seq=3 (gap = 1).
    const r2 = evaluateDedup(s, { sender_id: "host", monotonic_seq: 3, tag: "seek-tight-gap" }, 2000);
    check("seq=3 buffered (tight gap)", r2.decision.kind === "buffer");

    // A seq=2 arrives AFTER seq=3 was parked. Two sub-cases
    // by design:
    //   - case A: it FILLS the gap exactly (seq=2 with
    //     pending.seq=3) -> the parked seq=3 drains in the
    //     same call (already covered by the previous test).
    //   - case B: a NEW gap event with an even newer seq
    //     arrives (e.g. seq=5) BEFORE the parked seq=3 was
    //     drained -> the parked seq=3 is replaced, the older
    //     parked event is dropped as out-of-order, the
    //     newer event is buffered in its place.
    const r3 = evaluateDedup(r2.next, { sender_id: "host", monotonic_seq: 5, tag: "seek-wider-gap" }, 2500);
    check("seq=5 with pending seq=3 -> buffer", r3.decision.kind === "buffer");
    check(
        "buffered pending.seq == 5",
        r3.next.bySender.get("host")?.pending?.seq === 5,
    );

    // A SECOND seq=3 arrives (the original tight-gap event
    // is being replayed from a separate channel after the
    // wider-gap took its slot). It must be dropped as
    // out-of-order: seq=3 is older than the parked seq=5.
    const r4 = evaluateDedup(r3.next, { sender_id: "host", monotonic_seq: 3, tag: "seek-tight-gap-replay" }, 3000);
    check("seq=3 after seq=5 parked -> drop", r4.decision.kind === "drop");
    if (r4.decision.kind === "drop") {
        check(
            "drop reason is out_of_order",
            r4.decision.reason === "out_of_order",
        );
    }
    check(
        "pending still holds seq=5",
        r4.next.bySender.get("host")?.pending?.seq === 5,
    );
}

// ----- 5 s timeout: held event is force-applied -----
process.stdout.write("buffer timeout (5 s)\n");
{
    let s = initialDedupState<Ev>();
    s = evaluateDedup(s, { sender_id: "host", monotonic_seq: 1, tag: "a" }, 1000).next;
    const parked = evaluateDedup(s, { sender_id: "host", monotonic_seq: 5, tag: "held-seek" }, 2000);
    check("parked for timeout test", parked.decision.kind === "buffer");

    // Advance the clock past BUFFER_TIMEOUT_MS without
    // filling the gap.
    const ticked = tickDedup(parked.next, 2000 + BUFFER_TIMEOUT_MS);
    check("1 event expired from tick", ticked.expired.length === 1);
    if (ticked.expired[0]) {
        check("expired tag is the held SEEK", ticked.expired[0].event.tag === "held-seek");
        check("expired seq == 5", ticked.expired[0].seq === 5);
    }
    check(
        "state advanced lastAppliedSeq to 5 after force-apply",
        ticked.next.bySender.get("host")?.lastAppliedSeq === 5,
    );
    check("pending cleared after force-apply", ticked.next.bySender.get("host")?.pending === null);

    // A duplicate of seq=5 arriving AFTER the force-apply
    // must still drop.
    const dup = evaluateDedup(ticked.next, { sender_id: "host", monotonic_seq: 5, tag: "dup" }, 9000);
    check("post-timeout duplicate -> drop", dup.decision.kind === "drop");
}

// ----- tick before timeout: no expiry -----
process.stdout.write("tick before timeout\n");
{
    let s = initialDedupState<Ev>();
    s = evaluateDedup(s, { sender_id: "host", monotonic_seq: 1, tag: "a" }, 1000).next;
    const parked = evaluateDedup(s, { sender_id: "host", monotonic_seq: 3, tag: "b" }, 2000);
    const ticked = tickDedup(parked.next, 2000 + BUFFER_TIMEOUT_MS - 1);
    check("no events expired", ticked.expired.length === 0);
    check(
        "state unchanged after no-op tick",
        ticked.next.bySender.get("host")?.pending?.seq === 3,
    );
}

// ----- multiple senders are independent -----
process.stdout.write("multi-sender independence\n");
{
    let s = initialDedupState<Ev>();
    s = evaluateDedup(s, { sender_id: "host", monotonic_seq: 1, tag: "h1" }, 1000).next;
    s = evaluateDedup(s, { sender_id: "viewer", monotonic_seq: 1, tag: "v1" }, 1100).next;
    const dupHost = evaluateDedup(s, { sender_id: "host", monotonic_seq: 1, tag: "h-dup" }, 1200);
    const dupViewer = evaluateDedup(s, { sender_id: "viewer", monotonic_seq: 1, tag: "v-dup" }, 1200);
    check("host duplicate -> drop", dupHost.decision.kind === "drop");
    check("viewer duplicate -> drop", dupViewer.decision.kind === "drop");
    check(
        "host lastAppliedSeq preserved",
        dupHost.next.bySender.get("host")?.lastAppliedSeq === 1,
    );
    check(
        "viewer lastAppliedSeq preserved",
        dupViewer.next.bySender.get("viewer")?.lastAppliedSeq === 1,
    );
}

// ----- clearDedupState empties the map -----
process.stdout.write("clearDedupState\n");
{
    let s = initialDedupState<Ev>();
    s = evaluateDedup(s, { sender_id: "host", monotonic_seq: 1, tag: "a" }, 1000).next;
    const cleared = clearDedupState<Ev>();
    check("cleared state has empty map", cleared.bySender.size === 0);
    // After clearing, the same sender can re-bootstrap with
    // seq=1 (simulates a fresh room).
    const reapply = evaluateDedup(cleared, { sender_id: "host", monotonic_seq: 1, tag: "fresh" }, 2000);
    check("post-clear seq=1 reapply -> apply", reapply.decision.kind === "apply");
}

if (failures > 0) {
    process.stdout.write(`\n${failures} failure(s)\n`);
    process.exit(1);
} else {
    process.stdout.write("\nall checks passed\n");
}