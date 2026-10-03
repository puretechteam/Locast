import { create } from "zustand";
import type { SharedManifestIpc } from "../services/sharedMedia";

/**
 * Where one shared (manifest) item stands on this client.
 *
 * - `checking`: `download_open` is in flight (dedup runs first in Rust).
 * - `waiting`: no source peer has an open DataChannel yet; the bridge
 *   retries `download_open` on the same download row.
 * - `downloading`: a real transfer is running; progress comes from the
 *   `download://` events and the blocking modal.
 * - `local`: the item is in the library (`dedup` = it already was, no
 *   transfer happened). `localMediaId` is the library id to play.
 */
export type SharedItemStatus =
    | { kind: "checking" }
    | { kind: "waiting"; downloadId: string; localMediaId: string }
    | { kind: "downloading"; downloadId: string; localMediaId: string }
    | { kind: "local"; localMediaId: string; dedup: boolean }
    | { kind: "error"; message: string };

interface SharedMediaState {
    roomId: string | null;
    /** Display only; the server enforces host-only publishing. */
    isHost: boolean;
    /** The manifest Rust accepted (signature + trust anchor checked). */
    manifest: SharedManifestIpc | null;
    /** Why the late-join manifest fetch failed, if it did. */
    manifestError: string | null;
    /** Keyed by the manifest's media id. */
    items: Record<string, SharedItemStatus>;
    reset: (roomId: string | null, isHost: boolean) => void;
    setIsHost: (isHost: boolean) => void;
    setManifest: (m: SharedManifestIpc | null) => void;
    setManifestError: (message: string | null) => void;
    setItem: (mediaId: string, status: SharedItemStatus) => void;
}

export const useSharedMediaStore = create<SharedMediaState>((set) => ({
    roomId: null,
    isHost: false,
    manifest: null,
    manifestError: null,
    items: {},
    reset: (roomId, isHost) =>
        set({ roomId, isHost, manifest: null, manifestError: null, items: {} }),
    setIsHost: (isHost) => set({ isHost }),
    setManifest: (manifest) =>
        set((prev) => ({
            manifest,
            manifestError: manifest !== null ? null : prev.manifestError,
        })),
    setManifestError: (manifestError) => set({ manifestError }),
    setItem: (mediaId, status) =>
        set((prev) => ({ items: { ...prev.items, [mediaId]: status } })),
}));

// Test seam, same pattern as the other stores: only in `--mode test`.
if (import.meta.env.MODE === "test") {
    (window as unknown as { __locastSharedMediaStore?: typeof useSharedMediaStore })
        .__locastSharedMediaStore = useSharedMediaStore;
}
