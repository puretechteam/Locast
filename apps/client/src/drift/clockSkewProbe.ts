// apps/client/src/drift/clockSkewProbe.ts
//
// The production `probeOnce` for `useClockSkew`: one SKEW_PROBE round trip
// through the Tauri `clock_skew_probe` command. Defined at module level so it
// has a stable identity: `useClockSkew` restarts its 60 s cadence (and fires a
// 4-sample burst) whenever `probeOnce` changes, so an inline arrow would do
// that on every render of the room page.

import { commands } from "../services/ipc";
import type { SkewProbeFn } from "./useClockSkew";

export const probeClockSkewOnce: SkewProbeFn = async () => {
    const sample = await commands.clockSkewProbe();
    // Not signed in yet, or a transport without the command: no sample, which
    // `useClockSkew` counts as a rejected one.
    if (
        sample === null ||
        typeof sample !== "object" ||
        typeof sample.t0_local_ms !== "number" ||
        typeof sample.t3_local_ms !== "number" ||
        typeof sample.server_ts_ms !== "number"
    ) {
        return null;
    }
    return {
        t0_local_ms: sample.t0_local_ms,
        t3_local_ms: sample.t3_local_ms,
        server_ts_ms: sample.server_ts_ms,
    };
};
