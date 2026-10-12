// apps/client/src/services/hostNativeControls.ts
//
// Lets the host drive the room with the <video> element's own controls
// (the seek bar, the play/pause button, the space key), not only with the
// Play / Pause / "Seek 60s" buttons.
//
// The hard part is the feedback loop: the app itself moves the host's
// <video> (a command applied before it is sent, the Sync button, an event
// from a previous host), and each of those fires the same DOM events as a
// user gesture. Every programmatic change therefore goes through the
// helpers below, which raise a one-shot flag that the matching DOM event
// consumes. An event with no flag raised is the user, and is reported to
// `onUserChange`.
//
// Pure DOM, no stores and no IPC, so it can be unit tested with a fake
// element (`hostNativeControls.smoke.ts`).

export type NativeControlKind = "play" | "pause" | "seek";

interface Flags {
    seek: boolean;
    play: boolean;
    pause: boolean;
}

const flags = new WeakMap<HTMLVideoElement, Flags>();

function flagsFor(v: HTMLVideoElement): Flags {
    let f = flags.get(v);
    if (f === undefined) {
        f = { seek: false, play: false, pause: false };
        flags.set(v, f);
    }
    return f;
}

/** HTMLMediaElement.HAVE_METADATA: below this a `currentTime` write is only
 *  remembered for later and no `seeked` event follows it. */
const HAVE_METADATA = 1;

/** Move `v` to `seconds` unless it is already within `toleranceSec` of it.
 *  The resulting `seeked` event is the app's own, not the user's. */
export function seekProgrammatically(
    v: HTMLVideoElement,
    seconds: number,
    toleranceSec = 0.01,
): boolean {
    if (Math.abs(v.currentTime - seconds) <= toleranceSec) return false;
    // One flag, not a count: a seek started while another is in flight
    // aborts it, and the browser fires a single `seeked` for the last.
    if (v.readyState >= HAVE_METADATA) flagsFor(v).seek = true;
    v.currentTime = seconds;
    return true;
}

/** `v.play()`, marked as the app's own when it actually changes state.
 *  Returns the browser's promise so the caller can report a rejection. */
export function playProgrammatically(v: HTMLVideoElement): Promise<void> | undefined {
    const f = flagsFor(v);
    if (v.paused) f.play = true;
    const res = v.play();
    if (res && typeof (res as Promise<void>).then === "function") {
        // A refused play() fires no event, so the flag would go stale and
        // swallow the user's next real press.
        (res as Promise<void>).catch(() => {
            f.play = false;
        });
        return res as Promise<void>;
    }
    return undefined;
}

/** `v.pause()`, marked as the app's own when it actually changes state. */
export function pauseProgrammatically(v: HTMLVideoElement): void {
    if (!v.paused) flagsFor(v).pause = true;
    v.pause();
}

function positionMs(v: HTMLVideoElement): number {
    return Math.max(0, Math.round(v.currentTime * 1000));
}

/** Report the user's own seeks and play/pause presses on `v`. Returns the
 *  detach function. `onUserChange` gets the position the element is at when
 *  the event fires. */
export function attachNativeControlBroadcast(
    v: HTMLVideoElement,
    onUserChange: (kind: NativeControlKind, mediaPositionMs: number) => void,
): () => void {
    const onSeeked = (): void => {
        const f = flagsFor(v);
        if (f.seek) {
            f.seek = false;
            return;
        }
        onUserChange("seek", positionMs(v));
    };
    const onPlay = (): void => {
        const f = flagsFor(v);
        f.pause = false; // a play event makes any pending pause flag stale
        if (f.play) {
            f.play = false;
            return;
        }
        onUserChange("play", positionMs(v));
    };
    const onPause = (): void => {
        const f = flagsFor(v);
        f.play = false;
        if (f.pause) {
            f.pause = false;
            return;
        }
        onUserChange("pause", positionMs(v));
    };
    v.addEventListener("seeked", onSeeked);
    v.addEventListener("play", onPlay);
    v.addEventListener("pause", onPause);
    return () => {
        v.removeEventListener("seeked", onSeeked);
        v.removeEventListener("play", onPlay);
        v.removeEventListener("pause", onPause);
    };
}
