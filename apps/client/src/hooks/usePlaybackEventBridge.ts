import { useEffect } from "react";
import { onPlaybackState } from "../services/playback";
import { usePlaybackStore } from "../stores/usePlaybackStore";
import { useRoomStore } from "../stores/useRoomStore";

/**
 * P4-T02: bridge the `playback://state` Tauri event
 * into the Zustand `usePlaybackStore`.
 *
 * Also mirrors the current room id from `useRoomStore`
 * into the playback store so `acceptEvent` can filter
 * cross-room events. The store's `setRoomId` is
 * idempotent (no-op when the value is unchanged).
 */
export function usePlaybackEventBridge(): null {
    // Mirror the current room id into the playback
    // store. We do this in render (not in the
    // playback event handler) so the room id is
    // available the moment the bridge subscribes.
    const roomId = useRoomStore((s) => s.summary?.id ?? null);
    useEffect(() => {
        usePlaybackStore.getState().setRoomId(roomId);
    }, [roomId]);

    useEffect(() => {
        let cancelled = false;
        const unsubs: Array<() => void> = [];
        (async () => {
            try {
                const u = await onPlaybackState((e) => {
                    if (cancelled) return;
                    usePlaybackStore.getState().acceptEvent(e);
                });
                if (cancelled) { u(); return; }
                unsubs.push(u);
            } catch (err) {
                // Surface the failure in dev; in prod
                // the room is unusable without playback
                // and a subsequent re-mount will retry.
                console.warn("usePlaybackEventBridge: listen failed", err);
            }
        })();
        return () => {
            cancelled = true;
            for (const u of unsubs) {
                try { u(); } catch { /* swallow */ }
            }
        };
    }, []);

    // P4-T02 test seam: in Vite's test mode, expose the
    // playback store mutators + a getter for `lastApplied`
    // on `window.__locastStore` so the Playwright harness
    // can drive `mediaSrc` and `mediaReady` without a
    // real Tauri runtime, AND can assert the
    // server-authoritative `lastApplied` value directly
    // (DOM `<video>` mutations like `currentTime` and
    // `play()` are not reliable in a Vite-only harness
    // because the media element cannot load arbitrary
    // test URLs; the store is the authoritative record
    // of what was applied). The production build
    // (`pnpm build`) does not set this because
    // `import.meta.env.MODE === "test"` is only true
    // under `pnpm dev:test`.
    useEffect(() => {
        if (import.meta.env.MODE !== "test") return;
        const w = window as unknown as {
            __locastStore?: {
                setMediaSrc: (s: string) => void;
                setMediaReady: (r: boolean) => void;
                getLastApplied: () => unknown;
            };
        };
        w.__locastStore = {
            setMediaSrc: (s) => usePlaybackStore.getState().setMediaSrc(s),
            setMediaReady: (r) => usePlaybackStore.getState().setMediaReady(r),
            getLastApplied: () => usePlaybackStore.getState().lastApplied,
        };
        return () => {
            if (w.__locastStore) delete w.__locastStore;
        };
    }, []);

    return null;
}
