import { create } from "zustand";
import type { DownloadProgressEvent, DownloadStateEvent } from "../services/downloads";

export type { DownloadProgressEvent, DownloadStateEvent };

interface DownloadStoreState {
    downloads: Record<string, DownloadProgressEvent>;
    states: Record<string, DownloadStateEvent>;
    setProgress: (e: DownloadProgressEvent) => void;
    setState: (e: DownloadStateEvent) => void;
    clear: () => void;
}

export const useDownloadStore = create<DownloadStoreState>((set) => ({
    downloads: {},
    states: {},
    setProgress: (e) =>
        set((prev) => ({ downloads: { ...prev.downloads, [e.id]: e } })),
    setState: (e) =>
        set((prev) => ({ states: { ...prev.states, [e.id]: e } })),
    clear: () => set({ downloads: {}, states: {} }),
}));