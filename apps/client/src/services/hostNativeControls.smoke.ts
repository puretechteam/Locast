// Smoke test for the host's native-control broadcast: a user's seek or
// play/pause press on the <video> is reported, and the app's own moves of the
// element (a command applied before it is sent, the Sync button) are not.
//
// Run via `pnpm test` (every *.smoke.ts under src/ is discovered).

import {
    attachNativeControlBroadcast,
    pauseProgrammatically,
    playProgrammatically,
    seekProgrammatically,
    type NativeControlKind,
} from "./hostNativeControls.ts";

let failures = 0;

function check(name: string, cond: boolean): void {
    if (cond) {
        process.stdout.write(`  ok ${name}\n`);
    } else {
        process.stdout.write(`  FAIL ${name}\n`);
        failures++;
    }
}

/** The slice of HTMLVideoElement the module touches. Like the browser, the
 *  events are delivered later (`flush`), not inside the call that causes
 *  them, and a rejected play() fires nothing. */
class FakeVideo extends EventTarget {
    currentTime = 0;
    paused = true;
    readyState = 4;
    rejectPlay = false;
    queued: string[] = [];

    set seekTo(sec: number) {
        this.currentTime = sec;
        this.queued.push("seeked");
    }
    play(): Promise<void> {
        if (this.rejectPlay) return Promise.reject(new Error("autoplay blocked"));
        if (this.paused) {
            this.paused = false;
            this.queued.push("play");
        }
        return Promise.resolve();
    }
    pause(): void {
        if (!this.paused) {
            this.paused = true;
            this.queued.push("pause");
        }
    }
    /** What a user does on the native controls. */
    userSeek(sec: number): void {
        this.seekTo = sec;
    }
    flush(): void {
        const q = this.queued;
        this.queued = [];
        for (const name of q) this.dispatchEvent(new Event(name));
    }
}

// The module writes `v.currentTime = ...`; route that through the fake so the
// `seeked` event is queued like a real seek.
function asVideo(f: FakeVideo): HTMLVideoElement {
    return new Proxy(f, {
        set(target, prop, value) {
            if (prop === "currentTime") {
                target.seekTo = value as number;
                return true;
            }
            return Reflect.set(target, prop, value);
        },
        get(target, prop) {
            const v = Reflect.get(target, prop);
            return typeof v === "function" ? v.bind(target) : v;
        },
    }) as unknown as HTMLVideoElement;
}

function rig(): {
    fake: FakeVideo;
    video: HTMLVideoElement;
    seen: Array<[NativeControlKind, number]>;
    detach: () => void;
} {
    const fake = new FakeVideo();
    const video = asVideo(fake);
    const seen: Array<[NativeControlKind, number]> = [];
    const detach = attachNativeControlBroadcast(video, (k, ms) => seen.push([k, ms]));
    return { fake, video, seen, detach };
}

async function main(): Promise<void> {
    process.stdout.write("user gestures are reported\n");
    {
        const { fake, seen } = rig();
        fake.userSeek(5400.25);
        fake.flush();
        check("a scrub reports seek at the new position", seen.length === 1 && seen[0]![0] === "seek" && seen[0]![1] === 5_400_250);
        fake.paused = false;
        fake.currentTime = 10;
        fake.queued.length = 0;
        fake.pause();
        fake.flush();
        check("a native pause reports pause", seen[1]?.[0] === "pause");
        fake.play();
        fake.flush();
        check("a native play reports play", seen[2]?.[0] === "play");
        check("nothing else was reported", seen.length === 3);
    }

    process.stdout.write("the app's own moves are not echoed\n");
    {
        const { fake, video, seen } = rig();
        check("seekProgrammatically moves the element", seekProgrammatically(video, 7200) === true);
        fake.flush();
        check("its seeked event is swallowed", seen.length === 0);
        fake.userSeek(100);
        fake.flush();
        check("the next user seek is reported again", seen.length === 1);

        seen.length = 0;
        await playProgrammatically(video);
        fake.flush();
        check("programmatic play is swallowed", seen.length === 0);
        pauseProgrammatically(video);
        fake.flush();
        check("programmatic pause is swallowed", seen.length === 0);
        fake.pause();
        fake.play();
        fake.flush();
        check("a user play afterwards is reported", seen.length === 1 && seen[0]![0] === "play");
    }

    process.stdout.write("no-op and failed programmatic calls leave no stale flag\n");
    {
        const { fake, video, seen } = rig();
        fake.currentTime = 50;
        check("seeking to the same spot does nothing", seekProgrammatically(video, 50.005) === false);
        fake.userSeek(60);
        fake.flush();
        check("so a user seek right after is reported", seen.length === 1);

        seen.length = 0;
        fake.paused = false;
        await playProgrammatically(video);
        fake.flush();
        pauseProgrammatically(video);
        fake.flush();
        check("pausing a playing element is swallowed once", seen.length === 0);
        pauseProgrammatically(video);
        fake.play();
        fake.pause();
        fake.flush();
        check("a pause on an already paused element set no flag", seen.some((s) => s[0] === "play"));

        const r = rig();
        r.fake.rejectPlay = true;
        await (playProgrammatically(r.video) ?? Promise.resolve()).catch(() => undefined);
        await Promise.resolve();
        r.fake.rejectPlay = false;
        r.fake.play();
        r.fake.flush();
        check("a refused play() does not swallow the user's next press", r.seen.length === 1 && r.seen[0]![0] === "play");

        const loading = rig();
        loading.fake.readyState = 0;
        seekProgrammatically(loading.video, 30);
        loading.fake.queued.length = 0; // no seeked fires before metadata loads
        loading.fake.readyState = 4;
        loading.fake.userSeek(40);
        loading.fake.flush();
        check("a seek before metadata sets no flag", loading.seen.length === 1);
    }

    process.stdout.write("detach stops reporting\n");
    {
        const { fake, seen, detach } = rig();
        detach();
        fake.userSeek(1);
        fake.play();
        fake.flush();
        check("no events after detach", seen.length === 0);
    }
}

await main();

if (failures > 0) {
    process.stdout.write(`\n${failures} failure(s)\n`);
    process.exit(1);
} else {
    process.stdout.write("\nall checks passed\n");
}
