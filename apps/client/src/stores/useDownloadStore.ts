import { create } from "zustand";
import type { DownloadProgressEvent, DownloadStateEvent, DownloadState } from "../services/downloads";

export type { DownloadProgressEvent, DownloadStateEvent };

const ACTIVE_STATES: ReadonlySet<DownloadState> = new Set([
    "pending", "connecting", "transferring", "verifying", "failed", "paused",
]);

interface ActiveDownload {
    id: string;
    mediaId: string;
    state: DownloadState;
    errorMessage: string | null;
    progress: DownloadProgressEvent | undefined;
}

interface DownloadStoreState {
    downloads: Record<string, DownloadProgressEvent>;
    states: Record<string, DownloadStateEvent>;
    setProgress: (e: DownloadProgressEvent) => void;
    setState: (e: DownloadStateEvent) => void;
    clear: () => void;
    hasActiveDownload: () => boolean;
    activeDownloads: () => ActiveDownload[];
}

export const useDownloadStore = create<DownloadStoreState>((set, get) => ({
    downloads: {},
    states: {},
    setProgress: (e) => set((prev) => ({ downloads: { ...prev.downloads, [e.id]: e } })),
    setState: (e) => set((prev) => ({ states: { ...prev.states, [e.id]: e } })),
    clear: () => set({ downloads: {}, states: {} }),
    hasActiveDownload: () =>
        Object.values(get().states).some((s) => ACTIVE_STATES.has(s.state)),
    activeDownloads: () => {
        const { states, downloads } = get();
        return Object.values(states)
            .filter((s) => ACTIVE_STATES.has(s.state))
            .map((s): ActiveDownload => ({
                id: s.id,
                mediaId: s.media_id,
                state: s.state,
                errorMessage: s.error_message ?? null,
                progress: downloads[s.id],
            }))
            .sort((a, b) => {
                const at = a.progress?.bytes_per_sec_ema ?? 0;
                const bt = b.progress?.bytes_per_sec_ema ?? 0;
                if (bt !== at) return bt - at;
                return a.id < b.id ? -1 : a.id > b.id ? 1 : 0;
            });
    },
}));
