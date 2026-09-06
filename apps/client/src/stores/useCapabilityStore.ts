import { create } from "zustand";

export const CAP = {
    PLAYBACK_CONTROL: 0x01,
    DRAW: 0x02,
    LASER: 0x04,
    MANAGE_ROOM: 0x08,
    KICK: 0x10,
    PUBLISH_MANIFEST: 0x20,
    INVITE: 0x40,
    CHAT: 0x80,
} as const;

export type Cap = (typeof CAP)[keyof typeof CAP];

interface CapabilityState {
    youCapSet: number | null;
    setYouCapSet: (capSet: number) => void;
    hasCap: (cap: number) => boolean;
}

export const useCapabilityStore = create<CapabilityState>((set, get) => ({
    youCapSet: null,

    setYouCapSet: (capSet) => set({ youCapSet: capSet }),

    hasCap: (cap) => {
        const youCapSet = get().youCapSet;
        if (youCapSet === null) return false;
        return (youCapSet & cap) !== 0;
    },
}));
