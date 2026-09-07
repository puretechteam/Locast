import { create } from "zustand";

export type ConnectionQuality = "good" | "fair" | "poor";

export interface ConnectionQualityState {
    quality: ConnectionQuality;
    rttMs: number | null;
    lastUpdatedMs: number | null;
    setQuality: (quality: ConnectionQuality, rttMs: number) => void;
    clear: () => void;
}

export const useConnectionQualityStore = create<ConnectionQualityState>((set) => ({
    quality: "good",
    rttMs: null,
    lastUpdatedMs: null,
    setQuality: (quality, rttMs) => {
        set({ quality, rttMs, lastUpdatedMs: Date.now() });
    },
    clear: () => {
        set({ quality: "good", rttMs: null, lastUpdatedMs: null });
    },
}));

const RTT_GOOD_MS = 100;
const RTT_FAIR_MS = 300;

export function classifyQuality(rttMs: number): ConnectionQuality {
    if (rttMs < RTT_GOOD_MS) return "good";
    if (rttMs < RTT_FAIR_MS) return "fair";
    return "poor";
}
