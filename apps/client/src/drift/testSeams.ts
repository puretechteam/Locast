/* P4-T05 test-only counter + override: a module-level
 * counter that is bumped every time the `useManualSync`
 * hook completes a local DOM seek successfully, plus a
 * module-level "forced host command" override that the
 * `useManualSync` hook AND the `useDriftSmoother` seam
 * both honor in test mode.
 *
 * The Vite harness cannot observe a real
 * `<video>.currentTime` write (Chromium does not honor
 * `currentTime` assignments on a `<video>` element whose
 * source has not loaded), so the counter is the test's
 * only signal that the local seek ran. The forced host
 * command override lets tests set the host target
 * without going through the playback event bridge.
 *
 * Both the counter and the override are gated on
 * `import.meta.env.MODE === "test"` so they are
 * tree-shaken from production builds.
 */

const localSeekTick: { count: number } = { count: 0 };

export function bumpLocalSeekTick(): number {
    if (import.meta.env.MODE !== "test") return 0;
    localSeekTick.count += 1;
    return localSeekTick.count;
}

export function readLocalSeekTick(): number {
    if (import.meta.env.MODE !== "test") return 0;
    return localSeekTick.count;
}

export function resetLocalSeekTick(): void {
    if (import.meta.env.MODE !== "test") return;
    localSeekTick.count = 0;
}

const forcedHostCommand: {
    payload: {
        mediaPositionMs: number;
        serverTsMs: number;
    } | null;
} = { payload: null };

export function setForcedHostCommand(payload: {
    mediaPositionMs: number;
    serverTsMs: number;
} | null): void {
    if (import.meta.env.MODE !== "test") return;
    forcedHostCommand.payload = payload;
}

export function clearForcedHostCommand(): void {
    setForcedHostCommand(null);
}

export function getForcedHostCommand(): {
    mediaPositionMs: number;
    serverTsMs: number;
} | null {
    if (import.meta.env.MODE !== "test") return null;
    return forcedHostCommand.payload;
}
